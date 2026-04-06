use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementVolume {
    pub backend_index: usize,
    pub node_id: String,
    pub volume_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementShard {
    pub index: usize,
    pub backend_index: usize,
    pub node_id: String,
    pub volume_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementRecord {
    pub epoch_id: u64,
    pub cluster_id: String,
    pub data: usize,
    pub parity: usize,
    pub shards: Vec<PlacementShard>,
}

#[derive(Debug, Clone)]
pub struct PlacementPlanner {
    epoch_id: u64,
    cluster_id: String,
    disks: Vec<PlacementVolume>,
}

impl PlacementPlanner {
    pub fn new(epoch_id: u64, cluster_id: impl Into<String>, disks: Vec<PlacementVolume>) -> Self {
        Self {
            epoch_id,
            cluster_id: cluster_id.into(),
            disks,
        }
    }

    pub fn disks(&self) -> &[PlacementVolume] {
        &self.disks
    }

    pub fn plan(
        &self,
        bucket: &str,
        key: &str,
        data: usize,
        parity: usize,
    ) -> Result<PlacementRecord, String> {
        if data == 0 {
            return Err("data shards must be >= 1".to_string());
        }
        let total = data + parity;
        if total == 0 {
            return Err("at least one shard is required".to_string());
        }
        if total > self.disks.len() {
            return Err(format!(
                "need {} disks for placement, only {} available",
                total,
                self.disks.len()
            ));
        }

        let key_material = format!(
            "{}:{}:{}/{}",
            self.epoch_id, self.cluster_id, bucket, key
        );
        let mut per_node: BTreeMap<&str, Vec<&PlacementVolume>> = BTreeMap::new();
        for disk in &self.disks {
            per_node.entry(&disk.node_id).or_default().push(disk);
        }

        let ordered_nodes = stable_order(
            per_node.keys().copied().collect::<Vec<_>>(),
            |node_id| format!("node:{}:{}", key_material, node_id),
        );

        let mut ordered_disks: BTreeMap<&str, Vec<&PlacementVolume>> = BTreeMap::new();
        for (node_id, disks) in per_node {
            ordered_disks.insert(
                node_id,
                stable_order(disks, |disk| format!("disk:{}:{}", key_material, disk.volume_id)),
            );
        }

        let mut chosen = Vec::with_capacity(total);
        let mut round = 0usize;
        while chosen.len() < total {
            let mut placed_this_round = false;
            for node_id in &ordered_nodes {
                let Some(disks) = ordered_disks.get(node_id) else {
                    continue;
                };
                if round < disks.len() {
                    chosen.push(disks[round]);
                    placed_this_round = true;
                    if chosen.len() == total {
                        break;
                    }
                }
            }
            if !placed_this_round {
                break;
            }
            round += 1;
        }

        if chosen.len() != total {
            return Err(format!(
                "unable to place {} shards across {} disks",
                total,
                self.disks.len()
            ));
        }

        Ok(PlacementRecord {
            epoch_id: self.epoch_id,
            cluster_id: self.cluster_id.clone(),
            data,
            parity,
            shards: chosen
                .into_iter()
                .enumerate()
                .map(|(index, disk)| PlacementShard {
                    index,
                    backend_index: disk.backend_index,
                    node_id: disk.node_id.clone(),
                    volume_id: disk.volume_id.clone(),
                })
                .collect(),
        })
    }
}

fn stable_order<T, F>(mut items: Vec<T>, mut salt_for: F) -> Vec<T>
where
    T: Clone,
    F: FnMut(&T) -> String,
{
    items.sort_by_key(|item| hash_hex(&salt_for(item)));
    items
}

fn hash_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn planner() -> PlacementPlanner {
        let mut disks = Vec::new();
        for node in 0..4 {
            for disk in 0..2 {
                let backend_index = node * 2 + disk;
                disks.push(PlacementVolume {
                    backend_index,
                    node_id: format!("node-{}", node + 1),
                    volume_id: format!("node-{}-disk-{}", node + 1, disk + 1),
                });
            }
        }
        PlacementPlanner::new(7, "set-a", disks)
    }

    #[test]
    fn planner_is_deterministic() {
        let planner = planner();
        let a = planner.plan("bucket", "photos/cat.jpg", 2, 2).unwrap();
        let b = planner.plan("bucket", "photos/cat.jpg", 2, 2).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn planner_uses_each_node_before_reusing_a_node() {
        let planner = planner();
        for data in 1..=8 {
            let record = planner.plan("bucket", "alpha", data, 0).unwrap();
            let nodes = record
                .shards
                .iter()
                .map(|shard| shard.node_id.clone())
                .collect::<Vec<_>>();
            let distinct = nodes.iter().collect::<BTreeSet<_>>().len();
            assert_eq!(distinct, data.min(4));
            if data <= 4 {
                assert_eq!(distinct, data);
            }
        }
    }

    #[test]
    fn planner_never_duplicates_a_disk() {
        let planner = planner();
        for data in 1..=8 {
            for parity in 0..=(8 - data) {
                let record = planner.plan("bucket", "object", data, parity).unwrap();
                let disks = record
                    .shards
                    .iter()
                    .map(|shard| shard.volume_id.clone())
                    .collect::<Vec<_>>();
                let distinct = disks.iter().collect::<BTreeSet<_>>().len();
                assert_eq!(distinct, data + parity);
            }
        }
    }

    #[test]
    fn planner_differs_across_keys() {
        let planner = planner();
        let a = planner.plan("bucket", "alpha", 2, 2).unwrap();
        let b = planner.plan("bucket", "beta", 2, 2).unwrap();
        assert_ne!(a.shards, b.shards);
    }

    #[test]
    fn planner_balance_is_reasonable_across_many_keys() {
        let planner = planner();
        let mut per_node: BTreeMap<String, usize> = BTreeMap::new();
        let mut per_disk: BTreeMap<String, usize> = BTreeMap::new();

        for key_index in 0..10_000 {
            let key = format!("object-{key_index:05}");
            let record = planner.plan("bucket", &key, 2, 2).unwrap();
            for shard in record.shards {
                *per_node.entry(shard.node_id).or_default() += 1;
                *per_disk.entry(shard.volume_id).or_default() += 1;
            }
        }

        let node_mean = 10_000usize;
        for count in per_node.values() {
            let delta = count.abs_diff(node_mean);
            assert!(delta <= 1_000, "node count {} outside 10% band", count);
        }

        let disk_mean = 5_000usize;
        for count in per_disk.values() {
            let delta = count.abs_diff(disk_mean);
            assert!(delta <= 750, "disk count {} outside 15% band", count);
        }
    }

    #[test]
    fn planner_rejects_zero_data() {
        let planner = planner();
        assert!(planner.plan("bucket", "alpha", 0, 1).is_err());
    }
}
