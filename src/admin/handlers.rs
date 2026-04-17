use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use super::HealStats;
use super::types::*;
use crate::cluster::ClusterManager;
use crate::config::Config;
use crate::heal::mrf::MrfQueue;
use crate::heal::worker::{HealResult, heal_object};
use crate::query::parse_query;
use crate::storage::Backend;
use crate::storage::Store;
use crate::storage::bitrot::verify_shard_checksum;
use crate::storage::volume_pool::VolumePool;
use crate::storage::pathing;

type BoxBody = Full<Bytes>;

fn build_response(builder: hyper::http::response::Builder, body: Bytes) -> Response<BoxBody> {
    builder.body(Full::new(body)).unwrap_or_else(|e| {
        tracing::error!("response builder failed: {}", e);
        Response::new(Full::new(Bytes::from("internal error")))
    })
}

fn json_response(body: &impl serde::Serialize) -> Response<BoxBody> {
    let json = serde_json::to_string_pretty(body).unwrap_or_else(|_| "{}".to_string());
    build_response(
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json"),
        Bytes::from(json),
    )
}

fn error_json(status: StatusCode, msg: &str) -> Response<BoxBody> {
    let body = serde_json::json!({"error": msg});
    build_response(
        Response::builder()
            .status(status)
            .header("Content-Type", "application/json"),
        Bytes::from(body.to_string()),
    )
}

pub struct AdminHandler {
    store: Arc<VolumePool>,
    heal_disks: Arc<Vec<Box<dyn Backend>>>,
    mrf: Arc<MrfQueue>,
    stats: Arc<HealStats>,
    config: AdminConfig,
    cluster: Arc<ClusterManager>,
    metrics: Option<Arc<crate::metrics::Metrics>>,
    raft: Option<Arc<crate::raft::AbixioRaft>>,
}

/// Subset of server config needed by admin endpoints.
pub struct AdminConfig {
    pub listen: String,
    pub total_disks: usize,
    pub auth_enabled: bool,
    pub scan_interval: String,
    pub heal_interval: String,
    pub mrf_workers: usize,
    pub node_id: String,
    pub advertise_s3: String,
    pub advertise_cluster: String,
    pub node_count: usize,
}

impl AdminConfig {
    pub fn from_identity(
        identity: &crate::cluster::identity::ResolvedIdentity,
        cfg: &Config,
    ) -> Self {
        Self {
            listen: cfg.listen.clone(),
            total_disks: cfg.volumes.len(),
            auth_enabled: !cfg.no_auth,
            scan_interval: cfg.scan_interval.clone(),
            heal_interval: cfg.heal_interval.clone(),
            mrf_workers: cfg.mrf_workers,
            node_id: identity.node_id.clone(),
            advertise_s3: identity.advertise.clone(),
            advertise_cluster: identity.advertise.clone(),
            node_count: identity.nodes.len(),
        }
    }
}

impl AdminHandler {
    pub fn new(
        store: Arc<VolumePool>,
        heal_disks: Arc<Vec<Box<dyn Backend>>>,
        mrf: Arc<MrfQueue>,
        stats: Arc<HealStats>,
        config: AdminConfig,
        cluster: Arc<ClusterManager>,
    ) -> Self {
        Self {
            store,
            heal_disks,
            mrf,
            stats,
            config,
            cluster,
            metrics: None,
            raft: None,
        }
    }

    pub fn with_metrics(mut self, metrics: Arc<crate::metrics::Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn with_raft(mut self, raft: Arc<crate::raft::AbixioRaft>) -> Self {
        self.raft = Some(raft);
        self
    }

    pub async fn dispatch(&self, path: &str, method: &hyper::Method, query: &str) -> Response<BoxBody> {
        match (path, method) {
            ("status", &hyper::Method::GET) => self.status(),
            ("disks", &hyper::Method::GET) => self.disks().await,
            ("heal", &hyper::Method::GET) => self.heal_status(),
            ("heal", &hyper::Method::POST) => self.heal_object_handler(query).await,
            ("object", &hyper::Method::GET) => self.inspect_object(query).await,
            ("flush", &hyper::Method::POST) => {
                let _ = self.store.flush_write_cache().await;
                json_response(&serde_json::json!({"flushed": true}))
            },
            ("metrics", &hyper::Method::GET) => self.metrics_handler().await,
            ("raft/peers", &hyper::Method::GET) => self.raft_peers().await,
            ("raft/primary", &hyper::Method::GET) => self.raft_primary().await,
            ("raft/snapshot", &hyper::Method::POST) => self.raft_snapshot().await,
            ("raft/bootstrap", &hyper::Method::POST) => self.raft_bootstrap().await,
            ("raft/join", &hyper::Method::POST) => self.raft_join(query).await,
            ("raft/leave", &hyper::Method::POST) => self.raft_leave().await,
            ("cluster/status", &hyper::Method::GET) => self.cluster_status(),
            ("cluster/nodes", &hyper::Method::GET) => self.cluster_nodes(),
            ("cluster/epochs", &hyper::Method::GET) => self.cluster_epochs(),
            ("cluster/topology", &hyper::Method::GET) => self.cluster_topology(),
            _ if path.starts_with("bucket/") && path.ends_with("/ftt") => {
                let bucket = &path["bucket/".len()..path.len() - "/ftt".len()];
                match *method {
                    hyper::Method::GET => self.get_bucket_ftt(bucket).await,
                    hyper::Method::PUT => self.set_bucket_ftt(bucket, query).await,
                    _ => error_json(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
                }
            }
            _ => error_json(StatusCode::NOT_FOUND, "unknown admin endpoint"),
        }
    }

    async fn metrics_handler(&self) -> Response<BoxBody> {
        let Some(ref metrics) = self.metrics else {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "metrics disabled");
        };
        // refresh scrape-time gauges that are not already maintained
        // by the hot paths: disk capacity, cache sizes, WAL pending,
        // cluster state, MRF queue depth.
        self.store.refresh_scrape_gauges().await;
        let summary = self.cluster.summary();
        for label in ["Ready", "SyncingEpoch", "Fenced", "Joining"] {
            let current = match summary.state {
                crate::cluster::ServiceState::Ready => "Ready",
                crate::cluster::ServiceState::SyncingEpoch => "SyncingEpoch",
                crate::cluster::ServiceState::Fenced => "Fenced",
                crate::cluster::ServiceState::Joining => "Joining",
            };
            metrics.cluster_state
                .with_label_values(&[label])
                .set(if current == label { 1 } else { 0 });
        }
        metrics.cluster_reachable_voters.set(summary.reachable_voters as i64);
        metrics.cluster_quorum.set(summary.quorum as i64);
        metrics.cluster_epoch_id.set(summary.epoch_id as i64);
        metrics.mrf_queue_depth.set(self.mrf.pending_count() as i64);
        // HealStats tracks cumulative counts since process start;
        // reset-and-set keeps the Prometheus counter monotonic over
        // the lifetime of the process while tolerating restart.
        use std::sync::atomic::Ordering;
        metrics.scanner_objects_checked.reset();
        metrics.scanner_objects_checked.inc_by(self.stats.objects_scanned.load(Ordering::Relaxed));
        metrics.scanner_last_pass_seconds.set(self.stats.last_scan_duration_secs() as i64);
        metrics.heal_repaired.reset();
        metrics.heal_repaired.inc_by(self.stats.objects_healed.load(Ordering::Relaxed));

        let body = metrics.render();
        build_response(
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8"),
            Bytes::from(body),
        )
    }

    async fn raft_peers(&self) -> Response<BoxBody> {
        let Some(ref raft) = self.raft else {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "raft not enabled");
        };
        let members = raft.fsm.members();
        let primary_id = raft.raft.metrics().borrow().current_leader;
        let state = raft.raft.metrics().borrow().state;
        let peers: Vec<_> = members
            .into_iter()
            .map(|m| serde_json::json!({
                "node_id": m.node_id,
                "advertise_s3": m.advertise_s3,
                "advertise_cluster": m.advertise_cluster,
                "voter_kind": m.voter_kind,
                "added_at_unix_secs": m.added_at_unix_secs,
            }))
            .collect();
        json_response(&serde_json::json!({
            "primary_raft_id": primary_id,
            "this_node_raft_id": raft.node_id,
            "this_node_state": format!("{:?}", state),
            "peers": peers,
        }))
    }

    async fn raft_primary(&self) -> Response<BoxBody> {
        let Some(ref raft) = self.raft else {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "raft not enabled");
        };
        let metrics = raft.raft.metrics().borrow().clone();
        json_response(&serde_json::json!({
            "primary_raft_id": metrics.current_leader,
            "this_node_raft_id": raft.node_id,
            "this_node_state": format!("{:?}", metrics.state),
            "current_term": metrics.current_term,
            "last_log_index": metrics.last_log_index,
            "last_applied": metrics.last_applied,
        }))
    }

    async fn raft_bootstrap(&self) -> Response<BoxBody> {
        let Some(ref raft) = self.raft else {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "raft not enabled");
        };
        // Single-node bootstrap: this node is the only voter. Other
        // nodes join via `/raft/join` after bootstrap succeeds.
        let mut members = std::collections::BTreeMap::new();
        members.insert(
            raft.node_id,
            crate::raft::AbixioNode {
                advertise_s3: self.config.advertise_s3.clone(),
                advertise_cluster: self.config.advertise_cluster.clone(),
                voter_kind: crate::raft::VoterKind::Voter,
            },
        );
        match raft.raft.initialize(members).await {
            Ok(()) => json_response(&serde_json::json!({
                "bootstrapped": true,
                "raft_id": raft.node_id,
            })),
            Err(e) => {
                // openraft returns NotAllowed when already initialized.
                let msg = format!("{}", e);
                let status = if msg.contains("not allowed") || msg.contains("already") {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                error_json(status, &msg)
            }
        }
    }

    async fn raft_join(&self, query: &str) -> Response<BoxBody> {
        let Some(ref raft) = self.raft else {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "raft not enabled");
        };
        let params = parse_query(query);
        let joiner_raft_id: u64 = match params.get("raft_id").and_then(|s| s.parse().ok()) {
            Some(v) => v,
            None => return error_json(StatusCode::BAD_REQUEST, "missing or invalid raft_id"),
        };
        let Some(advertise_s3) = params.get("advertise_s3").cloned() else {
            return error_json(StatusCode::BAD_REQUEST, "missing advertise_s3");
        };
        let advertise_cluster = params
            .get("advertise_cluster")
            .cloned()
            .unwrap_or_else(|| advertise_s3.clone());
        let node = crate::raft::AbixioNode {
            advertise_s3,
            advertise_cluster,
            voter_kind: crate::raft::VoterKind::Voter,
        };

        // Must run on primary. Step 1: add as learner so the log can
        // replicate. Step 2: change membership to include as voter.
        if let Err(e) = raft.raft.add_learner(joiner_raft_id, node.clone(), true).await {
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("add_learner: {}", e),
            );
        }
        // Build the new voter set: current voters + joiner.
        let mut voters: std::collections::BTreeSet<u64> = raft
            .raft
            .metrics()
            .borrow()
            .membership_config
            .voter_ids()
            .collect();
        voters.insert(joiner_raft_id);
        match raft.raft.change_membership(voters, false).await {
            Ok(_) => json_response(&serde_json::json!({"joined": true, "raft_id": joiner_raft_id})),
            Err(e) => error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("change_membership: {}", e),
            ),
        }
    }

    async fn raft_leave(&self) -> Response<BoxBody> {
        let Some(ref raft) = self.raft else {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "raft not enabled");
        };
        let current: std::collections::BTreeSet<u64> = raft
            .raft
            .metrics()
            .borrow()
            .membership_config
            .voter_ids()
            .filter(|id| *id != raft.node_id)
            .collect();
        match raft.raft.change_membership(current, false).await {
            Ok(_) => json_response(&serde_json::json!({"left": true, "raft_id": raft.node_id})),
            Err(e) => error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("change_membership: {}", e),
            ),
        }
    }

    async fn raft_snapshot(&self) -> Response<BoxBody> {
        let Some(ref raft) = self.raft else {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "raft not enabled");
        };
        match raft.raft.trigger().snapshot().await {
            Ok(()) => json_response(&serde_json::json!({"triggered": true})),
            Err(e) => error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("snapshot trigger failed: {}", e),
            ),
        }
    }

    fn status(&self) -> Response<BoxBody> {
        json_response(&StatusResponse {
            server: "abixio",
            version: env!("CARGO_PKG_VERSION"),
            uptime_secs: self.stats.uptime_secs(),
            default_ftt: crate::storage::volume_pool::default_ftt(self.config.total_disks),
            total_disks: self.config.total_disks,
            listen: self.config.listen.clone(),
            auth_enabled: self.config.auth_enabled,
            scan_interval: self.config.scan_interval.clone(),
            heal_interval: self.config.heal_interval.clone(),
            mrf_workers: self.config.mrf_workers,
            cluster: self.cluster.summary(),
        })
    }

    fn cluster_status(&self) -> Response<BoxBody> {
        json_response(&self.cluster.status_response())
    }

    fn cluster_nodes(&self) -> Response<BoxBody> {
        json_response(&ClusterNodesResponse {
            nodes: self.cluster.nodes(),
        })
    }

    fn cluster_epochs(&self) -> Response<BoxBody> {
        json_response(&ClusterEpochsResponse {
            epochs: self.cluster.epochs(),
        })
    }

    fn cluster_topology(&self) -> Response<BoxBody> {
        json_response(&ClusterTopologyResponse {
            topology: self.cluster.topology(),
        })
    }

    async fn disks(&self) -> Response<BoxBody> {
        let mut disks = Vec::new();
        for (i, disk) in self.store.disks().iter().enumerate() {
            let backend_info = disk.info();
            let online = backend_info.total_bytes.is_some();

            let (bucket_count, object_count) = if online {
                count_buckets_and_objects(disk.as_ref()).await
            } else {
                (0, 0)
            };

            disks.push(DiskInfo {
                index: i,
                path: backend_info.label,
                online,
                total_bytes: backend_info.total_bytes.unwrap_or(0),
                used_bytes: backend_info.used_bytes.unwrap_or(0),
                free_bytes: backend_info.free_bytes.unwrap_or(0),
                bucket_count,
                object_count,
            });
        }
        json_response(&DisksResponse { disks })
    }

    fn heal_status(&self) -> Response<BoxBody> {
        json_response(&HealStatusResponse {
            mrf_pending: self.mrf.pending_count(),
            mrf_workers: self.config.mrf_workers,
            scanner: ScannerStatus {
                running: true,
                scan_interval: self.config.scan_interval.clone(),
                heal_interval: self.config.heal_interval.clone(),
                objects_scanned: self
                    .stats
                    .objects_scanned
                    .load(std::sync::atomic::Ordering::Relaxed),
                objects_healed: self
                    .stats
                    .objects_healed
                    .load(std::sync::atomic::Ordering::Relaxed),
                last_scan_started: self.stats.last_scan_started_secs_ago().unwrap_or(0),
                last_scan_duration_secs: self.stats.last_scan_duration_secs(),
            },
        })
    }

    async fn inspect_object(&self, query: &str) -> Response<BoxBody> {
        let params = parse_query(query);
        let bucket = match params.get("bucket") {
            Some(b) => b.as_str(),
            None => return error_json(StatusCode::BAD_REQUEST, "missing bucket parameter"),
        };
        let key = match params.get("key") {
            Some(k) => k.as_str(),
            None => return error_json(StatusCode::BAD_REQUEST, "missing key parameter"),
        };
        if let Err(e) = pathing::validate_bucket_name(bucket) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }
        if let Err(e) = pathing::validate_object_key(key) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }

        let disks = self.store.disks();

        // flush this object from cache to disk before inspecting
        // (inspect examines physical disk state)
        let _ = self.store.flush_write_cache().await;

        // check RAM write cache first (may still have entries if flush failed)
        if let Some(cache) = self.store.write_cache() {
            if let Some(entry) = cache.get(bucket, key) {
                let meta = entry.metas.first().cloned().unwrap_or_default();
                let total = entry.shards.len(); // use actual shard count, not derived
                let mut shards = Vec::new();
                for i in 0..total {
                    shards.push(serde_json::json!({
                        "index": i,
                        "disk": entry.distribution.get(i).copied().unwrap_or(0),
                        "status": "ok",
                        "location": "ram_cache",
                        "checksum_ok": true,
                    }));
                }
                // find a meta with valid erasure info
                let ec_meta = entry.metas.iter()
                    .find(|m| !m.erasure.volume_ids.is_empty())
                    .unwrap_or(&meta);
                let body = serde_json::json!({
                    "bucket": bucket,
                    "key": key,
                    "size": entry.original_size,
                    "etag": entry.etag,
                    "content_type": entry.content_type,
                    "erasure": {
                        "data": ec_meta.erasure.data(),
                        "parity": ec_meta.erasure.parity(),
                        "epoch_id": ec_meta.erasure.epoch_id,
                    },
                    "shards": shards,
                    "location": "ram_cache",
                });
                return json_response(&body);
            }
        }

        // read meta + shard from every disk
        let mut all_reads: Vec<Option<(Vec<u8>, crate::storage::metadata::ObjectMeta)>> =
            Vec::new();
        for disk in disks {
            match disk.read_shard(bucket, key).await {
                Ok(pair) => all_reads.push(Some(pair)),
                Err(_) => all_reads.push(None),
            }
        }

        // find first valid meta for consensus
        let consensus_meta = all_reads.iter().flatten().next().map(|(_, meta)| meta.clone());

        let meta = match consensus_meta {
            Some(m) => m,
            None => return error_json(StatusCode::NOT_FOUND, "object not found on any disk"),
        };

        // derive EC params from object's stored metadata
        let total = meta.erasure.data() + meta.erasure.parity();
        let mut shards: Vec<Option<ShardInfo>> = vec![None; total];

        // build shard status from each disk's read using erasure.index
        for (disk_idx, read) in all_reads.iter().enumerate() {
            if let Some((data, disk_meta)) = read {
                let shard_idx = disk_meta.erasure.index;
                if shard_idx >= total {
                    continue;
                }
                let (status, checksum) =
                    if disk_meta.quorum_eq(&meta) && verify_shard_checksum(data, &disk_meta.checksum) {
                        ("ok", Some(disk_meta.checksum.clone()))
                    } else {
                        ("corrupt", Some(disk_meta.checksum.clone()))
                    };
                shards[shard_idx] = Some(ShardInfo {
                    index: shard_idx,
                    disk: disk_idx,
                    volume_id: meta.erasure.volume_ids.get(shard_idx).cloned().unwrap_or_else(|| format!("vol-{}", disk_idx)),
                    status,
                    checksum,
                });
            }
        }
        // fill missing shard positions
        let shards: Vec<ShardInfo> = (0..total)
            .map(|shard_idx| {
                shards[shard_idx].take().unwrap_or(ShardInfo {
                    index: shard_idx,
                    disk: shard_idx,
                    volume_id: meta.erasure.volume_ids.get(shard_idx).cloned().unwrap_or_else(|| format!("vol-{}", shard_idx)),
                    status: "missing",
                    checksum: None,
                })
            })
            .collect();

        json_response(&ObjectInspectResponse {
            bucket: bucket.to_string(),
            key: key.to_string(),
            size: meta.size,
            etag: meta.etag.clone(),
            content_type: meta.content_type.clone(),
            created_at: meta.created_at,
            erasure: ErasureInfo {
                data: meta.erasure.data(),
                parity: meta.erasure.parity(),
                epoch_id: meta.erasure.epoch_id,
                volume_ids: meta.erasure.volume_ids.clone(),
            },
            shards,
        })
    }

    async fn get_bucket_ftt(&self, bucket: &str) -> Response<BoxBody> {
        if let Err(e) = pathing::validate_bucket_name(bucket) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }
        match self.store.get_ftt(bucket).await {
            Ok(Some(ftt)) => json_response(&serde_json::json!({ "ftt": ftt })),
            Ok(None) => {
                let ftt = crate::storage::volume_pool::default_ftt(self.store.disk_count());
                json_response(&serde_json::json!({ "ftt": ftt }))
            }
            Err(e) => error_json(map_storage_status(&e), &e.to_string()),
        }
    }

    async fn set_bucket_ftt(&self, bucket: &str, query: &str) -> Response<BoxBody> {
        if let Err(e) = pathing::validate_bucket_name(bucket) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }
        if !self.cluster.allows_mutation() {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "cluster is fenced");
        }
        let params = parse_query(query);
        let ftt: usize = match params.get("ftt").and_then(|v| v.parse().ok()) {
            Some(f) => f,
            None => return error_json(StatusCode::BAD_REQUEST, "missing ftt parameter"),
        };
        if let Err(e) = crate::storage::volume_pool::ftt_to_ec(ftt, self.store.disk_count()) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }
        match self.store.set_ftt(bucket, ftt).await {
            Ok(()) => json_response(&serde_json::json!({ "ftt": ftt })),
            Err(e) => error_json(StatusCode::BAD_REQUEST, &e.to_string()),
        }
    }

    async fn heal_object_handler(&self, query: &str) -> Response<BoxBody> {
        if !self.cluster.allows_mutation() {
            return error_json(StatusCode::SERVICE_UNAVAILABLE, "cluster is fenced");
        }
        let params = parse_query(query);
        let bucket = match params.get("bucket") {
            Some(b) => b.clone(),
            None => return error_json(StatusCode::BAD_REQUEST, "missing bucket parameter"),
        };
        let key = match params.get("key") {
            Some(k) => k.clone(),
            None => return error_json(StatusCode::BAD_REQUEST, "missing key parameter"),
        };
        if let Err(e) = pathing::validate_bucket_name(&bucket) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }
        if let Err(e) = pathing::validate_object_key(&key) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }

        match heal_object(
            &self.heal_disks,
            &bucket,
            &key,
        ).await {
            Ok(HealResult::Healthy) => json_response(&HealResponse {
                result: "healthy".to_string(),
                shards_fixed: None,
                error: None,
            }),
            Ok(HealResult::Repaired { shards_fixed }) => {
                self.stats.record_heal();
                json_response(&HealResponse {
                    result: "repaired".to_string(),
                    shards_fixed: Some(shards_fixed),
                    error: None,
                })
            }
            Ok(HealResult::Unrecoverable) => json_response(&HealResponse {
                result: "unrecoverable".to_string(),
                shards_fixed: None,
                error: Some("not enough healthy shards".to_string()),
            }),
            Err(e) => error_json(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }
}

fn map_storage_status(err: &crate::storage::StorageError) -> StatusCode {
    match err {
        crate::storage::StorageError::BucketNotFound | crate::storage::StorageError::ObjectNotFound => StatusCode::NOT_FOUND,
        crate::storage::StorageError::BucketExists | crate::storage::StorageError::BucketNotEmpty => StatusCode::CONFLICT,
        crate::storage::StorageError::InvalidConfig(_)
        | crate::storage::StorageError::InvalidBucketName(_)
        | crate::storage::StorageError::InvalidObjectKey(_)
        | crate::storage::StorageError::InvalidVersionId(_)
        | crate::storage::StorageError::InvalidUploadId(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Get total and free bytes for a filesystem path.
async fn count_buckets_and_objects(disk: &dyn Backend) -> (usize, usize) {
    let buckets = match disk.list_buckets().await {
        Ok(b) => b,
        Err(_) => return (0, 0),
    };

    let bucket_count = buckets.len();
    let mut object_count = 0;
    for bucket in &buckets {
        if let Ok(keys) = disk.list_objects(bucket, "").await {
            object_count += keys.len();
        }
    }

    (bucket_count, object_count)
}
