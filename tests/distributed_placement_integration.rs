#[path = "support/mod.rs"]
mod support;

use reqwest::Client;
use serde_json::Value;
use support::ClusterHarness;

fn endpoint(node: &support::TestNode, path: &str) -> String {
    format!("{}{}", node.endpoint(), path)
}

#[tokio::test]
async fn distributed_placement_exact_map_matches_raw_disk_state() {
    let harness = ClusterHarness::new().await;
    let client = Client::new();

    let resp = client
        .put(endpoint(&harness.nodes[0], "/testbucket"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // set bucket FTT=2 (2+2 on 4 shards used)
    let resp = client
        .put(endpoint(&harness.nodes[0], "/_admin/bucket/testbucket/ftt?ftt=2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let payload = "hello distributed placement";
    let resp = client
        .put(endpoint(&harness.nodes[1], "/testbucket/alpha.txt"))
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let inspect: Value = client
        .get(endpoint(
            &harness.nodes[2],
            "/_admin/object?bucket=testbucket&key=alpha.txt",
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // FTT=2 on 8 disks -> 6+2
    assert_eq!(inspect["erasure"]["data"], 6);
    assert_eq!(inspect["erasure"]["parity"], 2);
    assert_eq!(inspect["erasure"]["epoch_id"], 7);
    assert_eq!(inspect["erasure"]["set_id"], "cluster-set-4x2");

    let shards = inspect["shards"].as_array().unwrap();
    assert_eq!(shards.len(), 8);
    let distinct_nodes = shards
        .iter()
        .map(|shard| shard["node_id"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(distinct_nodes.len(), 4);

    for shard in shards {
        let volume_id = shard["volume_id"].as_str().unwrap();
        let obj_dir = harness.shard_root(volume_id).join("testbucket").join("alpha.txt");
        assert!(obj_dir.join("shard.dat").exists(), "missing shard data for {}", volume_id);
        assert!(obj_dir.join("meta.json").exists(), "missing meta for {}", volume_id);
    }
}

#[tokio::test]
async fn distributed_placement_is_identical_from_every_node() {
    let harness = ClusterHarness::new().await;
    let client = Client::new();

    client
        .put(endpoint(&harness.nodes[0], "/testbucket"))
        .send()
        .await
        .unwrap();
    client
        .put(endpoint(&harness.nodes[0], "/_admin/bucket/testbucket/ftt?ftt=2"))
        .send()
        .await
        .unwrap();
    client
        .put(endpoint(&harness.nodes[0], "/testbucket/docs/readme.txt"))
        .body("consistent map")
        .send()
        .await
        .unwrap();

    let mut inspections = Vec::new();
    for node in &harness.nodes {
        let inspect: Value = client
            .get(endpoint(
                node,
                "/_admin/object?bucket=testbucket&key=docs/readme.txt",
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        inspections.push(inspect);
    }

    for inspect in inspections.iter().skip(1) {
        assert_eq!(inspect["erasure"], inspections[0]["erasure"]);
        assert_eq!(inspect["shards"], inspections[0]["shards"]);
    }
}

#[tokio::test]
async fn distributed_read_survives_one_node_loss_for_2_plus_2() {
    let mut harness = ClusterHarness::new().await;
    let client = Client::new();

    client
        .put(endpoint(&harness.nodes[0], "/testbucket"))
        .send()
        .await
        .unwrap();
    client
        .put(endpoint(&harness.nodes[0], "/_admin/bucket/testbucket/ftt?ftt=2"))
        .send()
        .await
        .unwrap();
    client
        .put(endpoint(&harness.nodes[0], "/testbucket/resilient.txt"))
        .body("survives one node")
        .send()
        .await
        .unwrap();

    harness.stop_node(3);

    let resp = client
        .get(endpoint(&harness.nodes[0], "/testbucket/resilient.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "survives one node");
}

#[tokio::test]
async fn distributed_read_fails_beyond_parity_limit_for_2_plus_2() {
    let mut harness = ClusterHarness::new().await;
    let client = Client::new();

    client
        .put(endpoint(&harness.nodes[0], "/testbucket"))
        .send()
        .await
        .unwrap();
    client
        .put(endpoint(&harness.nodes[0], "/testbucket/unrecoverable.txt"))
        .body("needs two data shards")
        .send()
        .await
        .unwrap();

    harness.stop_node(2);
    harness.stop_node(3);
    harness.stop_node(1);

    let resp = client
        .get(endpoint(&harness.nodes[0], "/testbucket/unrecoverable.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn fenced_node_returns_503_for_s3() {
    let harness = ClusterHarness::new().await;
    let client = Client::new();

    harness.fence_node(2, "isolated from control plane");

    let resp = client
        .put(endpoint(&harness.nodes[2], "/testbucket"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
}
