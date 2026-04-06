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
use crate::storage::bitrot::sha256_hex;
use crate::storage::volume_pool::VolumePool;
use crate::storage::pathing;

type BoxBody = Full<Bytes>;

fn json_response(body: &impl serde::Serialize) -> Response<BoxBody> {
    let json = serde_json::to_string_pretty(body).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap()
}

fn error_json(status: StatusCode, msg: &str) -> Response<BoxBody> {
    let body = serde_json::json!({"error": msg});
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

pub struct AdminHandler {
    store: Arc<VolumePool>,
    heal_disks: Arc<Vec<Box<dyn Backend>>>,
    mrf: Arc<MrfQueue>,
    stats: Arc<HealStats>,
    config: AdminConfig,
    cluster: Arc<ClusterManager>,
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
        }
    }

    pub fn dispatch(&self, path: &str, method: &hyper::Method, query: &str) -> Response<BoxBody> {
        match (path, method) {
            ("status", &hyper::Method::GET) => self.status(),
            ("disks", &hyper::Method::GET) => self.disks(),
            ("heal", &hyper::Method::GET) => self.heal_status(),
            ("heal", &hyper::Method::POST) => self.heal_object_handler(query),
            ("object", &hyper::Method::GET) => self.inspect_object(query),
            ("cluster/status", &hyper::Method::GET) => self.cluster_status(),
            ("cluster/nodes", &hyper::Method::GET) => self.cluster_nodes(),
            ("cluster/epochs", &hyper::Method::GET) => self.cluster_epochs(),
            ("cluster/topology", &hyper::Method::GET) => self.cluster_topology(),
            _ if path.starts_with("bucket/") && path.ends_with("/ftt") => {
                let bucket = &path["bucket/".len()..path.len() - "/ftt".len()];
                match *method {
                    hyper::Method::GET => self.get_bucket_ftt(bucket),
                    hyper::Method::PUT => self.set_bucket_ftt(bucket, query),
                    _ => error_json(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
                }
            }
            _ => error_json(StatusCode::NOT_FOUND, "unknown admin endpoint"),
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

    fn disks(&self) -> Response<BoxBody> {
        let mut disks = Vec::new();
        for (i, disk) in self.store.disks().iter().enumerate() {
            let backend_info = disk.info();
            let online = backend_info.total_bytes.is_some();

            let (bucket_count, object_count) = if online {
                count_buckets_and_objects(disk.as_ref())
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

    fn inspect_object(&self, query: &str) -> Response<BoxBody> {
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

        // read meta + shard from every disk
        let mut all_reads: Vec<Option<(Vec<u8>, crate::storage::metadata::ObjectMeta)>> =
            Vec::new();
        for disk in disks {
            match disk.read_shard(bucket, key) {
                Ok(pair) => all_reads.push(Some(pair)),
                Err(_) => all_reads.push(None),
            }
        }

        // find first valid meta for consensus
        let mut consensus_meta = None;
        for (_, meta) in all_reads.iter().flatten() {
            consensus_meta = Some(meta.clone());
            break;
        }

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
                    if disk_meta.quorum_eq(&meta) && sha256_hex(data) == disk_meta.checksum {
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
                pool_id: meta.erasure.pool_id.clone(),
                volume_ids: meta.erasure.volume_ids.clone(),
            },
            shards,
        })
    }

    fn get_bucket_ftt(&self, bucket: &str) -> Response<BoxBody> {
        if let Err(e) = pathing::validate_bucket_name(bucket) {
            return error_json(StatusCode::BAD_REQUEST, &e.to_string());
        }
        match self.store.get_ftt(bucket) {
            Ok(Some(ftt)) => json_response(&serde_json::json!({ "ftt": ftt })),
            Ok(None) => {
                let ftt = crate::storage::volume_pool::default_ftt(self.store.disk_count());
                json_response(&serde_json::json!({ "ftt": ftt }))
            }
            Err(e) => error_json(map_storage_status(&e), &e.to_string()),
        }
    }

    fn set_bucket_ftt(&self, bucket: &str, query: &str) -> Response<BoxBody> {
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
        match self.store.set_ftt(bucket, ftt) {
            Ok(()) => json_response(&serde_json::json!({ "ftt": ftt })),
            Err(e) => error_json(StatusCode::BAD_REQUEST, &e.to_string()),
        }
    }

    fn heal_object_handler(&self, query: &str) -> Response<BoxBody> {
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
        ) {
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
fn count_buckets_and_objects(disk: &dyn Backend) -> (usize, usize) {
    let buckets = match disk.list_buckets() {
        Ok(b) => b,
        Err(_) => return (0, 0),
    };

    let bucket_count = buckets.len();
    let mut object_count = 0;
    for bucket in &buckets {
        if let Ok(keys) = disk.list_objects(bucket, "") {
            object_count += keys.len();
        }
    }

    (bucket_count, object_count)
}
