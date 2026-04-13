use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use tokio_rustls::TlsAcceptor;

use abixio::admin::HealStats;
use abixio::admin::handlers::{AdminConfig, AdminHandler};
use abixio::cluster::identity::resolve_identity;
use abixio::cluster::{ClusterConfig, ClusterManager};
use abixio::config::Config;
use abixio::heal::mrf::MrfQueue;
use abixio::heal::scanner::ScanState;
use abixio::heal::worker::{mrf_drain_worker, scanner_loop};
use abixio::s3_route::AbixioDispatch;
use abixio::storage::Backend;
use abixio::storage::local_volume::LocalVolume;
use abixio::storage::remote_volume::RemoteVolume;
use abixio::storage::storage_server::StorageServer;
use abixio::storage::volume_pool::VolumePool;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // rustls 0.23 requires an explicit crypto provider when both aws-lc-rs
    // and ring are available. Pick ring as the process default so the TLS
    // listener (and any rustls client in this process) can build a ServerConfig.
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls ring crypto provider");

    let mut cfg = Config::parse();
    if let Err(e) = cfg.expand_and_validate() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    let volume_paths = cfg.volume_paths();

    // resolve node identity from volume.json or peer exchange
    let identity = resolve_identity(&volume_paths, &cfg.listen, &cfg.nodes)
        .await
        .unwrap_or_else(|err| {
            eprintln!("error: {}", err);
            std::process::exit(1);
        });

    tracing::info!(
        node_id = %identity.node_id,
        cluster_id = %identity.cluster_id,
        volumes = identity.all_members.len(),
        "identity resolved"
    );

    // build mixed local + remote backends from identity
    let access_key = std::env::var("ABIXIO_ACCESS_KEY").unwrap_or_default();
    let secret_key = std::env::var("ABIXIO_SECRET_KEY").unwrap_or_default();

    if !cfg.no_auth && (access_key.is_empty() || secret_key.is_empty()) {
        eprintln!("error: ABIXIO_ACCESS_KEY and ABIXIO_SECRET_KEY must be set (or use --no-auth)");
        std::process::exit(1);
    }

    let mut backends: Vec<Box<dyn Backend>> = Vec::new();
    for nv in &identity.node_volumes {
        if nv.node_id == identity.node_id {
            // local volumes
            for vp in &nv.volume_paths {
                match LocalVolume::new(std::path::Path::new(vp)) {
                    Ok(mut v) => {
                        // apply --write-tier (default "file" = no extra wiring)
                        match cfg.write_tier.as_str() {
                            "wal" => {
                                if let Err(e) = v.enable_wal().await {
                                    eprintln!("error: enable WAL on {}: {}", vp, e);
                                    std::process::exit(1);
                                }
                            }
                            "log" => {
                                if let Err(e) = v.enable_log_store() {
                                    eprintln!("error: enable log store on {}: {}", vp, e);
                                    std::process::exit(1);
                                }
                            }
                            "pool" => {
                                if let Err(e) = v.enable_write_pool(1024).await {
                                    eprintln!("error: enable write pool on {}: {}", vp, e);
                                    std::process::exit(1);
                                }
                            }
                            _ => {}
                        }
                        backends.push(Box::new(v));
                    }
                    Err(e) => {
                        eprintln!("error: local volume {}: {}", vp, e);
                        std::process::exit(1);
                    }
                }
            }
        } else {
            // remote volumes
            for vp in &nv.volume_paths {
                let rv = match RemoteVolume::new(
                    nv.endpoint.clone(),
                    vp.clone(),
                    access_key.clone(),
                    secret_key.clone(),
                    cfg.no_auth,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("fatal: {}", e);
                        std::process::exit(1);
                    }
                };
                backends.push(Box::new(rv));
            }
        }
    }

    if backends.is_empty() {
        eprintln!("error: no backends available");
        std::process::exit(1);
    }

    let total_backends = backends.len();
    let local_count = identity
        .node_volumes
        .iter()
        .filter(|nv| nv.node_id == identity.node_id)
        .map(|nv| nv.volume_paths.len())
        .sum::<usize>();
    let remote_count = total_backends - local_count;
    tracing::info!(
        local = local_count,
        remote = remote_count,
        total = total_backends,
        "backends ready"
    );

    let default_ftt = abixio::storage::volume_pool::default_ftt(backends.len());
    tracing::info!(
        ftt = default_ftt,
        disks = backends.len(),
        "default bucket FTT"
    );

    let mut set = match VolumePool::new(backends) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    if cfg.write_cache > 0 {
        set.enable_write_cache(cfg.write_cache * 1024 * 1024);
    }

    // set up healing infrastructure
    let mrf = Arc::new(MrfQueue::new(1000));
    let scan_state = Arc::new(ScanState::new(cfg.heal_interval_duration()));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // wire MRF into volume pool for auto-enqueue on partial writes
    set.set_mrf(Arc::clone(&mrf));
    let set = Arc::new(set);

    // build disk list for heal workers (separate from VolumePool's disks)
    let mut heal_backends: Vec<Box<dyn Backend>> = volume_paths
        .iter()
        .filter_map(|p| LocalVolume::new(p.as_path()).ok())
        .map(|d| Box::new(d) as Box<dyn Backend>)
        .collect();
    abixio::storage::volume_pool::assign_volume_ids(&mut heal_backends);
    let heal_disks: Arc<Vec<Box<dyn Backend>>> = Arc::new(heal_backends);

    // spawn MRF drain worker (single receiver = single worker)
    if let Some(rx) = mrf.take_receiver() {
        tokio::spawn(mrf_drain_worker(
            Arc::clone(&heal_disks),
            Arc::clone(&mrf),
            rx,
            shutdown_rx.clone(),
        ));
    }

    // spawn background scanner
    tokio::spawn(scanner_loop(
        Arc::clone(&heal_disks),
        Arc::clone(&mrf),
        scan_state,
        cfg.scan_interval_duration(),
        shutdown_rx.clone(),
    ));

    let heal_stats = Arc::new(HealStats::new());
    let cluster = Arc::new(
        ClusterManager::new(ClusterConfig {
            node_id: identity.node_id.clone(),
            advertise_s3: identity.advertise.clone(),
            advertise_cluster: identity.advertise.clone(),
            nodes: identity.nodes.clone(),
            access_key: access_key.clone(),
            secret_key: secret_key.clone(),
            no_auth: cfg.no_auth,
            disk_paths: volume_paths.clone(),
        })
        .unwrap_or_else(|err| {
            eprintln!("error: {}", err);
            std::process::exit(1);
        }),
    );
    cluster.clone().spawn_peer_monitor(shutdown_rx.clone());

    let admin_config = AdminConfig::from_identity(&identity, &cfg);
    let admin = Arc::new(AdminHandler::new(
        Arc::clone(&set),
        Arc::clone(&heal_disks),
        Arc::clone(&mrf),
        Arc::clone(&heal_stats),
        admin_config,
        Arc::clone(&cluster),
    ));

    // build storage server for internode RPC
    let local_volumes_map: std::collections::HashMap<String, LocalVolume> = volume_paths
        .iter()
        .filter_map(|p| {
            LocalVolume::new(p.as_path())
                .ok()
                .map(|v| (p.display().to_string(), v))
        })
        .collect();
    let storage_server = Arc::new(StorageServer::new(
        local_volumes_map,
        access_key.clone(),
        secret_key.clone(),
        cfg.no_auth,
    ));

    // build s3s service
    let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&set), Arc::clone(&cluster));
    let mut builder = s3s::service::S3ServiceBuilder::new(s3);
    if !cfg.no_auth {
        builder.set_auth(abixio::s3_auth::AbixioAuth::new(&access_key, &secret_key));
    }
    builder.set_access(abixio::s3_access::AbixioAccess::new(Arc::clone(&cluster)));
    builder.set_validation(abixio::s3_service::RelaxedNameValidation);
    let s3_service = builder.build();

    // wrap with dispatch layer for admin + storage RPC
    let dispatch = Arc::new(abixio::s3_route::AbixioDispatch::new(
        s3_service,
        Some(admin),
        Some(storage_server),
    ));

    let addr = parse_listen_addr(&cfg.listen);

    // run server with graceful shutdown
    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let server_token = shutdown_token.clone();

    let server_handle = tokio::spawn(serve(
        dispatch,
        addr,
        cfg.tls_cert.clone(),
        cfg.tls_key.clone(),
        server_token,
    ));

    // wait for ctrl+c
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");

    // 1. signal heal workers and peer monitor to stop
    let _ = shutdown_tx.send(true);

    // 2. stop accepting new connections, let in-flight requests finish
    shutdown_token.cancel();
    let drain_timeout = tokio::time::Duration::from_secs(5);
    match tokio::time::timeout(drain_timeout, server_handle).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => tracing::warn!(error = %e, "server error during shutdown"),
        Ok(Err(e)) => tracing::warn!(error = %e, "server task panicked during shutdown"),
        Err(_) => tracing::warn!("in-flight requests did not finish within 5s, proceeding"),
    }

    // 3. flush write cache and drain pool renames
    set.shutdown().await;

    tracing::info!("shutdown complete");
}

async fn serve(
    dispatch: Arc<AbixioDispatch>,
    addr: SocketAddr,
    tls_cert: Option<String>,
    tls_key: Option<String>,
    shutdown: tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let tls_acceptor = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => Some(TlsAcceptor::from(load_tls_config(&cert, &key)?)),
        (None, None) => None,
        _ => unreachable!("config validation enforces cert/key pairing"),
    };
    tracing::info!(tls = tls_acceptor.is_some(), "abixio listening on {}", addr);

    // track in-flight connections so we can wait for them on shutdown
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            result = listener.accept() => {
                let (stream, _) = result?;
                stream.set_nodelay(true)?;
                let dispatch = dispatch.clone();

                match tls_acceptor {
                    Some(ref acceptor) => {
                        let tls_stream = match acceptor.accept(stream).await {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!("tls handshake error: {}", e);
                                continue;
                            }
                        };
                        let io = hyper_util::rt::TokioIo::new(tls_stream);
                        let service = hyper::service::service_fn(move |req| {
                            let dispatch = dispatch.clone();
                            async move { Ok::<_, hyper::Error>(dispatch.dispatch(req).await) }
                        });
                        let conn = hyper::server::conn::http1::Builder::new()
                            .writev(true)
                            .max_buf_size(4 * 1024 * 1024)
                            .serve_connection(io, service);
                        let watched = graceful.watch(conn);
                        tokio::spawn(async move {
                            if let Err(e) = watched.await {
                                tracing::error!("connection error: {}", e);
                            }
                        });
                    }
                    None => {
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let service = hyper::service::service_fn(move |req| {
                            let dispatch = dispatch.clone();
                            async move { Ok::<_, hyper::Error>(dispatch.dispatch(req).await) }
                        });
                        let conn = hyper::server::conn::http1::Builder::new()
                            .writev(true)
                            .max_buf_size(4 * 1024 * 1024)
                            .serve_connection(io, service);
                        let watched = graceful.watch(conn);
                        tokio::spawn(async move {
                            if let Err(e) = watched.await {
                                tracing::error!("connection error: {}", e);
                            }
                        });
                    }
                };
            }
        }
    }

    // stop accepting, wait for in-flight connections to finish
    tracing::info!("waiting for in-flight requests to complete");
    graceful.shutdown().await;
    Ok(())
}

fn load_tls_config(
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<Arc<tokio_rustls::rustls::ServerConfig>> {
    use std::fs::File;
    use std::io::BufReader;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let mut cert_reader = BufReader::new(File::open(cert_path)?);
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        !certs.is_empty(),
        "no TLS certificates found in {}",
        cert_path
    );

    let mut key_reader = BufReader::new(File::open(key_path)?);
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| anyhow::anyhow!("no TLS private key found in {}", key_path))?;

    Ok(Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?,
    ))
}

fn parse_listen_addr(s: &str) -> SocketAddr {
    let s = if s.starts_with(':') {
        format!("0.0.0.0{}", s)
    } else {
        s.to_string()
    };
    s.parse().unwrap_or_else(|_| {
        eprintln!("invalid listen address: {}", s);
        std::process::exit(1);
    })
}
