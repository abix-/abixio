use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;

use abixio::admin::HealStats;
use abixio::admin::handlers::{AdminConfig, AdminHandler};
use abixio::config::Config;
use abixio::heal::mrf::MrfQueue;
use abixio::heal::scanner::ScanState;
use abixio::heal::worker::{mrf_drain_worker, scanner_loop};
use abixio::s3::auth::AuthConfig;
use abixio::s3::handlers::S3Handler;
use abixio::s3::router;
use abixio::storage::disk::DiskStorage;
use abixio::storage::erasure_set::ErasureSet;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cfg = Config::parse();
    if let Err(e) = cfg.validate() {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }

    let disk_paths: Vec<&std::path::Path> = cfg.disks.iter().map(|p| p.as_path()).collect();
    let mut set = match ErasureSet::new(&disk_paths, cfg.data, cfg.parity) {
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

    // build disk list for heal workers (separate from ErasureSet's disks)
    let heal_disks: Arc<Vec<DiskStorage>> = Arc::new(
        disk_paths
            .iter()
            .filter_map(|p| DiskStorage::new(p).ok())
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

    let admin_config = AdminConfig::from_config(&cfg);
    let admin = Arc::new(AdminHandler::new(
        Arc::clone(&set),
        Arc::clone(&heal_disks),
        Arc::clone(&mrf),
        Arc::clone(&heal_stats),
        admin_config,
    ));

    let mut handler = S3Handler::new(set, auth);
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
