use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use abixio::s3::auth::AuthConfig;
use abixio::s3::handlers::S3Handler;
use abixio::storage::erasure_set::ErasureSet;
use tempfile::TempDir;

fn setup() -> (TempDir, Vec<std::path::PathBuf>) {
    let base = TempDir::new().unwrap();
    let mut paths = Vec::new();
    for i in 0..4 {
        let p = base.path().join(format!("d{}", i));
        std::fs::create_dir_all(&p).unwrap();
        paths.push(p);
    }
    (base, paths)
}

async fn start_server(paths: &[std::path::PathBuf]) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    let set = Arc::new(ErasureSet::new(&refs, 2, 2).unwrap());
    let auth = AuthConfig {
        access_key: String::new(),
        secret_key: String::new(),
        no_auth: true,
    };
    let handler = Arc::new(S3Handler::new(set, auth));

    // bind to random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let handler = handler.clone();
                    async move { Ok::<_, hyper::Error>(handler.dispatch(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    (addr, handle)
}

fn url(addr: &SocketAddr, path: &str) -> String {
    format!("http://{}{}", addr, path)
}

fn url_with_query(addr: &SocketAddr, path: &str, params: &[(&str, &str)]) -> reqwest::Url {
    let mut url = reqwest::Url::parse(&url(addr, path)).unwrap();
    url.query_pairs_mut().extend_pairs(params);
    url
}

#[tokio::test]
async fn create_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client.put(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn create_bucket_twice_returns_409() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    let resp = client.put(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn head_bucket_after_create() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    // before create
    let resp = client.head(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 404);

    // create
    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    // after create
    let resp = client.head(url(&addr, "/testbucket")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn list_buckets() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/alpha")).send().await.unwrap();
    client.put(url(&addr, "/beta")).send().await.unwrap();

    let resp = client.get(url(&addr, "/")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("alpha"));
    assert!(body.contains("beta"));
    assert!(body.contains("ListAllMyBucketsResult"));
}

#[tokio::test]
async fn put_get_object() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    // put
    let resp = client
        .put(url(&addr, "/testbucket/hello.txt"))
        .header("Content-Type", "text/plain")
        .body("hello world")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("etag"));

    // get
    let resp = client
        .get(url(&addr, "/testbucket/hello.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/plain");
    assert!(resp.headers().contains_key("etag"));
    assert!(resp.headers().contains_key("last-modified"));
    let body = resp.text().await.unwrap();
    assert_eq!(body, "hello world");
}

#[tokio::test]
async fn head_object() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/key"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .head(url(&addr, "/testbucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], "4");
    assert!(resp.headers().contains_key("etag"));
    // head should have no body
    let body = resp.text().await.unwrap();
    assert!(body.is_empty());
}

#[tokio::test]
async fn delete_object() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/key"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .delete(url(&addr, "/testbucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // subsequent get returns 404
    let resp = client
        .get(url(&addr, "/testbucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn list_objects() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/aaa"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/bbb"))
        .body("2")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/testbucket?list-type=2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ListBucketResult"));
    assert!(body.contains("aaa"));
    assert!(body.contains("bbb"));
}

#[tokio::test]
async fn list_objects_with_prefix() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/logs/a"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/logs/b"))
        .body("2")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/data/c"))
        .body("3")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/testbucket?list-type=2&prefix=logs/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("logs/a"));
    assert!(body.contains("logs/b"));
    assert!(!body.contains("data/c"));
}

#[tokio::test]
async fn list_objects_with_delimiter() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/a/1"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/b/2"))
        .body("2")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/root"))
        .body("3")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/testbucket?list-type=2&delimiter=/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("CommonPrefixes"));
    assert!(body.contains("root"));
}

#[tokio::test]
async fn list_objects_with_encoded_delimiter_and_prefix() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/docs/readme.txt"))
        .body("readme")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/docs/guide.txt"))
        .body("guide")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/photos/cat.jpg"))
        .body("cat")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url_with_query(
            &addr,
            "/testbucket",
            &[("list-type", "2"), ("delimiter", "/")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Prefix>docs/</Prefix>"));
    assert!(body.contains("<Prefix>photos/</Prefix>"));

    let resp = client
        .get(url_with_query(
            &addr,
            "/testbucket",
            &[("list-type", "2"), ("prefix", "docs/")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("docs/readme.txt"));
    assert!(body.contains("docs/guide.txt"));
    assert!(!body.contains("photos/cat.jpg"));
}

#[tokio::test]
async fn get_nonexistent_bucket_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/nonexistent/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn put_empty_body() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/testbucket/empty"))
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/testbucket/empty"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], "0");
}

#[tokio::test]
async fn delete_nonexistent_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    let resp = client
        .delete(url(&addr, "/testbucket/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
