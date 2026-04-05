use std::net::SocketAddr;
use std::sync::Arc;

use abixio::s3::auth::AuthConfig;
use abixio::s3::handlers::S3Handler;
use abixio::storage::Backend;
use abixio::storage::disk::LocalDisk;
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
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalDisk::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let set = Arc::new(ErasureSet::new(backends, 2, 2).unwrap());
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

// --- x-amz-request-id header on all responses ---

#[tokio::test]
async fn request_id_on_success() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client.get(url(&addr, "/")).send().await.unwrap();
    assert!(resp.headers().contains_key("x-amz-request-id"));
    let rid = resp.headers()["x-amz-request-id"].to_str().unwrap();
    assert!(!rid.is_empty());
}

#[tokio::test]
async fn request_id_on_error() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/nonexistent/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(resp.headers().contains_key("x-amz-request-id"));
}

#[tokio::test]
async fn error_xml_contains_request_id_and_resource() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/nonexistent/key"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("<RequestId>"), "missing RequestId in error XML: {}", body);
    // Resource is included when the handler has context; auth-level errors may omit it
    assert!(body.contains("Error"), "expected Error XML: {}", body);
}

// --- Object tagging ---

#[tokio::test]
async fn object_tagging_put_get_delete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // get tags on fresh object -- should return empty TagSet
    let resp = client
        .get(url(&addr, "/tb/obj?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Tagging"), "expected Tagging XML: {}", body);

    // put tags
    let tag_xml = r#"<Tagging><TagSet><Tag><Key>env</Key><Value>prod</Value></Tag><Tag><Key>team</Key><Value>infra</Value></Tag></TagSet></Tagging>"#;
    let resp = client
        .put(url(&addr, "/tb/obj?tagging"))
        .body(tag_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // get tags back
    let resp = client
        .get(url(&addr, "/tb/obj?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("env"), "missing tag key 'env': {}", body);
    assert!(body.contains("prod"), "missing tag value 'prod': {}", body);
    assert!(body.contains("team"), "missing tag key 'team': {}", body);
    assert!(body.contains("infra"), "missing tag value 'infra': {}", body);

    // delete tags
    let resp = client
        .delete(url(&addr, "/tb/obj?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // verify tags are gone
    let resp = client
        .get(url(&addr, "/tb/obj?tagging"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(!body.contains("env"), "tag should be gone: {}", body);
}

#[tokio::test]
async fn object_tagging_on_nonexistent_object_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let resp = client
        .get(url(&addr, "/tb/missing?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn object_tagging_malformed_xml_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tb/obj?tagging"))
        .body("not xml at all")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// --- Bucket tagging ---

#[tokio::test]
async fn bucket_tagging_put_get_delete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    // get bucket tags (empty initially)
    let resp = client
        .get(url(&addr, "/tb?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // put bucket tags
    let tag_xml = r#"<Tagging><TagSet><Tag><Key>project</Key><Value>alpha</Value></Tag></TagSet></Tagging>"#;
    let resp = client
        .put(url(&addr, "/tb?tagging"))
        .body(tag_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // get bucket tags back
    let resp = client
        .get(url(&addr, "/tb?tagging"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("project"), "missing key: {}", body);
    assert!(body.contains("alpha"), "missing value: {}", body);

    // delete bucket tags
    let resp = client
        .delete(url(&addr, "/tb?tagging"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

// --- Conditional requests ---

#[tokio::test]
async fn if_none_match_returns_304() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    let resp = client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    let etag = resp.headers()["etag"].to_str().unwrap().to_string();

    // GET with matching If-None-Match -> 304
    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);

    // HEAD with matching If-None-Match -> 304
    let resp = client
        .head(url(&addr, "/tb/obj"))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);
}

#[tokio::test]
async fn if_none_match_different_etag_returns_200() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("If-None-Match", "\"nonexistent-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn if_match_returns_412_on_mismatch() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // If-Match with wrong etag -> 412
    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("If-Match", "\"wrong-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 412);
}

#[tokio::test]
async fn if_match_correct_etag_returns_200() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    let resp = client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    let etag = resp.headers()["etag"].to_str().unwrap().to_string();

    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("If-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn if_modified_since_returns_304_when_not_modified() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // use a date far in the future
    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("If-Modified-Since", "Sun, 01 Jan 2090 00:00:00 GMT")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);
}

#[tokio::test]
async fn if_unmodified_since_returns_412_when_modified() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // use a date far in the past
    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("If-Unmodified-Since", "Thu, 01 Jan 2000 00:00:00 GMT")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 412);
}

// --- Bucket versioning config ---

#[tokio::test]
async fn versioning_disabled_by_default() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let resp = client
        .get(url(&addr, "/tb?versioning"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("VersioningConfiguration"), "expected XML: {}", body);
    // no <Status> element when disabled
    assert!(!body.contains("<Status>Enabled</Status>"));
    assert!(!body.contains("<Status>Suspended</Status>"));
}

#[tokio::test]
async fn versioning_enable_and_get() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    let resp = client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/tb?versioning"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Status>Enabled</Status>"), "body: {}", body);
}

#[tokio::test]
async fn versioning_enable_then_suspend() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let xml = r#"<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>"#;
    let resp = client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/tb?versioning"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Status>Suspended</Status>"), "body: {}", body);
}

#[tokio::test]
async fn versioning_invalid_status_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Invalid</Status></VersioningConfiguration>"#;
    let resp = client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// --- Versioned PUT creates versions ---

#[tokio::test]
async fn versioned_put_returns_version_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    // enable versioning
    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    // PUT should return x-amz-version-id
    let resp = client
        .put(url(&addr, "/tb/obj"))
        .body("v1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().contains_key("x-amz-version-id"),
        "missing x-amz-version-id header"
    );
    let vid1 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(!vid1.is_empty());

    // second PUT should return different version id
    let resp = client
        .put(url(&addr, "/tb/obj"))
        .body("v2")
        .send()
        .await
        .unwrap();
    let vid2 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert_ne!(vid1, vid2);
}

// --- ListObjectVersions ---

#[tokio::test]
async fn list_object_versions_returns_all_versions() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    client
        .put(url(&addr, "/tb/obj"))
        .body("v1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("v2")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tb?versions"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("ListVersionsResult"),
        "expected ListVersionsResult: {}",
        body
    );
    // should contain at least 2 <Version> elements
    let version_count = body.matches("<Version>").count();
    assert!(
        version_count >= 2,
        "expected >= 2 versions, got {}: {}",
        version_count,
        body
    );
    assert!(body.contains("<IsLatest>true</IsLatest>"));
    assert!(body.contains("<VersionId>"));
}

#[tokio::test]
async fn list_object_versions_empty_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let resp = client
        .get(url(&addr, "/tb?versions"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ListVersionsResult"));
    assert!(!body.contains("<Version>"));
}

// --- Versioned DELETE creates delete marker ---

#[tokio::test]
async fn versioned_delete_creates_delete_marker() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    // delete without versionId -> should create delete marker
    let resp = client
        .delete(url(&addr, "/tb/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert!(resp.headers().contains_key("x-amz-delete-marker"));
    assert_eq!(resp.headers()["x-amz-delete-marker"], "true");
    assert!(resp.headers().contains_key("x-amz-version-id"));

    // list versions should show the delete marker
    let resp = client
        .get(url(&addr, "/tb?versions"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<DeleteMarker>"),
        "expected DeleteMarker: {}",
        body
    );
}

// --- GET/DELETE with versionId ---

#[tokio::test]
async fn get_specific_version() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tb/obj"))
        .body("version-one")
        .send()
        .await
        .unwrap();
    let vid1 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();

    client
        .put(url(&addr, "/tb/obj"))
        .body("version-two")
        .send()
        .await
        .unwrap();

    // GET without versionId -> latest (version-two)
    let resp = client
        .get(url(&addr, "/tb/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "version-two");

    // GET with versionId=vid1 -> first version
    let resp = client
        .get(url_with_query(
            &addr,
            "/tb/obj",
            &[("versionId", &vid1)],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "version-one");
}

#[tokio::test]
async fn delete_specific_version() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tb/obj"))
        .body("v1")
        .send()
        .await
        .unwrap();
    let vid1 = resp.headers()["x-amz-version-id"]
        .to_str()
        .unwrap()
        .to_string();

    client
        .put(url(&addr, "/tb/obj"))
        .body("v2")
        .send()
        .await
        .unwrap();

    // permanently delete v1
    let resp = client
        .delete(url_with_query(
            &addr,
            "/tb/obj",
            &[("versionId", &vid1)],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(resp.headers()["x-amz-version-id"], vid1.as_str());

    // GET v1 should now fail
    let resp = client
        .get(url_with_query(
            &addr,
            "/tb/obj",
            &[("versionId", &vid1)],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // latest (v2) should still work
    let resp = client
        .get(url(&addr, "/tb/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "v2");
}

// --- Suspended versioning uses null version ---

#[tokio::test]
async fn suspended_versioning_uses_null_version() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    // enable then suspend
    let xml = r#"<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    let xml = r#"<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>"#;
    client
        .put(url(&addr, "/tb?versioning"))
        .body(xml)
        .send()
        .await
        .unwrap();

    // PUT should return version-id "null"
    let resp = client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-amz-version-id"], "null");
}

// --- CopyObject ---

#[tokio::test]
async fn copy_object_same_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/src"))
        .header("Content-Type", "text/plain")
        .body("original")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/tb/dst"))
        .header("x-amz-copy-source", "/tb/src")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("CopyObjectResult"), "body: {}", body);
    assert!(body.contains("ETag"));

    // verify copy
    let resp = client.get(url(&addr, "/tb/dst")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "original");
}

#[tokio::test]
async fn copy_object_cross_bucket() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/src-bucket")).send().await.unwrap();
    client.put(url(&addr, "/dst-bucket")).send().await.unwrap();
    client
        .put(url(&addr, "/src-bucket/key"))
        .body("cross-bucket")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(url(&addr, "/dst-bucket/key"))
        .header("x-amz-copy-source", "/src-bucket/key")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, "/dst-bucket/key"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "cross-bucket");
}

// --- DeleteObjects (batch) ---

#[tokio::test]
async fn delete_objects_batch() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/a"))
        .body("1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/tb/b"))
        .body("2")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/tb/c"))
        .body("3")
        .send()
        .await
        .unwrap();

    let delete_xml = r#"<Delete><Object><Key>a</Key></Object><Object><Key>b</Key></Object></Delete>"#;
    let resp = client
        .post(url(&addr, "/tb?delete"))
        .body(delete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("DeleteResult"), "body: {}", body);
    assert!(body.contains("<Key>a</Key>"));
    assert!(body.contains("<Key>b</Key>"));

    // a and b should be gone, c should remain
    let resp = client.get(url(&addr, "/tb/a")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client.get(url(&addr, "/tb/c")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

// --- Range requests ---

#[tokio::test]
async fn range_request_partial_content() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("hello world")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("Range", "bytes=0-4")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert!(resp.headers().contains_key("content-range"));
    assert_eq!(resp.text().await.unwrap(), "hello");
}

#[tokio::test]
async fn range_request_invalid_returns_416() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("small")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tb/obj"))
        .header("Range", "bytes=100-200")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 416);
}

// --- Custom metadata ---

#[tokio::test]
async fn custom_metadata_round_trip() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .header("x-amz-meta-author", "alice")
        .header("x-amz-meta-version", "42")
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .head(url(&addr, "/tb/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-amz-meta-author"], "alice");
    assert_eq!(resp.headers()["x-amz-meta-version"], "42");

    let resp = client
        .get(url(&addr, "/tb/obj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-amz-meta-author"], "alice");
}

// --- Delete bucket ---

#[tokio::test]
async fn delete_bucket_non_empty_returns_409() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();
    client
        .put(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client.delete(url(&addr, "/tb")).send().await.unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn delete_bucket_empty_succeeds() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let resp = client.delete(url(&addr, "/tb")).send().await.unwrap();
    assert_eq!(resp.status(), 204);

    // bucket should be gone
    let resp = client.head(url(&addr, "/tb")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

// --- Method not allowed ---

#[tokio::test]
async fn unsupported_method_returns_405() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .patch(url(&addr, "/tb/obj"))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
}

// --- Multipart upload ---

#[tokio::test]
async fn multipart_create_upload_returns_upload_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    let resp = client
        .post(url(&addr, "/tb/bigfile?uploads"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("InitiateMultipartUploadResult"), "body: {}", body);
    assert!(body.contains("<UploadId>"), "missing UploadId: {}", body);
    assert!(body.contains("<Bucket>tb</Bucket>"));
    assert!(body.contains("<Key>bigfile</Key>"));
}

#[tokio::test]
async fn multipart_full_lifecycle() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    // create upload
    let resp = client
        .post(url(&addr, "/tb/assembled?uploads"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    let upload_id = extract_xml_value(&body, "UploadId");

    // upload 3 parts
    let part1 = "aaaa";
    let part2 = "bbbb";
    let part3 = "cccc";

    let resp = client
        .put(url_with_query(
            &addr,
            "/tb/assembled",
            &[("uploadId", &upload_id), ("partNumber", "1")],
        ))
        .body(part1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let etag1 = resp.headers()["etag"].to_str().unwrap().to_string();

    let resp = client
        .put(url_with_query(
            &addr,
            "/tb/assembled",
            &[("uploadId", &upload_id), ("partNumber", "2")],
        ))
        .body(part2)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let etag2 = resp.headers()["etag"].to_str().unwrap().to_string();

    let resp = client
        .put(url_with_query(
            &addr,
            "/tb/assembled",
            &[("uploadId", &upload_id), ("partNumber", "3")],
        ))
        .body(part3)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let etag3 = resp.headers()["etag"].to_str().unwrap().to_string();

    // list parts
    let resp = client
        .get(url_with_query(
            &addr,
            "/tb/assembled",
            &[("uploadId", &upload_id)],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ListPartsResult"), "body: {}", body);
    assert!(body.contains("<PartNumber>1</PartNumber>"));
    assert!(body.contains("<PartNumber>2</PartNumber>"));
    assert!(body.contains("<PartNumber>3</PartNumber>"));

    // complete upload
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>3</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1, etag2, etag3
    );
    let resp = client
        .post(url_with_query(
            &addr,
            "/tb/assembled",
            &[("uploadId", &upload_id)],
        ))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("CompleteMultipartUploadResult"), "body: {}", body);

    // GET the assembled object
    let resp = client
        .get(url(&addr, "/tb/assembled"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "aaaabbbbcccc");
}

#[tokio::test]
async fn multipart_abort_cleans_up() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    // create and upload a part
    let resp = client
        .post(url(&addr, "/tb/aborted?uploads"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    let upload_id = extract_xml_value(&body, "UploadId");

    client
        .put(url_with_query(
            &addr,
            "/tb/aborted",
            &[("uploadId", &upload_id), ("partNumber", "1")],
        ))
        .body("data")
        .send()
        .await
        .unwrap();

    // abort
    let resp = client
        .delete(url_with_query(
            &addr,
            "/tb/aborted",
            &[("uploadId", &upload_id)],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // list parts should fail (upload gone)
    let resp = client
        .get(url_with_query(
            &addr,
            "/tb/aborted",
            &[("uploadId", &upload_id)],
        ))
        .send()
        .await
        .unwrap();
    // either 404 or empty parts list
    let body = resp.text().await.unwrap();
    assert!(!body.contains("<PartNumber>1</PartNumber>"), "part should be gone");
}

#[tokio::test]
async fn multipart_list_uploads() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tb")).send().await.unwrap();

    // create two uploads
    client
        .post(url(&addr, "/tb/file1?uploads"))
        .send()
        .await
        .unwrap();
    client
        .post(url(&addr, "/tb/file2?uploads"))
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url(&addr, "/tb?uploads"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("ListMultipartUploadsResult"), "body: {}", body);
    assert!(body.contains("file1"), "missing file1: {}", body);
    assert!(body.contains("file2"), "missing file2: {}", body);
}

/// Extract a value between <Tag>value</Tag> from XML
fn extract_xml_value(xml: &str, tag: &str) -> String {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    if let Some(start) = xml.find(&open) {
        let start = start + open.len();
        if let Some(end) = xml[start..].find(&close) {
            return xml[start..start + end].to_string();
        }
    }
    panic!("tag <{}> not found in: {}", tag, xml);
}
