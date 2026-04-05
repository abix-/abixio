use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

pub mod placement;

const CLUSTER_STATE_FILE: &str = ".abixio.sys/cluster/state.json";
const CLUSTER_PROBE_HEADER: &str = "x-abixio-cluster-secret";

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub node_id: String,
    pub advertise_s3: String,
    pub advertise_cluster: String,
    pub peers: Vec<String>,
    pub cluster_secret: String,
    pub disk_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Joining,
    SyncingEpoch,
    Fenced,
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterSummary {
    pub enabled: bool,
    pub cluster_id: String,
    pub node_id: String,
    pub state: ServiceState,
    pub epoch_id: u64,
    pub leader_id: String,
    pub peer_count: usize,
    pub voter_count: usize,
    pub reachable_voters: usize,
    pub quorum: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fenced_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterNodeStatus {
    pub node_id: String,
    pub advertise_s3: String,
    pub advertise_cluster: String,
    pub state: ServiceState,
    pub voter: bool,
    pub reachable: bool,
    pub total_disks: usize,
    pub last_heartbeat_unix_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterDiskStatus {
    pub disk_id: String,
    pub node_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterEpoch {
    pub epoch_id: u64,
    pub leader_id: String,
    pub committed_at_unix_secs: u64,
    pub voter_count: usize,
    pub reachable_voters: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterTopology {
    pub cluster_id: String,
    pub epoch: ClusterEpoch,
    pub nodes: Vec<ClusterNodeStatus>,
    pub disks: Vec<ClusterDiskStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterStatusResponse {
    pub summary: ClusterSummary,
    pub topology: ClusterTopology,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedClusterState {
    summary: ClusterSummary,
    topology: ClusterTopology,
    epochs: Vec<ClusterEpoch>,
}

#[derive(Debug)]
pub struct ClusterManager {
    config: ClusterConfig,
    http: Client,
    state: RwLock<PersistedClusterState>,
}

#[derive(Debug, Deserialize)]
struct PeerProbeResponse {
    summary: ClusterSummary,
}

impl ClusterManager {
    pub fn new(config: ClusterConfig) -> Result<Self> {
        let config = ClusterConfig {
            node_id: normalize_id(&config.node_id),
            advertise_s3: normalize_endpoint(&config.advertise_s3),
            advertise_cluster: normalize_endpoint(&config.advertise_cluster),
            peers: normalize_peers(&config.peers),
            cluster_secret: config.cluster_secret,
            disk_paths: config.disk_paths,
        };
        let cluster_id = cluster_id_for(&config.node_id, &config.peers);
        let persisted = load_persisted_state(&config.disk_paths)?
            .filter(|state| state.summary.cluster_id == cluster_id)
            .unwrap_or_else(|| initial_state(&config, &cluster_id));
        let manager = Self {
            config,
            http: Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .context("build cluster HTTP client")?,
            state: RwLock::new(persisted),
        };
        manager.refresh_local_identity();
        if !manager.cluster_enabled() {
            manager.mark_ready(1, manager.config.node_id.clone(), 1, None);
        } else {
            manager.persist_state()?;
        }
        Ok(manager)
    }

    pub fn cluster_enabled(&self) -> bool {
        !self.config.peers.is_empty()
    }

    pub fn cluster_secret(&self) -> &str {
        &self.config.cluster_secret
    }

    pub fn local_node_id(&self) -> &str {
        &self.config.node_id
    }

    pub fn blocks_data_plane(&self) -> bool {
        self.summary().state != ServiceState::Ready
    }

    pub fn allows_mutation(&self) -> bool {
        self.summary().state == ServiceState::Ready
    }

    pub fn summary(&self) -> ClusterSummary {
        self.state.read().unwrap().summary.clone()
    }

    pub fn status_response(&self) -> ClusterStatusResponse {
        let guard = self.state.read().unwrap();
        ClusterStatusResponse {
            summary: guard.summary.clone(),
            topology: guard.topology.clone(),
        }
    }

    pub fn nodes(&self) -> Vec<ClusterNodeStatus> {
        self.state.read().unwrap().topology.nodes.clone()
    }

    pub fn topology(&self) -> ClusterTopology {
        self.state.read().unwrap().topology.clone()
    }

    pub fn epochs(&self) -> Vec<ClusterEpoch> {
        self.state.read().unwrap().epochs.clone()
    }

    pub fn force_fence(&self, reason: impl Into<String>) {
        let mut guard = self.state.write().unwrap();
        let reason = reason.into();
        guard.summary.state = ServiceState::Fenced;
        guard.summary.fenced_reason = Some(reason);
        if let Some(local) = guard
            .topology
            .nodes
            .iter_mut()
            .find(|node| node.node_id == self.config.node_id)
        {
            local.state = ServiceState::Fenced;
        }
        drop(guard);
        let _ = self.persist_state();
    }

    pub fn install_topology(
        &self,
        epoch_id: u64,
        leader_id: impl Into<String>,
        nodes: Vec<ClusterNodeStatus>,
        disks: Vec<ClusterDiskStatus>,
    ) {
        let leader_id = leader_id.into();
        let mut guard = self.state.write().unwrap();
        let committed_at = unix_secs();
        let voter_count = nodes.iter().filter(|node| node.voter).count().max(1);
        let reachable_voters = nodes
            .iter()
            .filter(|node| node.voter && node.reachable)
            .count()
            .max(1);
        let quorum = voter_count / 2 + 1;
        guard.summary.state = if reachable_voters >= quorum {
            ServiceState::Ready
        } else {
            ServiceState::Fenced
        };
        guard.summary.epoch_id = epoch_id;
        guard.summary.leader_id = leader_id.clone();
        guard.summary.peer_count = nodes.len().saturating_sub(1);
        guard.summary.voter_count = voter_count;
        guard.summary.reachable_voters = reachable_voters;
        guard.summary.quorum = quorum;
        guard.summary.fenced_reason = if reachable_voters >= quorum {
            None
        } else {
            Some(format!(
                "cluster quorum lost: reachable_voters={} quorum={}",
                reachable_voters, quorum
            ))
        };
        guard.topology.cluster_id = guard.summary.cluster_id.clone();
        guard.topology.epoch = ClusterEpoch {
            epoch_id,
            leader_id,
            committed_at_unix_secs: committed_at,
            voter_count,
            reachable_voters,
        };
        guard.topology.nodes = nodes;
        guard.topology.disks = disks;
        let current_epoch = guard.topology.epoch.clone();
        if guard.epochs.is_empty() {
            guard.epochs.push(current_epoch);
        } else {
            guard.epochs[0] = current_epoch;
        }
        drop(guard);
        let _ = self.persist_state();
    }

    pub fn spawn_peer_monitor(self: Arc<Self>, mut shutdown_rx: watch::Receiver<bool>) {
        tokio::spawn(async move {
            if !self.cluster_enabled() {
                return;
            }
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(err) = self.refresh_from_peers().await {
                            tracing::warn!("cluster peer refresh failed: {}", err);
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }

    async fn refresh_from_peers(&self) -> Result<()> {
        let peer_summaries = self.fetch_peer_summaries().await;
        let reachable_voters = 1 + peer_summaries.len();
        let voter_count = self.config.peers.len() + 1;
        let quorum = voter_count / 2 + 1;
        let leader_id = elect_leader(&self.config.node_id, &peer_summaries);
        if reachable_voters < quorum {
            self.mark_fenced(
                1,
                leader_id,
                reachable_voters,
                Some(format!(
                    "cluster quorum lost: reachable_voters={} quorum={}",
                    reachable_voters, quorum
                )),
            );
            return Ok(());
        }

        self.mark_ready(1, leader_id, reachable_voters, Some(peer_summaries));
        Ok(())
    }

    async fn fetch_peer_summaries(&self) -> Vec<(String, ClusterSummary)> {
        let mut summaries = Vec::new();
        for peer in &self.config.peers {
            let url = format!("{}/_admin/cluster/status", peer.trim_end_matches('/'));
            let mut req = self.http.get(&url);
            if !self.config.cluster_secret.is_empty() {
                req = req.header(CLUSTER_PROBE_HEADER, &self.config.cluster_secret);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(peer_status) = resp.json::<PeerProbeResponse>().await {
                        if peer_status.summary.cluster_id == self.summary().cluster_id {
                            summaries.push((peer.clone(), peer_status.summary));
                        }
                    }
                }
                Ok(_) | Err(_) => {}
            }
        }
        summaries
    }

    fn mark_ready(
        &self,
        epoch_id: u64,
        leader_id: String,
        reachable_voters: usize,
        peer_summaries: Option<Vec<(String, ClusterSummary)>>,
    ) {
        let mut guard = self.state.write().unwrap();
        let committed_at = unix_secs();
        let voter_count = self.config.peers.len() + 1;
        let quorum = voter_count / 2 + 1;
        guard.summary.state = ServiceState::Ready;
        guard.summary.epoch_id = epoch_id;
        guard.summary.leader_id = leader_id.clone();
        guard.summary.voter_count = voter_count;
        guard.summary.reachable_voters = reachable_voters;
        guard.summary.quorum = quorum;
        guard.summary.fenced_reason = None;

        guard.topology.epoch = ClusterEpoch {
            epoch_id,
            leader_id: leader_id.clone(),
            committed_at_unix_secs: committed_at,
            voter_count,
            reachable_voters,
        };
        let epoch = guard.topology.epoch.clone();
        if guard.epochs.is_empty() {
            guard.epochs.push(epoch);
        } else {
            guard.epochs[0] = epoch;
        }
        guard.topology.nodes = self.build_nodes(ServiceState::Ready, peer_summaries);
        if let Some(local) = guard
            .topology
            .nodes
            .iter_mut()
            .find(|node| node.node_id == self.config.node_id)
        {
            local.state = ServiceState::Ready;
            local.reachable = true;
            local.last_heartbeat_unix_secs = committed_at;
        }
        drop(guard);
        let _ = self.persist_state();
    }

    fn mark_fenced(
        &self,
        epoch_id: u64,
        leader_id: String,
        reachable_voters: usize,
        reason: Option<String>,
    ) {
        let mut guard = self.state.write().unwrap();
        let voter_count = self.config.peers.len() + 1;
        let quorum = voter_count / 2 + 1;
        guard.summary.state = ServiceState::Fenced;
        guard.summary.epoch_id = epoch_id;
        guard.summary.leader_id = leader_id;
        guard.summary.voter_count = voter_count;
        guard.summary.reachable_voters = reachable_voters;
        guard.summary.quorum = quorum;
        guard.summary.fenced_reason = reason.clone();
        guard.topology.nodes = self.build_nodes(ServiceState::Fenced, None);
        if let Some(local) = guard
            .topology
            .nodes
            .iter_mut()
            .find(|node| node.node_id == self.config.node_id)
        {
            local.state = ServiceState::Fenced;
            local.reachable = true;
            local.last_heartbeat_unix_secs = unix_secs();
        }
        drop(guard);
        let _ = self.persist_state();
    }

    fn build_nodes(
        &self,
        local_state: ServiceState,
        peer_summaries: Option<Vec<(String, ClusterSummary)>>,
    ) -> Vec<ClusterNodeStatus> {
        let mut nodes = vec![ClusterNodeStatus {
            node_id: self.config.node_id.clone(),
            advertise_s3: self.config.advertise_s3.clone(),
            advertise_cluster: self.config.advertise_cluster.clone(),
            state: local_state,
            voter: true,
            reachable: true,
            total_disks: self.config.disk_paths.len(),
            last_heartbeat_unix_secs: unix_secs(),
        }];

        let peers = peer_summaries.unwrap_or_default();
        for peer in &self.config.peers {
            let status = peers
                .iter()
                .find(|(endpoint, _)| endpoint == peer)
                .map(|(_, summary)| summary);
            nodes.push(ClusterNodeStatus {
                node_id: status
                    .map(|summary| summary.node_id.clone())
                    .unwrap_or_else(|| peer.clone()),
                advertise_s3: status
                    .map(|summary| summary.node_id.clone())
                    .unwrap_or_else(|| String::new()),
                advertise_cluster: peer.clone(),
                state: status
                    .map(|summary| summary.state)
                    .unwrap_or(ServiceState::Joining),
                voter: true,
                reachable: status.is_some(),
                total_disks: 0,
                last_heartbeat_unix_secs: unix_secs(),
            });
        }
        nodes
    }

    fn refresh_local_identity(&self) {
        let mut guard = self.state.write().unwrap();
        guard.summary.node_id = self.config.node_id.clone();
        guard.summary.cluster_id = cluster_id_for(&self.config.node_id, &self.config.peers);
        guard.summary.peer_count = self.config.peers.len();
        guard.topology.cluster_id = guard.summary.cluster_id.clone();
        guard.topology.disks = self
            .config
            .disk_paths
            .iter()
            .map(|path| ClusterDiskStatus {
                disk_id: disk_id_for(&self.config.node_id, path),
                node_id: self.config.node_id.clone(),
                path: path.display().to_string(),
            })
            .collect();
    }

    fn persist_state(&self) -> Result<()> {
        let guard = self.state.read().unwrap();
        let bytes = serde_json::to_vec_pretty(&*guard).context("serialize cluster state")?;
        for disk in &self.config.disk_paths {
            let path = disk.join(CLUSTER_STATE_FILE);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create cluster state dir {}", parent.display()))?;
            }
            fs::write(&path, &bytes)
                .with_context(|| format!("write cluster state {}", path.display()))?;
        }
        Ok(())
    }
}

fn load_persisted_state(disks: &[PathBuf]) -> Result<Option<PersistedClusterState>> {
    for disk in disks {
        let path = disk.join(CLUSTER_STATE_FILE);
        if !path.exists() {
            continue;
        }
        let bytes = fs::read(&path)
            .with_context(|| format!("read persisted cluster state {}", path.display()))?;
        let state = serde_json::from_slice::<PersistedClusterState>(&bytes)
            .with_context(|| format!("parse persisted cluster state {}", path.display()))?;
        return Ok(Some(state));
    }
    Ok(None)
}

fn initial_state(config: &ClusterConfig, cluster_id: &str) -> PersistedClusterState {
    let committed_at = unix_secs();
    let summary = ClusterSummary {
        enabled: !config.peers.is_empty(),
        cluster_id: cluster_id.to_string(),
        node_id: config.node_id.clone(),
        state: if config.peers.is_empty() {
            ServiceState::Ready
        } else {
            ServiceState::SyncingEpoch
        },
        epoch_id: 1,
        leader_id: config.node_id.clone(),
        peer_count: config.peers.len(),
        voter_count: config.peers.len() + 1,
        reachable_voters: 1,
        quorum: (config.peers.len() + 1) / 2 + 1,
        fenced_reason: None,
    };
    let epoch = ClusterEpoch {
        epoch_id: 1,
        leader_id: config.node_id.clone(),
        committed_at_unix_secs: committed_at,
        voter_count: config.peers.len() + 1,
        reachable_voters: 1,
    };
    let topology = ClusterTopology {
        cluster_id: cluster_id.to_string(),
        epoch: epoch.clone(),
        nodes: vec![ClusterNodeStatus {
            node_id: config.node_id.clone(),
            advertise_s3: config.advertise_s3.clone(),
            advertise_cluster: config.advertise_cluster.clone(),
            state: summary.state,
            voter: true,
            reachable: true,
            total_disks: config.disk_paths.len(),
            last_heartbeat_unix_secs: committed_at,
        }],
        disks: config
            .disk_paths
            .iter()
            .map(|path| ClusterDiskStatus {
                disk_id: disk_id_for(&config.node_id, path),
                node_id: config.node_id.clone(),
                path: path.display().to_string(),
            })
            .collect(),
    };
    PersistedClusterState {
        summary,
        topology,
        epochs: vec![epoch],
    }
}

fn elect_leader(local_node_id: &str, peer_summaries: &[(String, ClusterSummary)]) -> String {
    let mut ids = BTreeSet::new();
    ids.insert(local_node_id.to_string());
    for (_, summary) in peer_summaries {
        ids.insert(summary.node_id.clone());
    }
    ids.into_iter()
        .next()
        .unwrap_or_else(|| local_node_id.to_string())
}

fn cluster_id_for(node_id: &str, peers: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(node_id.as_bytes());
    for peer in peers {
        hasher.update(peer.as_bytes());
    }
    let digest = hasher.finalize();
    format!("abixio-{}", hex::encode(&digest[..8]))
}

fn disk_id_for(node_id: &str, path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(node_id.as_bytes());
    hasher.update(path.display().to_string().as_bytes());
    let digest = hasher.finalize();
    format!("disk-{}", hex::encode(&digest[..8]))
}

fn normalize_peers(peers: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for peer in peers {
        let value = normalize_endpoint(peer);
        if seen.insert(value.clone()) {
            normalized.push(value);
        }
    }
    normalized
}

fn normalize_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/').to_string();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed
    } else if trimmed.is_empty() {
        trimmed
    } else {
        format!("http://{}", trimmed)
    }
}

fn normalize_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "local".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn cluster_probe_header() -> &'static str {
    CLUSTER_PROBE_HEADER
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use hyper::body::{Bytes, Incoming};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    fn test_disk() -> (TempDir, PathBuf) {
        let base = TempDir::new().unwrap();
        let disk = base.path().join("d1");
        fs::create_dir_all(&disk).unwrap();
        (base, disk)
    }

    fn test_config(disk: PathBuf) -> ClusterConfig {
        ClusterConfig {
            node_id: "node-a".to_string(),
            advertise_s3: "127.0.0.1:9000".to_string(),
            advertise_cluster: "127.0.0.1:9000".to_string(),
            peers: Vec::new(),
            cluster_secret: String::new(),
            disk_paths: vec![disk],
        }
    }

    async fn start_mock_peer(
        summary: Arc<RwLock<ClusterSummary>>,
        expected_secret: Option<String>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let io = TokioIo::new(stream);
                let summary = Arc::clone(&summary);
                let expected_secret = expected_secret.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let summary = Arc::clone(&summary);
                        let expected_secret = expected_secret.clone();
                        async move {
                            if req.uri().path() != "/_admin/cluster/status" {
                                return Ok::<_, hyper::Error>(
                                    Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Full::new(Bytes::new()))
                                        .unwrap(),
                                );
                            }
                            if let Some(secret) = expected_secret.as_ref() {
                                let got = req
                                    .headers()
                                    .get(cluster_probe_header())
                                    .and_then(|value| value.to_str().ok());
                                if got != Some(secret.as_str()) {
                                    return Ok::<_, hyper::Error>(
                                        Response::builder()
                                            .status(StatusCode::FORBIDDEN)
                                            .body(Full::new(Bytes::new()))
                                            .unwrap(),
                                    );
                                }
                            }
                            let body = serde_json::to_vec(&serde_json::json!({
                                "summary": *summary.read().unwrap()
                            }))
                            .unwrap();
                            Ok::<_, hyper::Error>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("Content-Type", "application/json")
                                    .body(Full::new(Bytes::from(body)))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });
        (format!("http://{}", addr), handle)
    }

    #[test]
    fn standalone_cluster_starts_ready() {
        let (_base, disk) = test_disk();
        let manager = ClusterManager::new(test_config(disk)).unwrap();
        assert_eq!(manager.summary().state, ServiceState::Ready);
        assert!(!manager.blocks_data_plane());
    }

    #[test]
    fn force_fence_blocks_data_plane() {
        let (_base, disk) = test_disk();
        let manager = ClusterManager::new(test_config(disk)).unwrap();
        manager.force_fence("test fence");
        let summary = manager.summary();
        assert_eq!(summary.state, ServiceState::Fenced);
        assert_eq!(summary.fenced_reason.as_deref(), Some("test fence"));
        assert!(manager.blocks_data_plane());
    }

    #[test]
    fn clustered_config_starts_syncing_and_blocked() {
        let (_base, disk) = test_disk();
        let mut cfg = test_config(disk);
        cfg.peers = vec!["node-b:9000".to_string(), "node-c:9000".to_string()];
        let manager = ClusterManager::new(cfg).unwrap();
        let summary = manager.summary();
        assert_eq!(summary.state, ServiceState::SyncingEpoch);
        assert!(summary.enabled);
        assert!(manager.blocks_data_plane());
    }

    #[test]
    fn normalize_peers_deduplicates_and_adds_scheme() {
        let peers = normalize_peers(&[
            "node-b:9000".to_string(),
            "http://node-b:9000/".to_string(),
            "node-c:9000".to_string(),
        ]);
        assert_eq!(
            peers,
            vec![
                "http://node-b:9000".to_string(),
                "http://node-c:9000".to_string()
            ]
        );
    }

    #[test]
    fn persisted_state_survives_restart() {
        let (_base, disk) = test_disk();
        let mut cfg = test_config(disk.clone());
        cfg.peers = vec!["http://127.0.0.1:65011".to_string()];
        let manager = ClusterManager::new(cfg.clone()).unwrap();
        manager.force_fence("persisted fence");

        let reloaded = ClusterManager::new(cfg).unwrap();
        let summary = reloaded.summary();
        assert_eq!(summary.state, ServiceState::Fenced);
        assert_eq!(summary.fenced_reason.as_deref(), Some("persisted fence"));
    }

    #[tokio::test]
    async fn quorum_met_transitions_cluster_to_ready() {
        let (_base, disk) = test_disk();
        let peer_summary = Arc::new(RwLock::new(ClusterSummary {
            enabled: true,
            cluster_id: String::new(),
            node_id: "node-b".to_string(),
            state: ServiceState::Ready,
            epoch_id: 1,
            leader_id: "node-a".to_string(),
            peer_count: 1,
            voter_count: 2,
            reachable_voters: 2,
            quorum: 2,
            fenced_reason: None,
        }));
        let (peer, handle) = start_mock_peer(Arc::clone(&peer_summary), None).await;
        peer_summary.write().unwrap().cluster_id =
            cluster_id_for("node-a", std::slice::from_ref(&peer));

        let mut cfg = test_config(disk);
        cfg.peers = vec![peer];
        let manager = ClusterManager::new(cfg).unwrap();
        manager.refresh_from_peers().await.unwrap();

        let summary = manager.summary();
        assert_eq!(summary.state, ServiceState::Ready);
        assert_eq!(summary.reachable_voters, 2);
        assert!(!manager.blocks_data_plane());
        handle.abort();
    }

    #[tokio::test]
    async fn quorum_loss_fences_cluster() {
        let (_base, disk) = test_disk();
        let mut cfg = test_config(disk);
        cfg.peers = vec![
            "http://127.0.0.1:65001".to_string(),
            "http://127.0.0.1:65002".to_string(),
        ];
        let manager = ClusterManager::new(cfg).unwrap();
        manager.refresh_from_peers().await.unwrap();

        let summary = manager.summary();
        assert_eq!(summary.state, ServiceState::Fenced);
        assert_eq!(summary.reachable_voters, 1);
        assert!(summary.fenced_reason.unwrap().contains("quorum lost"));
        assert!(manager.blocks_data_plane());
    }

    #[tokio::test]
    async fn secret_mismatch_counts_peer_as_unreachable() {
        let (_base, disk) = test_disk();
        let peer_summary = Arc::new(RwLock::new(ClusterSummary {
            enabled: true,
            cluster_id: String::new(),
            node_id: "node-b".to_string(),
            state: ServiceState::Ready,
            epoch_id: 1,
            leader_id: "node-a".to_string(),
            peer_count: 1,
            voter_count: 2,
            reachable_voters: 2,
            quorum: 2,
            fenced_reason: None,
        }));
        let (peer, handle) = start_mock_peer(
            Arc::clone(&peer_summary),
            Some("expected-secret".to_string()),
        )
        .await;
        peer_summary.write().unwrap().cluster_id =
            cluster_id_for("node-a", std::slice::from_ref(&peer));

        let mut cfg = test_config(disk);
        cfg.peers = vec![peer];
        cfg.cluster_secret = "wrong-secret".to_string();
        let manager = ClusterManager::new(cfg).unwrap();
        manager.refresh_from_peers().await.unwrap();

        let summary = manager.summary();
        assert_eq!(summary.state, ServiceState::Fenced);
        assert_eq!(summary.reachable_voters, 1);
        handle.abort();
    }

    #[tokio::test]
    async fn cluster_recovers_from_fence_when_peer_returns() {
        let (_base, disk) = test_disk();
        let peer_summary = Arc::new(RwLock::new(ClusterSummary {
            enabled: true,
            cluster_id: String::new(),
            node_id: "node-b".to_string(),
            state: ServiceState::Ready,
            epoch_id: 1,
                leader_id: "node-a".to_string(),
                peer_count: 1,
                voter_count: 2,
            reachable_voters: 2,
            quorum: 2,
            fenced_reason: None,
        }));
        let (peer, handle) = start_mock_peer(Arc::clone(&peer_summary), None).await;
        peer_summary.write().unwrap().cluster_id =
            cluster_id_for("node-a", std::slice::from_ref(&peer));

        let mut cfg = test_config(disk);
        cfg.peers = vec![peer];
        let manager = ClusterManager::new(cfg).unwrap();
        manager.force_fence("temporary fence");
        assert_eq!(manager.summary().state, ServiceState::Fenced);

        manager.refresh_from_peers().await.unwrap();
        let summary = manager.summary();
        assert_eq!(summary.state, ServiceState::Ready);
        assert_eq!(summary.fenced_reason, None);
        handle.abort();
    }
}
