//! End-to-end test of the write-cache peer-replication protocol:
//! `PeerCacheClient` -> `StorageServer` -> `WriteCache`. Covers phases
//! 5-6 (replicate on PUT) and phase 8 (sync-pull on restart).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use abixio::storage::metadata::{ErasureMeta, ObjectMeta};
use abixio::storage::peer_cache::PeerCacheClient;
use abixio::storage::storage_server::StorageServer;
use abixio::storage::write_cache::{CacheEntry, WriteCache, unix_nanos_now};
use bytes::Bytes;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

fn test_meta() -> ObjectMeta {
    ObjectMeta {
        size: 4096,
        etag: "abc".to_string(),
        content_type: "text/plain".to_string(),
        created_at: 1700000000,
        erasure: ErasureMeta { ftt: 0, index: 0, epoch_id: 1, volume_ids: vec!["v1".to_string()] },
        checksum: "dead".to_string(),
        user_metadata: HashMap::new(),
        tags: HashMap::new(),
        version_id: String::new(),
        is_latest: true,
        is_delete_marker: false,
        parts: Vec::new(),
        inline_data: None,
    }
}

fn test_entry(primary: &str, payload: u8) -> CacheEntry {
    CacheEntry {
        shards: vec![Bytes::from(vec![payload; 1365])],
        metas: vec![test_meta()],
        distribution: vec![0],
        original_size: 4096,
        content_type: "text/plain".to_string(),
        etag: "abc".to_string(),
        created_at: 1700000000,
        user_metadata: HashMap::new(),
        tags: HashMap::new(),
        cached_at_unix_nanos: unix_nanos_now(),
        cached_at_local: Instant::now(),
        primary_node_id: primary.to_string(),
    }
}

async fn spawn_peer_storage_server(cache: Arc<WriteCache>) -> (String, tokio::task::JoinHandle<()>) {
    let server = Arc::new(
        StorageServer::new(HashMap::new(), String::new(), String::new(), true)
            .with_write_cache(cache),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            let io = TokioIo::new(stream);
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let server = Arc::clone(&server);
                    async move { Ok::<_, hyper::Error>(server.dispatch(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    (format!("http://{}", addr), handle)
}

#[tokio::test]
async fn replicate_inserts_into_peer_cache() {
    let peer_cache = Arc::new(WriteCache::new(1024 * 1024));
    let (endpoint, handle) = spawn_peer_storage_server(Arc::clone(&peer_cache)).await;

    let client = PeerCacheClient::new(endpoint, String::new(), String::new(), true).unwrap();
    let entry = test_entry("node-a", 0x11);
    client.replicate("mybucket", "mykey", &entry).await.expect("replicate ok");

    assert!(peer_cache.contains("mybucket", "mykey"));
    let got = peer_cache.get("mybucket", "mykey").unwrap();
    assert_eq!(got.primary_node_id, "node-a");
    assert_eq!(got.shards[0].len(), 1365);
    assert_eq!(got.shards[0][0], 0x11);
    handle.abort();
}

#[tokio::test]
async fn sync_pull_drains_only_entries_owned_by_caller() {
    let peer_cache = Arc::new(WriteCache::new(1024 * 1024));
    peer_cache.insert("b", "from-a", test_entry("node-a", 0xA0));
    peer_cache.insert("b", "from-c", test_entry("node-c", 0xC0));
    let (endpoint, handle) = spawn_peer_storage_server(Arc::clone(&peer_cache)).await;

    let client = PeerCacheClient::new(endpoint, String::new(), String::new(), true).unwrap();
    let mine = client.sync_pull("node-a").await.expect("sync ok");
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].1, "from-a");
    assert_eq!(mine[0].2.primary_node_id, "node-a");

    // peer still has the node-c entry; node-a entry was drained
    assert!(!peer_cache.contains("b", "from-a"));
    assert!(peer_cache.contains("b", "from-c"));
    handle.abort();
}

#[tokio::test]
async fn replicate_rejects_when_peer_cache_disabled() {
    // Storage server without write_cache attached -> 503 on /cache-replicate.
    let server = Arc::new(StorageServer::new(HashMap::new(), String::new(), String::new(), true));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let svc = service_fn(move |req: Request<Incoming>| {
            let server = Arc::clone(&server);
            async move { Ok::<_, hyper::Error>(server.dispatch(req).await) }
        });
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(io, svc)
            .await;
    });

    let client =
        PeerCacheClient::new(format!("http://{}", addr), String::new(), String::new(), true)
            .unwrap();
    let err = client
        .replicate("b", "k", &test_entry("node-a", 0x01))
        .await
        .expect_err("should fail");
    assert!(err.contains("503"), "expected 503 in error, got: {}", err);
    let _ = handle.await;
}
