use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cluster::{ClusterVolumeStatus, ClusterNodeStatus, ServiceState};
use crate::cluster::placement::PlacementVolume;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticTopology {
    pub cluster_id: String,
    pub epoch_id: u64,
    pub pool_id: String,
    pub nodes: Vec<TopologyNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyNode {
    pub node_id: String,
    pub advertise_s3: String,
    pub advertise_cluster: String,
    pub volumes: Vec<TopologyVolume>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyVolume {
    pub volume_id: String,
    pub path: PathBuf,
}

impl StaticTopology {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("read cluster topology {}", path.display()))?;
        let topology = serde_json::from_slice::<Self>(&bytes)
            .with_context(|| format!("parse cluster topology {}", path.display()))?;
        topology.validate()?;
        Ok(topology)
    }

    pub fn validate(&self) -> Result<()> {
        if self.cluster_id.trim().is_empty() {
            bail!("cluster_id must not be empty");
        }
        if self.epoch_id == 0 {
            bail!("epoch_id must be >= 1");
        }
        if self.pool_id.trim().is_empty() {
            bail!("pool_id must not be empty");
        }
        if self.nodes.is_empty() {
            bail!("topology must contain at least one node");
        }

        let mut node_ids = BTreeSet::new();
        let mut volume_ids = BTreeSet::new();
        let mut all_paths = BTreeSet::new();
        for node in &self.nodes {
            if node.node_id.trim().is_empty() {
                bail!("node_id must not be empty");
            }
            if !node_ids.insert(node.node_id.clone()) {
                bail!("duplicate node_id {}", node.node_id);
            }
            if node.volumes.is_empty() {
                bail!("node {} must define at least one disk", node.node_id);
            }
            for disk in &node.volumes {
                if disk.volume_id.trim().is_empty() {
                    bail!("volume_id must not be empty");
                }
                if !volume_ids.insert(disk.volume_id.clone()) {
                    bail!("duplicate volume_id {}", disk.volume_id);
                }
                let path = normalize_path(&disk.path);
                if !all_paths.insert(path.clone()) {
                    bail!("duplicate disk path {}", path);
                }
            }
        }
        Ok(())
    }

    pub fn topology_hash(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self).context("serialize topology for hashing")?;
        let digest = Sha256::digest(&bytes);
        Ok(hex::encode(digest))
    }

    pub fn node(&self, node_id: &str) -> Option<&TopologyNode> {
        self.nodes.iter().find(|node| node.node_id == node_id)
    }

    pub fn peers_for(&self, node_id: &str) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| node.node_id != node_id)
            .map(|node| node.advertise_cluster.clone())
            .collect()
    }

    pub fn validate_local_node(&self, node_id: &str, disk_paths: &[PathBuf]) -> Result<()> {
        let node = self
            .node(node_id)
            .with_context(|| format!("node_id {} missing from topology", node_id))?;
        let expected = node
            .volumes
            .iter()
            .map(|disk| normalize_path(&disk.path))
            .collect::<BTreeSet<_>>();
        let actual = disk_paths
            .iter()
            .map(|path| normalize_path(path))
            .collect::<BTreeSet<_>>();
        if expected != actual {
            bail!(
                "local disks do not match topology for {}: expected {:?}, got {:?}",
                node_id,
                expected,
                actual
            );
        }
        Ok(())
    }

    pub fn placement_volumes(&self) -> Vec<PlacementVolume> {
        let mut disks = Vec::new();
        for node in &self.nodes {
            for disk in &node.volumes {
                disks.push(PlacementVolume {
                    backend_index: disks.len(),
                    node_id: node.node_id.clone(),
                    volume_id: disk.volume_id.clone(),
                });
            }
        }
        disks
    }

    pub fn volume_statuses(&self) -> Vec<ClusterVolumeStatus> {
        let mut disks = Vec::new();
        for node in &self.nodes {
            for disk in &node.volumes {
                disks.push(ClusterVolumeStatus {
                    volume_id: disk.volume_id.clone(),
                    node_id: node.node_id.clone(),
                    path: disk.path.display().to_string(),
                });
            }
        }
        disks
    }

    pub fn node_statuses(&self, local_state: ServiceState, local_node_id: &str) -> Vec<ClusterNodeStatus> {
        self.nodes
            .iter()
            .map(|node| ClusterNodeStatus {
                node_id: node.node_id.clone(),
                advertise_s3: node.advertise_s3.clone(),
                advertise_cluster: node.advertise_cluster.clone(),
                state: if node.node_id == local_node_id {
                    local_state
                } else {
                    ServiceState::Joining
                },
                voter: true,
                reachable: node.node_id == local_node_id,
                total_disks: node.volumes.len(),
                last_heartbeat_unix_secs: 0,
            })
            .collect()
    }
}

fn normalize_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_topology() -> StaticTopology {
        StaticTopology {
            cluster_id: "cluster-a".to_string(),
            epoch_id: 7,
            pool_id: "set-a".to_string(),
            nodes: vec![
                TopologyNode {
                    node_id: "node-1".to_string(),
                    advertise_s3: "http://node-1:9000".to_string(),
                    advertise_cluster: "http://node-1:9000".to_string(),
                    volumes: vec![
                        TopologyVolume {
                            volume_id: "vol-a".to_string(),
                            path: PathBuf::from("C:/data/d1"),
                        },
                        TopologyVolume {
                            volume_id: "vol-b".to_string(),
                            path: PathBuf::from("C:/data/d2"),
                        },
                    ],
                },
                TopologyNode {
                    node_id: "node-2".to_string(),
                    advertise_s3: "http://node-2:9000".to_string(),
                    advertise_cluster: "http://node-2:9000".to_string(),
                    volumes: vec![TopologyVolume {
                        volume_id: "vol-c".to_string(),
                        path: PathBuf::from("C:/data/d3"),
                    }],
                },
            ],
        }
    }

    #[test]
    fn topology_hash_is_deterministic() {
        let topology = sample_topology();
        assert_eq!(topology.topology_hash().unwrap(), topology.topology_hash().unwrap());
    }

    #[test]
    fn topology_validation_rejects_duplicate_nodes() {
        let mut topology = sample_topology();
        topology.nodes[1].node_id = "node-1".to_string();
        assert!(topology.validate().is_err());
    }

    #[test]
    fn topology_validation_rejects_duplicate_disks() {
        let mut topology = sample_topology();
        topology.nodes[1].volumes[0].volume_id = "vol-a".to_string();
        assert!(topology.validate().is_err());
    }

    #[test]
    fn validate_local_node_checks_disk_paths() {
        let topology = sample_topology();
        assert!(topology
            .validate_local_node(
                "node-1",
                &[PathBuf::from("C:/data/d1"), PathBuf::from("C:/data/d2")]
            )
            .is_ok());
        assert!(topology
            .validate_local_node("node-1", &[PathBuf::from("C:/data/d1")])
            .is_err());
    }
}
