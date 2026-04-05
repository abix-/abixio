use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;

use abixio::admin::HealStats;
use abixio::admin::handlers::{AdminConfig, AdminHandler};
use abixio::cluster::{ClusterConfig, ClusterManager};
use abixio::cluster::topology::StaticTopology;
use abixio::config::Config;
use abixio::heal::mrf::MrfQueue;
use abixio::heal::scanner::ScanState;
use abixio::heal::worker::{mrf_drain_worker, scanner_loop};
use abixio::s3::auth::AuthConfig;
use abixio::s3::handlers::S3Handler;
use abixio::s3::router;
use abixio::storage::Backend;
use abixio::storage::disk::LocalDisk;
use abixio::storage::erasure_set::ErasureSet;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();
    if let Err(e) = cfg.validate() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    let static_topology = cfg
        .cluster_topology
        .as_ref()
        .map(|path| {
            StaticTopology::load(path).unwrap_or_else(|err| {
                eprintln!("error: {}", err);
                std::process::exit(1);
            })
        });

    let backends: Vec<Box<dyn Backend>> = match cfg
        .disks
        .iter()
        .map(|p| LocalDisk::new(p.as_path()).map(|d| Box::new(d) as Box<dyn Backend>))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let mut set = match ErasureSet::new(backends, cfg.data, cfg.parity) {
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

    // wire MRF into erasure set for auto-enqueue on partial writes
    set.set_mrf(Arc::clone(&mrf));

    let set = Arc::new(set);
    if let Some(topology) = &static_topology {
        let placement_volumes = topology.placement_volumes();
        if placement_volumes.len() == set.disks().len() {
            if let Err(err) = set.set_placement_topology(
                topology.epoch_id,
                topology.set_id.clone(),
                placement_volumes,
            ) {
                eprintln!("error: {}", err);
                std::process::exit(1);
            }
        } else {
            tracing::warn!(
                "cluster topology defines {} disks but local store has {}; placement remains local until remote shard transport exists",
                placement_volumes.len(),
                set.disks().len()
            );
        }
    }

    // build disk list for heal workers (separate from ErasureSet's disks)
    let heal_disks: Arc<Vec<Box<dyn Backend>>> = Arc::new(
        cfg.disks
            .iter()
            .filter_map(|p| LocalDisk::new(p.as_path()).ok())
            .map(|d| Box::new(d) as Box<dyn Backend>)
            .collect(),
    );

    // spawn MRF drain worker (single receiver = single worker)
    if let Some(rx) = mrf.take_receiver() {
        tokio::spawn(mrf_drain_worker(
            Arc::clone(&heal_disks),
            cfg.data,
            cfg.parity,
            Arc::clone(&mrf),
            rx,
            shutdown_rx.clone(),
        ));
    }

    // spawn background scanner
    tokio::spawn(scanner_loop(
        Arc::clone(&heal_disks),
        cfg.data,
        cfg.parity,
        Arc::clone(&mrf),
        scan_state,
        cfg.scan_interval_duration(),
        shutdown_rx.clone(),
    ));

    let auth = AuthConfig {
        access_key: std::env::var("ABIXIO_ACCESS_KEY").unwrap_or_default(),
        secret_key: std::env::var("ABIXIO_SECRET_KEY").unwrap_or_default(),
        no_auth: cfg.no_auth,
    };

    let heal_stats = Arc::new(HealStats::new());
    let cluster = Arc::new(
        ClusterManager::new(ClusterConfig {
            node_id: cfg.node_id.clone(),
            advertise_s3: cfg.advertise_s3.clone(),
            advertise_cluster: cfg.advertise_cluster.clone(),
            peers: cfg.peers.clone(),
            cluster_secret: cfg.cluster_secret.clone(),
            disk_paths: cfg.disks.clone(),
            topology: static_topology,
        })
        .unwrap_or_else(|err| {
            eprintln!("error: {}", err);
            std::process::exit(1);
        }),
    );
    cluster.clone().spawn_peer_monitor(shutdown_rx.clone());

    let admin_config = AdminConfig::from_config(&cfg);
    let admin = Arc::new(AdminHandler::new(
        Arc::clone(&set),
        Arc::clone(&heal_disks),
        Arc::clone(&mrf),
        Arc::clone(&heal_stats),
        admin_config,
        Arc::clone(&cluster),
    ));

    let mut handler = S3Handler::new(set, auth, cluster);
    handler.set_admin(admin);
    let handler = Arc::new(handler);
    let addr = parse_listen_addr(&cfg.listen);

    // run server with graceful shutdown
    tokio::select! {
        result = router::serve(handler, addr) => {
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

fn parse_listen_addr(s: &str) -> SocketAddr {
    // handle ":9000" -> "0.0.0.0:9000"
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
