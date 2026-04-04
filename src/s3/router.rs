use std::net::SocketAddr;
use std::sync::Arc;

use hyper::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use super::handlers::S3Handler;

pub async fn serve(handler: Arc<S3Handler>, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("abixio listening on {}", addr);

    loop {
        let (stream, _remote) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let handler = handler.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let handler = handler.clone();
                async move { Ok::<_, hyper::Error>(handler.dispatch(req).await) }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                tracing::error!("connection error: {}", e);
            }
        });
    }
}
