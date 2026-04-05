use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Response, StatusCode};

use super::HealStats;
use super::types::*;
use crate::config::Config;
use crate::heal::mrf::MrfQueue;
use crate::heal::worker::{HealResult, heal_object};
use crate::query::parse_query;
use crate::storage::bitrot::sha256_hex;
use crate::storage::Backend;
use crate::storage::Store;
use crate::storage::erasure_set::ErasureSet;

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
    store: Arc<ErasureSet>,
    heal_disks: Arc<Vec<Box<dyn Backend>>>,
    mrf: Arc<MrfQueue>,
    stats: Arc<HealStats>,
    config: AdminConfig,
}

/// Subset of server config needed by admin endpoints.
pub struct AdminConfig {
    pub listen: String,
    pub data_shards: usize,
    pub parity_shards: usize,
    pub total_disks: usize,
    pub auth_enabled: bool,
    pub scan_interval: String,
    pub heal_interval: String,
    pub mrf_workers: usize,
}

impl AdminConfig {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            listen: cfg.listen.clone(),
            data_shards: cfg.data,
            parity_shards: cfg.parity,
            total_disks: cfg.disks.len(),
            auth_enabled: !cfg.no_auth,
            scan_interval: cfg.scan_interval.clone(),
            heal_interval: cfg.heal_interval.clone(),
            mrf_workers: cfg.mrf_workers,
        }
    }
}

impl AdminHandler {
    pub fn new(
        store: Arc<ErasureSet>,
        heal_disks: Arc<Vec<Box<dyn Backend>>>,
        mrf: Arc<MrfQueue>,
        stats: Arc<HealStats>,
        config: AdminConfig,
    ) -> Self {
        Self {
            store,
            heal_disks,
            mrf,
            stats,
            config,
        }
    }

    pub fn dispatch(&self, path: &str, method: &hyper::Method, query: &str) -> Response<BoxBody> {
        match (path, method) {
            ("status", &hyper::Method::GET) => self.status(),
            ("disks", &hyper::Method::GET) => self.disks(),
            ("heal", &hyper::Method::GET) => self.heal_status(),
            ("heal", &hyper::Method::POST) => self.heal_object_handler(query),
            ("object", &hyper::Method::GET) => self.inspect_object(query),
            _ if path.starts_with("bucket/") && path.ends_with("/ec") => {
                let bucket = &path["bucket/".len()..path.len() - "/ec".len()];
                match *method {
                    hyper::Method::GET => self.get_bucket_ec(bucket),
                    hyper::Method::PUT => self.set_bucket_ec(bucket, query),
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
            data_shards: self.config.data_shards,
            parity_shards: self.config.parity_shards,
            total_disks: self.config.total_disks,
            listen: self.config.listen.clone(),
            auth_enabled: self.config.auth_enabled,
            scan_interval: self.config.scan_interval.clone(),
            heal_interval: self.config.heal_interval.clone(),
            mrf_workers: self.config.mrf_workers,
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
        let total = meta.erasure.data + meta.erasure.parity;
        let distribution = &meta.erasure.distribution;
        let mut shards = Vec::with_capacity(total);

        // build shard status
        for shard_idx in 0..total {
            let disk_idx = if shard_idx < distribution.len() {
                distribution[shard_idx]
            } else {
                shard_idx
            };

            let (status, checksum) = if disk_idx < all_reads.len() {
                if let Some((data, disk_meta)) = &all_reads[disk_idx] {
                    if disk_meta.quorum_eq(&meta) && sha256_hex(data) == disk_meta.checksum {
                        ("ok", Some(disk_meta.checksum.clone()))
                    } else {
                        ("corrupt", Some(disk_meta.checksum.clone()))
                    }
                } else {
                    ("missing", None)
                }
            } else {
                ("missing", None)
            };

            shards.push(ShardInfo {
                index: shard_idx,
                disk: disk_idx,
                status,
                checksum,
            });
        }

        json_response(&ObjectInspectResponse {
            bucket: bucket.to_string(),
            key: key.to_string(),
            size: meta.size,
            etag: meta.etag.clone(),
            content_type: meta.content_type.clone(),
            created_at: meta.created_at,
            erasure: ErasureInfo {
                data: meta.erasure.data,
                parity: meta.erasure.parity,
                distribution: meta.erasure.distribution.clone(),
            },
            shards,
        })
    }

    fn get_bucket_ec(&self, bucket: &str) -> Response<BoxBody> {
        match self.store.get_ec_config(bucket) {
            Ok(Some(config)) => json_response(&config),
            Ok(None) => json_response(&serde_json::json!({
                "data": self.config.data_shards,
                "parity": self.config.parity_shards,
                "source": "server_default"
            })),
            Err(e) => error_json(StatusCode::NOT_FOUND, &e.to_string()),
        }
    }

    fn set_bucket_ec(&self, bucket: &str, query: &str) -> Response<BoxBody> {
        let params = parse_query(query);
        let data: usize = match params.get("data").and_then(|v| v.parse().ok()) {
            Some(d) => d,
            None => return error_json(StatusCode::BAD_REQUEST, "missing data parameter"),
        };
        let parity: usize = match params.get("parity").and_then(|v| v.parse().ok()) {
            Some(p) => p,
            None => return error_json(StatusCode::BAD_REQUEST, "missing parity parameter"),
        };
        let config = crate::storage::metadata::EcConfig { data, parity };
        match self.store.set_ec_config(bucket, &config) {
            Ok(()) => json_response(&config),
            Err(e) => error_json(StatusCode::BAD_REQUEST, &e.to_string()),
        }
    }

    fn heal_object_handler(&self, query: &str) -> Response<BoxBody> {
        let params = parse_query(query);
        let bucket = match params.get("bucket") {
            Some(b) => b.clone(),
            None => return error_json(StatusCode::BAD_REQUEST, "missing bucket parameter"),
        };
        let key = match params.get("key") {
            Some(k) => k.clone(),
            None => return error_json(StatusCode::BAD_REQUEST, "missing key parameter"),
        };

        match heal_object(
            &self.heal_disks,
            self.config.data_shards,
            self.config.parity_shards,
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
