use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;

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
use abixio::storage::volume_pool::VolumePool;
use abixio::storage::storage_server::StorageServer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut cfg = Config::parse();
    if let Err(e) = cfg.expand_and_validate() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    let volume_paths = cfg.volume_paths();

    // resolve node identity from volume.json or peer exchange
    let identity = resolve_identity(
        &volume_paths,
        &cfg.listen,
        &cfg.nodes,
    )
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
                    Ok(v) => backends.push(Box::new(v)),
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
    let local_count = identity.node_volumes.iter()
        .filter(|nv| nv.node_id == identity.node_id)
        .map(|nv| nv.volume_paths.len())
        .sum::<usize>();
    let remote_count = total_backends - local_count;
    tracing::info!(local = local_count, remote = remote_count, total = total_backends, "backends ready");

    let default_ftt = abixio::storage::volume_pool::default_ftt(backends.len());
    tracing::info!(ftt = default_ftt, disks = backends.len(), "default bucket FTT");

    let mut set = match VolumePool::new(backends) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

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
    let s3_service = builder.build();

    // wrap with dispatch layer for admin + storage RPC
    let dispatch = Arc::new(abixio::s3_route::AbixioDispatch::new(
        s3_service,
        Some(admin),
        Some(storage_server),
    ));

    let addr = parse_listen_addr(&cfg.listen);

    // run server with graceful shutdown
    tokio::select! {
        result = serve(dispatch, addr) => {
            if let Err(e) = result {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            let _ = shutdown_tx.send(true);
        }
    }
}

async fn serve(dispatch: Arc<AbixioDispatch>, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("abixio listening on {}", addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = hyper_util::rt::TokioIo::new(stream);
        let dispatch = dispatch.clone();

        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let dispatch = dispatch.clone();
                async move { Ok::<_, hyper::Error>(dispatch.dispatch(req).await) }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, service)
                .await
            {
                tracing::error!("connection error: {}", e);
            }
        });
    }
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
