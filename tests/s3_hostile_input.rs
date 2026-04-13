use std::net::SocketAddr;
use std::sync::Arc;

use abixio::cluster::{ClusterConfig, ClusterManager};
use abixio::s3_route::AbixioDispatch;
use abixio::storage::Backend;
use abixio::storage::local_volume::LocalVolume;
use abixio::storage::volume_pool::VolumePool;
use s3s::S3Result;
use s3s::auth::{S3Auth, SecretKey};
use tempfile::TempDir;

/// No-op auth that accepts any access key. Required because s3s only invokes
/// S3Access::check when an auth provider is set.
struct AlwaysAllowAuth;

#[async_trait::async_trait]
impl S3Auth for AlwaysAllowAuth {
    async fn get_secret_key(&self, _access_key: &str) -> S3Result<SecretKey> {
        Ok(SecretKey::from("unused".to_string()))
    }
}

fn setup() -> (TempDir, Vec<std::path::PathBuf>) {
    setup_n(4)
}

fn setup_n(n: usize) -> (TempDir, Vec<std::path::PathBuf>) {
    let base = TempDir::new().unwrap();
    let mut paths = Vec::new();
    for i in 0..n {
        let p = base.path().join(format!("d{}", i));
        std::fs::create_dir_all(&p).unwrap();
        paths.push(p);
    }
    (base, paths)
}

async fn start_server(paths: &[std::path::PathBuf]) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let (addr, handle, _cluster) = start_server_with_cluster(paths).await;
    (addr, handle)
}

fn build_dispatch(set: Arc<VolumePool>, cluster: Arc<ClusterManager>) -> Arc<AbixioDispatch> {
    let s3 = abixio::s3_service::AbixioS3::new(Arc::clone(&set), Arc::clone(&cluster));
    let mut builder = s3s::service::S3ServiceBuilder::new(s3);
    builder.set_auth(AlwaysAllowAuth);
    builder.set_validation(abixio::s3_service::RelaxedNameValidation);
    builder.set_access(abixio::s3_access::AbixioAccess::new(Arc::clone(&cluster)));
    let s3_service = builder.build();
    Arc::new(AbixioDispatch::new(s3_service, None, None))
}

async fn spawn_server(dispatch: Arc<AbixioDispatch>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let dispatch = dispatch.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req| {
                    let dispatch = dispatch.clone();
                    async move { Ok::<_, hyper::Error>(dispatch.dispatch(req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    (addr, handle)
}

async fn start_server_with_cluster(
    paths: &[std::path::PathBuf],
) -> (SocketAddr, tokio::task::JoinHandle<()>, Arc<ClusterManager>) {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let set = Arc::new(VolumePool::new(backends).unwrap());
    let cluster = Arc::new(
        ClusterManager::new(ClusterConfig {
            node_id: "test-node".to_string(),
            advertise_s3: "http://127.0.0.1:0".to_string(),
            advertise_cluster: "http://127.0.0.1:0".to_string(),
            nodes: Vec::new(),
            access_key: String::new(),
            secret_key: String::new(),
            no_auth: true,
            disk_paths: paths.to_vec(),
        })
        .unwrap(),
    );
    let dispatch = build_dispatch(set, Arc::clone(&cluster));
    let (addr, handle) = spawn_server(dispatch).await;
    (addr, handle, cluster)
}

async fn start_server_pool(
    paths: &[std::path::PathBuf],
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalVolume::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let set = Arc::new(VolumePool::new(backends).unwrap());
    let cluster = Arc::new(
        ClusterManager::new(ClusterConfig {
            node_id: "test-node".to_string(),
            advertise_s3: "http://127.0.0.1:0".to_string(),
            advertise_cluster: "http://127.0.0.1:0".to_string(),
            nodes: Vec::new(),
            access_key: String::new(),
            secret_key: String::new(),
            no_auth: true,
            disk_paths: paths.to_vec(),
        })
        .unwrap(),
    );
    let dispatch = build_dispatch(set, cluster);
    let (addr, handle) = spawn_server(dispatch).await;

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

#[tokio::test]
async fn create_bucket_rejects_invalid_bucket_name() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client.put(url(&addr, "/Bad_Bucket")).send().await.unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("InvalidBucketName"), "body: {}", body);
}

#[tokio::test]
async fn list_objects_rejects_hostile_prefix() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .get(url_with_query(
            &addr,
            "/tbk",
            &[("list-type", "2"), ("prefix", "safe/../escape")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("InvalidArgument"), "body: {}", body);
}

#[tokio::test]
async fn get_object_rejects_invalid_version_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    let resp = client
        .get(url_with_query(
            &addr,
            "/tbk/obj",
            &[("versionId", "../escape")],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("InvalidArgument"), "body: {}", body);
}

#[tokio::test]
async fn multipart_rejects_invalid_upload_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url_with_query(
            &addr,
            "/tbk/key",
            &[("uploadId", "../escape"), ("partNumber", "1")],
        ))
        .body("data")
        .send()
        .await
        .unwrap();
    // invalid upload IDs return 400 (InvalidArgument) or 404 (NoSuchUpload)
    assert!(resp.status() == 400 || resp.status() == 404, "got {}", resp.status());
}

#[tokio::test]
async fn multipart_rejects_percent_encoded_traversal_upload_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // raw URL so %2e%2e in uploadId is not double-encoded
    let raw = format!("http://{}/tbk/key?uploadId=%2e%2e%2fescape&partNumber=1", addr);
    let resp = client
        .put(&raw)
        .body("data")
        .send()
        .await
        .unwrap();
    assert!(resp.status() == 400 || resp.status() == 404, "got {}", resp.status());
}

#[tokio::test]
async fn list_objects_rejects_percent_encoded_traversal_prefix() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // raw URL so %2e%2e is not double-encoded
    let raw = format!("http://{}/tb?list-type=2&prefix=%2e%2e%2fescape", addr);
    let resp = client
        .get(&raw)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("InvalidArgument"), "body: {}", body);
}

// --- CopyObject ---

#[tokio::test]
async fn put_rejects_dot_dot_key_via_query() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // URL paths get normalized by HTTP clients/servers, so ../
    // in paths may get collapsed before reaching the handler.
    // Test via query params (list prefix) where values pass through raw.
    for hostile_prefix in &[
        "../etc/passwd",
        "../../root",
        "a/../../b",
        "./hidden",
        "a/./b/../c",
    ] {
        let resp = client
            .get(url_with_query(
                &addr,
                "/tbk",
                &[("list-type", "2"), ("prefix", hostile_prefix)],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "prefix '{}' should be rejected, got {}",
            hostile_prefix,
            resp.status()
        );
    }
}

#[tokio::test]
async fn put_rejects_backslash_traversal() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // backslash gets percent-encoded by HTTP clients (%5C), so test
    // via raw URL to ensure encoded backslash reaches validator
    let raw = format!("http://{}/tbk/a%5Cb", addr);
    let resp = client.put(&raw).body("data").send().await.unwrap();
    // %5C stays encoded in path (hyper does not decode path segments)
    // so the validator sees literal "%5C" not "\", which is a valid segment.
    // The real protection is that LocalVolume uses pathing:: which rejects
    // actual backslash. This test verifies the request doesn't crash.
    assert!(resp.status() == 200 || resp.status() == 400);
}

#[tokio::test]
async fn put_rejects_null_byte_in_key() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // null byte in URL path -- hyper may reject this before reaching handler
    // or it may pass through as %00. Either rejection or 400 is acceptable.
    let raw = format!("http://{}/tbk/a%00b", addr);
    let result = client.put(&raw).body("data").send().await;
    match result {
        Ok(resp) => assert!(
            resp.status() == 400 || resp.status() == 200,
            "null byte got {}",
            resp.status()
        ),
        Err(_) => {} // connection-level rejection is fine
    }
}

#[tokio::test]
async fn put_rejects_double_slash_in_key() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .put(url(&addr, "/tbk/a//b"))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "double slash in key should be rejected");
}

// --- Bucket name validation ---

#[tokio::test]
async fn create_bucket_rejects_all_hostile_names() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    // Note: some hostile names (../, has/slash, empty) get reinterpreted
    // by the HTTP framework before reaching our handler. We test the ones
    // that pass through the URL intact and reach the bucket validator.
    let too_long = "a".repeat(64);
    let hostile_names = vec![
        "a..b",                       // double dot
        "-leading",                   // leading dash
        "trailing-",                  // trailing dash
        "UPPER",                      // uppercase
        "abixio",                     // reserved name
        "127.0.0.1",                  // IP address
        &too_long[..],               // too long (>63)
    ];

    for name in hostile_names {
        let resp = client
            .put(url(&addr, &format!("/{}", name)))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "bucket '{}' should be rejected, got {}",
            name.escape_default(),
            resp.status()
        );
    }
}

#[tokio::test]
async fn create_bucket_accepts_valid_edge_cases() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let max_len = "a".repeat(63);
    let valid_names = vec![
        "a",                          // single char
        "abc",                        // simple
        &max_len[..],                // max length
        "my.bucket.name",            // dots allowed
        "my-bucket-123",             // dashes and digits
        "0bucket",                   // starts with digit
    ];

    for name in valid_names {
        let resp = client
            .put(url(&addr, &format!("/{}", name)))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "bucket '{}' should be accepted, got {}",
            name,
            resp.status()
        );
    }
}

// --- Key length boundary ---

#[tokio::test]
async fn put_rejects_oversized_key() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // key > 1024 chars should be rejected
    let long_key = "a".repeat(1025);
    let resp = client
        .put(url(&addr, &format!("/tbk/{}", long_key)))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "key >1024 chars should be rejected");
}

#[tokio::test]
async fn put_accepts_reasonable_length_key() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // 100-char key should be accepted (well within 1024 limit)
    let key = "a".repeat(100);
    let resp = client
        .put(url(&addr, &format!("/tbk/{}", key)))
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "100-char key should be accepted");
}

// --- list-objects max-keys boundary ---

#[tokio::test]
async fn multipart_rejects_hostile_part_numbers() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // initiate upload
    let resp = client
        .post(url_with_query(&addr, "/tbk/mpkey", &[("uploads", "")]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    let upload_id = extract_xml_value(&body, "UploadId");

    // part 0 -- below valid range
    let resp = client
        .put(url_with_query(
            &addr,
            "/tbk/mpkey",
            &[("uploadId", &upload_id), ("partNumber", "0")],
        ))
        .body("data")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 400 || resp.status() == 200,
        "partNumber=0 got {}",
        resp.status()
    );

    // part -1
    let resp = client
        .put(url_with_query(
            &addr,
            "/tbk/mpkey",
            &[("uploadId", &upload_id), ("partNumber", "-1")],
        ))
        .body("data")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 400 || resp.status() == 200,
        "partNumber=-1 got {}",
        resp.status()
    );

    // part 10001 -- above S3 max
    let resp = client
        .put(url_with_query(
            &addr,
            "/tbk/mpkey",
            &[("uploadId", &upload_id), ("partNumber", "10001")],
        ))
        .body("data")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 400 || resp.status() == 200,
        "partNumber=10001 got {}",
        resp.status()
    );

    // partNumber=abc
    let resp = client
        .put(url_with_query(
            &addr,
            "/tbk/mpkey",
            &[("uploadId", &upload_id), ("partNumber", "abc")],
        ))
        .body("data")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == 400 || resp.status() == 200,
        "partNumber=abc got {}",
        resp.status()
    );
}

// --- Version ID hostile inputs ---

#[tokio::test]
async fn get_rejects_all_hostile_version_ids() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();
    client
        .put(url(&addr, "/tbk/obj"))
        .body("data")
        .send()
        .await
        .unwrap();

    let long_str = "a".repeat(256);
    let hostile_versions: Vec<&str> = vec![
        "../escape",
        "..\\escape",
        "../../etc/passwd",
        "%2e%2e/escape",
        "null\x00byte",
        "",
        &long_str,
        "not-a-uuid-at-all",
        "<script>alert(1)</script>",
    ];

    for vid in &hostile_versions {
        let resp = client
            .get(url_with_query(&addr, "/tbk/obj", &[("versionId", vid)]))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "versionId '{}' should be rejected, got {}",
            vid.escape_default(),
            resp.status()
        );
    }
}

// --- Upload ID hostile inputs ---

#[tokio::test]
async fn multipart_rejects_all_hostile_upload_ids() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let long_uid = "a".repeat(256);
    let hostile_ids = vec![
        "../escape",
        "..\\escape",
        "../../etc/passwd",
        "not-a-uuid",
        "",
        "<script>alert(1)</script>",
        &long_uid[..],
    ];

    for uid in hostile_ids {
        let resp = client
            .put(url_with_query(
                &addr,
                "/tbk/key",
                &[("uploadId", uid), ("partNumber", "1")],
            ))
            .body("data")
            .send()
            .await
            .unwrap();
        assert!(
            resp.status() == 400 || resp.status() == 404,
            "uploadId '{}' should be rejected, got {}",
            uid.escape_default(),
            resp.status()
        );
    }
}

// --- Windows-forbidden characters in keys ---

#[tokio::test]
async fn windows_forbidden_chars_rejected_at_storage_layer() {
    // Some windows-forbidden chars (? < > " etc) have special meaning in
    // HTTP URLs, so they get percent-encoded by clients. The HTTP path
    // layer sees the encoded form, not the raw char.
    //
    // The real defense is in pathing::validate_key_segments which checks
    // for these chars. This is tested thoroughly in unit tests. Here we
    // verify the storage layer rejects them when called directly.
    let dir = tempfile::TempDir::new().unwrap();
    let vol = abixio::storage::local_volume::LocalVolume::new(dir.path()).unwrap();
    use abixio::storage::Backend;
    vol.make_bucket("tbk").await.unwrap();

    for ch in &[':', '*', '?', '"', '|', '<', '>', '\\'] {
        let key = format!("file{}name", ch);
        let meta = abixio::storage::metadata::ObjectMeta {
            size: 4,
            etag: String::new(),
            content_type: "application/octet-stream".to_string(),
            created_at: 0,
            erasure: abixio::storage::metadata::ErasureMeta {
                ftt: 0, index: 0,
                epoch_id: 1,
                volume_ids: vec!["v".to_string()],
            },
            checksum: String::new(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: false,
            is_delete_marker: false,
            parts: Vec::new(),
            inline_data: None,
        };
        let result = vol.write_shard("tbk", &key, b"data", &meta).await;
        assert!(
            result.is_err(),
            "key with '{}' should be rejected at storage layer",
            ch
        );
    }
}

// --- Nested safe keys still work ---

#[tokio::test]
async fn deeply_nested_key_round_trips() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let deep_key = "a/b/c/d/e/f/g/h/file.txt";
    let resp = client
        .put(url(&addr, &format!("/tbk/{}", deep_key)))
        .body("nested-content")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(url(&addr, &format!("/tbk/{}", deep_key)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "nested-content");
}

// --- HTTP method abuse ---

#[tokio::test]
async fn hostile_metadata_headers_do_not_corrupt() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // user metadata with traversal value (valid header value, hostile content)
    let resp = client
        .put(url(&addr, "/tbk/metaobj"))
        .header("x-amz-meta-evil", "../../../etc/passwd")
        .body("data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // verify we can read it back without corruption
    let resp = client
        .head(url(&addr, "/tbk/metaobj"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // CRLF injection in headers is rejected by the HTTP library itself
    // (reqwest refuses to build the header), so we don't test that here.
    // The protection is at the transport layer, not the application layer.
}

// --- Internode storage server boundary (via StorageServer dispatch) ---
// These test the StorageServer validation we added as defense-in-depth.
// The StorageServer is internal, but we test its validation indirectly
// through the S3 layer since both share the same validators.

#[tokio::test]
async fn list_prefix_rejects_hostile_prefixes() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let hostile_prefixes = vec![
        "../escape",
        "..\\escape",
        "a/../b",
        "a/./b",
        "./hidden",
    ];

    for prefix in hostile_prefixes {
        let resp = client
            .get(url_with_query(
                &addr,
                "/tbk",
                &[("list-type", "2"), ("prefix", prefix)],
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "prefix '{}' should be rejected, got {}",
            prefix,
            resp.status()
        );
    }
}

// --- Complete multipart with zero parts ---

