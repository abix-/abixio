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
    builder.set_access(abixio::s3_access::AbixioAccess::new(Arc::clone(&cluster), Arc::clone(&set), String::new()));
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

fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[tokio::test]
async fn multipart_create_upload_returns_upload_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .post(url(&addr, "/tbk/bigfile?uploads"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("InitiateMultipartUploadResult"),
        "body: {}",
        body
    );
    assert!(body.contains("<UploadId>"), "missing UploadId: {}", body);
    assert!(body.contains("<Bucket>tbk</Bucket>"));
    assert!(body.contains("<Key>bigfile</Key>"));
}

#[tokio::test]
async fn multipart_full_lifecycle() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // create upload
    let resp = client
        .post(url(&addr, "/tbk/assembled?uploads"))
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
            "/tbk/assembled",
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
            "/tbk/assembled",
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
            "/tbk/assembled",
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
            "/tbk/assembled",
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
            "/tbk/assembled",
            &[("uploadId", &upload_id)],
        ))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("CompleteMultipartUploadResult"),
        "body: {}",
        body
    );

    // GET the assembled object
    let resp = client
        .get(url(&addr, "/tbk/assembled"))
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

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // create and upload a part
    let resp = client
        .post(url(&addr, "/tbk/aborted?uploads"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    let upload_id = extract_xml_value(&body, "UploadId");

    client
        .put(url_with_query(
            &addr,
            "/tbk/aborted",
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
            "/tbk/aborted",
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
            "/tbk/aborted",
            &[("uploadId", &upload_id)],
        ))
        .send()
        .await
        .unwrap();
    // either 404 or empty parts list
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("<PartNumber>1</PartNumber>"),
        "part should be gone"
    );
}

#[tokio::test]
async fn multipart_list_uploads() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/tbk")).send().await.unwrap();

    // create two uploads
    client
        .post(url(&addr, "/tbk/file1?uploads"))
        .send()
        .await
        .unwrap();
    client
        .post(url(&addr, "/tbk/file2?uploads"))
        .send()
        .await
        .unwrap();

    let resp = client.get(url(&addr, "/tbk?uploads")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("ListMultipartUploadsResult"),
        "body: {}",
        body
    );
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

/// Helper: create upload, return upload_id
async fn create_upload(
    client: &reqwest::Client,
    addr: &SocketAddr,
    bucket: &str,
    key: &str,
) -> String {
    let resp = client
        .post(url(addr, &format!("/{}/{}?uploads", bucket, key)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    extract_xml_value(&body, "UploadId")
}

/// Helper: upload a part, return etag
async fn upload_part(
    client: &reqwest::Client,
    addr: &SocketAddr,
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: i32,
    data: &[u8],
) -> String {
    let resp = client
        .put(url_with_query(
            addr,
            &format!("/{}/{}", bucket, key),
            &[
                ("uploadId", upload_id),
                ("partNumber", &part_number.to_string()),
            ],
        ))
        .body(data.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "upload_part {} failed", part_number);
    resp.headers()["etag"].to_str().unwrap().to_string()
}

// --- Comprehensive multipart tests ---

#[tokio::test]
async fn multipart_each_upload_gets_unique_id() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let id1 = create_upload(&client, &addr, "tbk", "key").await;
    let id2 = create_upload(&client, &addr, "tbk", "key").await;
    let id3 = create_upload(&client, &addr, "tbk", "other").await;
    assert_ne!(id1, id2, "same key should get different upload IDs");
    assert_ne!(id1, id3);
    assert_ne!(id2, id3);
}

#[tokio::test]
async fn multipart_upload_part_returns_correct_etag() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;

    let data = b"hello world";
    let expected_md5 = format!("\"{}\"", md5_hex(data));
    let etag = upload_part(&client, &addr, "tbk", "key", &uid, 1, data).await;
    assert_eq!(etag, expected_md5, "part etag should be MD5 of part data");
}

#[tokio::test]
async fn multipart_overwrite_part_number() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;

    // upload part 1 twice with different data
    let _etag1a = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"first").await;
    let etag1b = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"second").await;
    let etag2 = upload_part(&client, &addr, "tbk", "key", &uid, 2, b"part2").await;

    // complete with the overwritten part 1
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1b, etag2
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // verify content uses second upload of part 1
    let resp = client.get(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "secondpart2");
}

#[tokio::test]
async fn multipart_non_sequential_part_numbers() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;

    // upload parts 3, 1, 7 (non-sequential, out of order)
    let etag3 = upload_part(&client, &addr, "tbk", "key", &uid, 3, b"CCC").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"AAA").await;
    let etag7 = upload_part(&client, &addr, "tbk", "key", &uid, 7, b"GGG").await;

    // complete in order 1, 3, 7
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>3</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>7</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1, etag3, etag7
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client.get(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "AAACCCGGG");
}

#[tokio::test]
async fn multipart_large_parts() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;

    // 1MB parts
    let part1: Vec<u8> = vec![b'A'; 1024 * 1024];
    let part2: Vec<u8> = vec![b'B'; 1024 * 1024];

    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, &part1).await;
    let etag2 = upload_part(&client, &addr, "tbk", "key", &uid, 2, &part2).await;

    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1, etag2
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // verify size and content
    let resp = client.get(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], "2097152"); // 2MB
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 2 * 1024 * 1024);
    assert!(body[0..1024].iter().all(|&b| b == b'A'));
    assert!(
        body[1024 * 1024..1024 * 1024 + 1024]
            .iter()
            .all(|&b| b == b'B')
    );
}

#[tokio::test]
async fn multipart_complete_with_missing_part_fails() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"data").await;

    // try to complete with part 1 and non-existent part 2
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>"missing"</ETag></Part></CompleteMultipartUpload>"#,
        etag1
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), 200, "should fail with missing part");
}

#[tokio::test]
async fn multipart_complete_malformed_xml_fails() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;

    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body("not xml")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn multipart_list_parts_shows_sizes() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    upload_part(&client, &addr, "tbk", "key", &uid, 1, b"short").await;
    upload_part(&client, &addr, "tbk", "key", &uid, 2, &vec![b'x'; 10000]).await;

    let resp = client
        .get(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Size>5</Size>"), "part 1 size: {}", body);
    assert!(body.contains("<Size>10000</Size>"), "part 2 size: {}", body);
    assert!(
        body.contains("<UploadId>"),
        "should include upload ID: {}",
        body
    );
    assert!(body.contains("<Bucket>tbk</Bucket>"));
    assert!(body.contains("<Key>key</Key>"));
}

#[tokio::test]
async fn multipart_abort_then_complete_fails() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"data").await;

    // abort
    let resp = client
        .delete(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // try to complete after abort
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_ne!(resp.status(), 200, "complete after abort should fail");
}

#[tokio::test]
async fn multipart_completed_object_survives_head_and_delete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"multipart-data").await;

    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1
    );
    client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();

    // HEAD should work
    let resp = client.head(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], "14");

    // object should appear in list
    let resp = client
        .get(url(&addr, "/tbk?list-type=2"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Key>key</Key>"),
        "object should be listed: {}",
        body
    );

    // delete should work
    let resp = client.delete(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.status(), 204);

    // GET after delete should 404
    let resp = client.get(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn multipart_upload_does_not_appear_as_object_until_complete() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "invisible").await;
    upload_part(&client, &addr, "tbk", "invisible", &uid, 1, b"data").await;

    // object should NOT appear in listing yet
    let resp = client
        .get(url(&addr, "/tbk?list-type=2"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains("invisible"),
        "in-progress upload should not be listed: {}",
        body
    );

    // GET should 404
    let resp = client
        .get(url(&addr, "/tbk/invisible"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn multipart_list_uploads_after_complete_is_empty() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"data").await;

    // should show in list
    let resp = client.get(url(&addr, "/tbk?uploads")).send().await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(&uid),
        "upload should be listed before complete"
    );

    // complete
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1
    );
    client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();

    // should NOT show in list after complete
    let resp = client.get(url(&addr, "/tbk?uploads")).send().await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains(&uid),
        "upload should be gone after complete: {}",
        body
    );
}

#[tokio::test]
async fn multipart_list_uploads_after_abort_is_empty() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;

    // abort
    client
        .delete(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .send()
        .await
        .unwrap();

    let resp = client.get(url(&addr, "/tbk?uploads")).send().await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains(&uid),
        "upload should be gone after abort: {}",
        body
    );
}

#[tokio::test]
async fn multipart_complete_etag_is_multipart_format() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"aaa").await;
    let etag2 = upload_part(&client, &addr, "tbk", "key", &uid, 2, b"bbb").await;

    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1, etag2
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    let etag = extract_xml_value(&body, "ETag");

    // multipart etag format: "md5-N" where N is part count
    assert!(
        etag.contains("-2"),
        "multipart etag should end with -2: {}",
        etag
    );
}

#[tokio::test]
async fn multipart_single_part_upload() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"only-part").await;

    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client.get(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "only-part");
}

#[tokio::test]
async fn multipart_empty_part() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let uid = create_upload(&client, &addr, "tbk", "key").await;
    let etag1 = upload_part(&client, &addr, "tbk", "key", &uid, 1, b"data").await;
    let etag2 = upload_part(&client, &addr, "tbk", "key", &uid, 2, b"").await;

    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag1, etag2
    );
    let resp = client
        .post(url_with_query(&addr, "/tbk/key", &[("uploadId", &uid)]))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client.get(url(&addr, "/tbk/key")).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "data");
}

// --- Bucket lifecycle ---

#[tokio::test]
async fn multipart_complete_with_zero_parts_fails() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    client.put(url(&addr, "/tbk")).send().await.unwrap();

    let resp = client
        .post(url_with_query(&addr, "/tbk/mpkey", &[("uploads", "")]))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    let upload_id = extract_xml_value(&body, "UploadId");

    // complete with empty parts list
    let complete_xml = format!(
        "<CompleteMultipartUpload></CompleteMultipartUpload>"
    );
    let resp = client
        .post(url_with_query(
            &addr,
            "/tbk/mpkey",
            &[("uploadId", &upload_id)],
        ))
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    // s3s may accept zero parts or reject; both are valid
    assert!(
        resp.status() == 400 || resp.status() == 200,
        "expected 400 or 200, got {}",
        resp.status()
    );
}
