use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;

use abixio::config::Config;
use abixio::s3::auth::AuthConfig;
use abixio::s3::handlers::S3Handler;
use abixio::s3::router;
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
    let set = match ErasureSet::new(&disk_paths, cfg.data, cfg.parity) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };

    let auth = AuthConfig {
        access_key: std::env::var("ABIXIO_ACCESS_KEY").unwrap_or_default(),
        secret_key: std::env::var("ABIXIO_SECRET_KEY").unwrap_or_default(),
        no_auth: cfg.no_auth,
    };

    let handler = Arc::new(S3Handler::new(set, auth));
    let addr = parse_listen_addr(&cfg.listen);

    if let Err(e) = router::serve(handler, addr).await {
        eprintln!("error: {}", e);
        std::process::exit(1);
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
