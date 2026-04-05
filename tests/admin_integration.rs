use std::net::SocketAddr;
use std::sync::Arc;

use abixio::admin::HealStats;
use abixio::admin::handlers::{AdminConfig, AdminHandler};
use abixio::heal::mrf::MrfQueue;
use abixio::s3::auth::AuthConfig;
use abixio::s3::handlers::S3Handler;
use abixio::storage::Backend;
use abixio::storage::disk::LocalDisk;
use abixio::storage::erasure_set::ErasureSet;
use tempfile::TempDir;

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
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalDisk::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let mut set = ErasureSet::new(backends, 2, 2).unwrap();

    let mrf = Arc::new(MrfQueue::new(1000));
    set.set_mrf(Arc::clone(&mrf));
    let set = Arc::new(set);

    let heal_disks: Arc<Vec<Box<dyn Backend>>> = Arc::new(
        paths
            .iter()
            .filter_map(|p| LocalDisk::new(p.as_path()).ok())
            .map(|d| Box::new(d) as Box<dyn Backend>)
            .collect(),
    );

    let heal_stats = Arc::new(HealStats::new());

    let admin_config = AdminConfig {
        listen: ":0".to_string(),
        data_shards: 2,
        parity_shards: 2,
        total_disks: 4,
        auth_enabled: false,
        scan_interval: "10m".to_string(),
        heal_interval: "24h".to_string(),
        mrf_workers: 2,
    };

    let admin = Arc::new(AdminHandler::new(
        Arc::clone(&set),
        heal_disks,
        mrf,
        heal_stats,
        admin_config,
    ));

    let auth = AuthConfig {
        access_key: String::new(),
        secret_key: String::new(),
        no_auth: true,
    };
    let mut handler = S3Handler::new(set, auth);
    handler.set_admin(admin);
    let handler = Arc::new(handler);

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

async fn start_server_pool(
    paths: &[std::path::PathBuf],
    data: usize,
    parity: usize,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let backends: Vec<Box<dyn Backend>> = paths
        .iter()
        .map(|p| Box::new(LocalDisk::new(p.as_path()).unwrap()) as Box<dyn Backend>)
        .collect();
    let mut set = ErasureSet::new(backends, data, parity).unwrap();

    let mrf = Arc::new(MrfQueue::new(1000));
    set.set_mrf(Arc::clone(&mrf));
    let set = Arc::new(set);

    let heal_disks: Arc<Vec<Box<dyn Backend>>> = Arc::new(
        paths
            .iter()
            .filter_map(|p| LocalDisk::new(p.as_path()).ok())
            .map(|d| Box::new(d) as Box<dyn Backend>)
            .collect(),
    );

    let heal_stats = Arc::new(HealStats::new());

    let admin_config = AdminConfig {
        listen: ":0".to_string(),
        data_shards: data,
        parity_shards: parity,
        total_disks: paths.len(),
        auth_enabled: false,
        scan_interval: "10m".to_string(),
        heal_interval: "24h".to_string(),
        mrf_workers: 2,
    };

    let admin = Arc::new(AdminHandler::new(
        Arc::clone(&set),
        heal_disks,
        mrf,
        heal_stats,
        admin_config,
    ));

    let auth = AuthConfig {
        access_key: String::new(),
        secret_key: String::new(),
        no_auth: true,
    };
    let mut handler = S3Handler::new(set, auth);
    handler.set_admin(admin);
    let handler = Arc::new(handler);

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

// -- status endpoint --

#[tokio::test]
async fn admin_status_returns_json() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/_admin/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["server"], "abixio");
    assert_eq!(body["data_shards"], 2);
    assert_eq!(body["parity_shards"], 2);
    assert_eq!(body["total_disks"], 4);
    assert_eq!(body["auth_enabled"], false);
}

#[tokio::test]
async fn admin_status_has_uptime() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let body: serde_json::Value = client
        .get(url(&addr, "/_admin/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(body["uptime_secs"].as_u64().is_some());
    assert!(body["version"].as_str().is_some());
}

// -- disks endpoint --

#[tokio::test]
async fn admin_disks_returns_all_disks() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let body: serde_json::Value = client
        .get(url(&addr, "/_admin/disks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let disks = body["disks"].as_array().unwrap();
    assert_eq!(disks.len(), 4);

    for (i, disk) in disks.iter().enumerate() {
        assert_eq!(disk["index"], i);
        assert_eq!(disk["online"], true);
        assert!(disk["path"].as_str().is_some());
        assert!(disk["total_bytes"].as_u64().unwrap() > 0);
    }
}

#[tokio::test]
async fn admin_disks_counts_buckets_and_objects() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    // create bucket and objects
    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/obj1"))
        .body("data1")
        .send()
        .await
        .unwrap();
    client
        .put(url(&addr, "/testbucket/obj2"))
        .body("data2")
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = client
        .get(url(&addr, "/_admin/disks"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let disks = body["disks"].as_array().unwrap();
    // every disk should have at least 1 bucket
    for disk in disks {
        assert!(disk["bucket_count"].as_u64().unwrap() >= 1);
    }
}

// -- heal status endpoint --

#[tokio::test]
async fn admin_heal_status_returns_scanner_info() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let body: serde_json::Value = client
        .get(url(&addr, "/_admin/heal"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["mrf_pending"], 0);
    assert_eq!(body["mrf_workers"], 2);
    assert!(body["scanner"]["scan_interval"].as_str().is_some());
    assert!(body["scanner"]["heal_interval"].as_str().is_some());
}

// -- object inspect endpoint --

#[tokio::test]
async fn admin_inspect_object_shows_all_shards() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/hello.txt"))
        .body("hello world")
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = client
        .get(url(&addr, "/_admin/object?bucket=testbucket&key=hello.txt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["bucket"], "testbucket");
    assert_eq!(body["key"], "hello.txt");
    assert_eq!(body["size"], 11);
    assert_eq!(body["erasure"]["data"], 2);
    assert_eq!(body["erasure"]["parity"], 2);

    let shards = body["shards"].as_array().unwrap();
    assert_eq!(shards.len(), 4);
    for shard in shards {
        assert_eq!(shard["status"], "ok");
        assert!(shard["checksum"].as_str().is_some());
    }
}

#[tokio::test]
async fn admin_inspect_missing_object_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();

    let resp = client
        .get(url(
            &addr,
            "/_admin/object?bucket=testbucket&key=nonexistent",
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn admin_inspect_missing_params_returns_400() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/_admin/object"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let resp = client
        .get(url(&addr, "/_admin/object?bucket=test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

// -- manual heal endpoint --

#[tokio::test]
async fn admin_heal_healthy_object_returns_healthy() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/hello.txt"))
        .body("hello")
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = client
        .post(url(&addr, "/_admin/heal?bucket=testbucket&key=hello.txt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["result"], "healthy");
}

#[tokio::test]
async fn admin_heal_missing_shards_repairs() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/fixme.txt"))
        .body("repair this")
        .send()
        .await
        .unwrap();

    // inspect to find shard distribution
    let inspect: serde_json::Value = client
        .get(url(&addr, "/_admin/object?bucket=testbucket&key=fixme.txt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // delete shards from 2 disks (within parity tolerance)
    let shards = inspect["shards"].as_array().unwrap();
    let disk0 = shards[0]["disk"].as_u64().unwrap() as usize;
    let disk1 = shards[1]["disk"].as_u64().unwrap() as usize;
    let obj_dir0 = paths[disk0].join("testbucket").join("fixme.txt");
    let obj_dir1 = paths[disk1].join("testbucket").join("fixme.txt");
    let _ = std::fs::remove_dir_all(&obj_dir0);
    let _ = std::fs::remove_dir_all(&obj_dir1);

    // verify shards missing
    let inspect2: serde_json::Value = client
        .get(url(&addr, "/_admin/object?bucket=testbucket&key=fixme.txt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let missing_count = inspect2["shards"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["status"] == "missing")
        .count();
    assert!(missing_count >= 1, "expected missing shards after deletion");

    // data still readable (erasure resilience)
    let resp = client
        .get(url(&addr, "/testbucket/fixme.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "repair this");

    // trigger heal
    let heal_resp: serde_json::Value = client
        .post(url(&addr, "/_admin/heal?bucket=testbucket&key=fixme.txt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(heal_resp["result"], "repaired");
    assert!(heal_resp["shards_fixed"].as_u64().unwrap() >= 1);

    // verify all shards restored
    let inspect3: serde_json::Value = client
        .get(url(&addr, "/_admin/object?bucket=testbucket&key=fixme.txt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for shard in inspect3["shards"].as_array().unwrap() {
        assert_eq!(
            shard["status"], "ok",
            "shard {} not ok after heal",
            shard["index"]
        );
    }
}

#[tokio::test]
async fn admin_endpoints_accept_encoded_bucket_and_key() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();
    let key = "dir-one/inspect-me.txt";

    client.put(url(&addr, "/testbucket")).send().await.unwrap();
    client
        .put(url(&addr, "/testbucket/dir-one/inspect-me.txt"))
        .body("encoded admin object")
        .send()
        .await
        .unwrap();

    let inspect: serde_json::Value = client
        .get(url_with_query(
            &addr,
            "/_admin/object",
            &[("bucket", "testbucket"), ("key", key)],
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(inspect["bucket"], "testbucket");
    assert_eq!(inspect["key"], key);
    assert_eq!(inspect["size"], "encoded admin object".len() as u64);

    let heal: serde_json::Value = client
        .post(url_with_query(
            &addr,
            "/_admin/heal",
            &[("bucket", "testbucket"), ("key", key)],
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(heal["result"], "healthy");
}

// -- unknown admin endpoint --

#[tokio::test]
async fn admin_unknown_endpoint_returns_404() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/_admin/nonexistent"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// -- admin endpoints don't break S3 --

#[tokio::test]
async fn s3_still_works_with_admin_enabled() {
    let (_base, paths) = setup();
    let (addr, _handle) = start_server(&paths).await;
    let client = reqwest::Client::new();

    // S3 operations
    client.put(url(&addr, "/mybucket")).send().await.unwrap();
    client
        .put(url(&addr, "/mybucket/test"))
        .body("works")
        .send()
        .await
        .unwrap();
    let resp = client
        .get(url(&addr, "/mybucket/test"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.text().await.unwrap(), "works");

    // admin also works
    let resp = client
        .get(url(&addr, "/_admin/status"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // S3 listing still works
    let resp = client.get(url(&addr, "/")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("mybucket"));
}

// -- bucket EC config --

#[tokio::test]
async fn admin_get_ec_config_returns_server_default() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths, 2, 2).await;
    let client = reqwest::Client::new();

    // create bucket
    client.put(url(&addr, "/ecbucket")).send().await.unwrap();

    // get EC config -- should return server defaults
    let resp = client
        .get(url(&addr, "/_admin/bucket/ecbucket/ec"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["data"], 2);
    assert_eq!(body["parity"], 2);
    assert_eq!(body["source"], "server_default");
}

#[tokio::test]
async fn admin_set_and_get_ec_config() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths, 2, 2).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/ecbucket")).send().await.unwrap();

    // set EC config
    let set_url = url_with_query(
        &addr,
        "/_admin/bucket/ecbucket/ec",
        &[("data", "3"), ("parity", "3")],
    );
    let resp = client.put(set_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["data"], 3);
    assert_eq!(body["parity"], 3);

    // get should return the custom config
    let resp = client
        .get(url(&addr, "/_admin/bucket/ecbucket/ec"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["data"], 3);
    assert_eq!(body["parity"], 3);
}

#[tokio::test]
async fn admin_set_ec_config_invalid_params() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths, 2, 2).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/ecbucket")).send().await.unwrap();

    // data=0 should fail
    let set_url = url_with_query(
        &addr,
        "/_admin/bucket/ecbucket/ec",
        &[("data", "0"), ("parity", "2")],
    );
    let resp = client.put(set_url).send().await.unwrap();
    assert_eq!(resp.status(), 400);

    // exceeds disk count should fail
    let set_url = url_with_query(
        &addr,
        "/_admin/bucket/ecbucket/ec",
        &[("data", "4"), ("parity", "4")],
    );
    let resp = client.put(set_url).send().await.unwrap();
    assert_eq!(resp.status(), 400);

    // missing params should fail
    let resp = client
        .put(url(&addr, "/_admin/bucket/ecbucket/ec"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn admin_ec_config_nonexistent_bucket_returns_404() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths, 2, 2).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(url(&addr, "/_admin/bucket/nonexistent/ec"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn admin_bucket_ec_config_affects_new_objects() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths, 2, 2).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/ecobj")).send().await.unwrap();

    // set bucket EC to 3+3
    let set_url = url_with_query(
        &addr,
        "/_admin/bucket/ecobj/ec",
        &[("data", "3"), ("parity", "3")],
    );
    client.put(set_url).send().await.unwrap();

    // write object (no per-object EC headers -- should use bucket default)
    client
        .put(url(&addr, "/ecobj/mykey"))
        .body("bucket ec test")
        .send()
        .await
        .unwrap();

    // verify it used 3+3 by checking shard count on disk
    let mut disks_with_obj = 0;
    for p in &paths {
        if p.join("ecobj").join("mykey").join("meta.json").exists() {
            disks_with_obj += 1;
        }
    }
    assert_eq!(disks_with_obj, 6, "bucket EC 3+3 should use all 6 disks");

    // read back
    let resp = client
        .get(url(&addr, "/ecobj/mykey"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"bucket ec test");
}

#[tokio::test]
async fn admin_inspect_object_shows_per_object_ec() {
    let (_base, paths) = setup_n(6);
    let (addr, _handle) = start_server_pool(&paths, 2, 2).await;
    let client = reqwest::Client::new();

    client.put(url(&addr, "/insp")).send().await.unwrap();

    // write with custom EC
    client
        .put(url(&addr, "/insp/key"))
        .header("x-amz-meta-ec-data", "1")
        .header("x-amz-meta-ec-parity", "5")
        .body("inspect me")
        .send()
        .await
        .unwrap();

    // inspect should show the object's actual EC params
    let inspect_url = url_with_query(
        &addr,
        "/_admin/object",
        &[("bucket", "insp"), ("key", "key")],
    );
    let resp = client.get(inspect_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["erasure"]["data"], 1);
    assert_eq!(body["erasure"]["parity"], 5);
    assert_eq!(body["shards"].as_array().unwrap().len(), 6);
}
