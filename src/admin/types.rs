use serde::Serialize;

use crate::cluster::{ClusterEpoch, ClusterNodeStatus, ClusterSummary, ClusterTopology};

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub server: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
    pub default_ftt: usize,
    pub total_disks: usize,
    pub listen: String,
    pub auth_enabled: bool,
    pub scan_interval: String,
    pub heal_interval: String,
    pub mrf_workers: usize,
    pub cluster: ClusterSummary,
}

#[derive(Debug, Serialize)]
pub struct DisksResponse {
    pub disks: Vec<DiskInfo>,
}

#[derive(Debug, Serialize)]
pub struct DiskInfo {
    pub index: usize,
    pub path: String,
    pub online: bool,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub bucket_count: usize,
    pub object_count: usize,
}

#[derive(Debug, Serialize)]
pub struct HealStatusResponse {
    pub mrf_pending: usize,
    pub mrf_workers: usize,
    pub scanner: ScannerStatus,
}

#[derive(Debug, Serialize)]
pub struct ScannerStatus {
    pub running: bool,
    pub scan_interval: String,
    pub heal_interval: String,
    pub objects_scanned: u64,
    pub objects_healed: u64,
    pub last_scan_started: u64,
    pub last_scan_duration_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct ObjectInspectResponse {
    pub bucket: String,
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub created_at: u64,
    pub erasure: ErasureInfo,
    pub shards: Vec<ShardInfo>,
}

#[derive(Debug, Serialize)]
pub struct ErasureInfo {
    pub data: usize,
    pub parity: usize,
    pub epoch_id: u64,
    pub set_id: String,
    pub distribution: Vec<usize>,
    pub node_ids: Vec<String>,
    pub volume_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ShardInfo {
    pub index: usize,
    pub disk: usize,
    pub node_id: String,
    pub volume_id: String,
    pub status: &'static str,
    pub checksum: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HealResponse {
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shards_fixed: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClusterNodesResponse {
    pub nodes: Vec<ClusterNodeStatus>,
}

#[derive(Debug, Serialize)]
pub struct ClusterEpochsResponse {
    pub epochs: Vec<ClusterEpoch>,
}

#[derive(Debug, Serialize)]
pub struct ClusterTopologyResponse {
    pub topology: ClusterTopology,
}
