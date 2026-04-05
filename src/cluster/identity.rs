use std::path::PathBuf;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage::volume::{
    SetMember, finalize_volumes, format_volumes, load_volumes,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeIdentity {
    pub node_id: String,
    pub advertise: String,
    pub volumes: Vec<VolumeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VolumeEntry {
    pub volume_id: String,
    pub index: u32,
    pub path: String,
}

/// Resolved cluster identity after boot + node exchange.
#[derive(Debug, Clone)]
pub struct ResolvedIdentity {
    pub node_id: String,
    pub deployment_id: String,
    pub set_id: String,
    pub advertise: String,
    pub nodes: Vec<String>,
    pub disk_paths: Vec<PathBuf>,
    pub all_members: Vec<SetMember>,
    /// node_id -> (advertise_endpoint, vec of volume paths)
    pub node_volumes: Vec<NodeVolumes>,
}

#[derive(Debug, Clone)]
pub struct NodeVolumes {
    pub node_id: String,
    pub endpoint: String,
    pub volume_paths: Vec<String>,
}

/// Boot sequence: load or format volumes, then resolve identity.
pub async fn resolve_identity(
    disk_paths: &[PathBuf],
    listen: &str,
    nodes: &[String],
) -> Result<ResolvedIdentity, String> {
    // step 1: load or format volumes
    let mut formats = match load_volumes(disk_paths)? {
        Some(f) => f,
        None => format_volumes(disk_paths)?,
    };

    let node_id = formats[0].node_id.clone();
    let advertise = derive_advertise(listen);

    let local_identity = NodeIdentity {
        node_id: node_id.clone(),
        advertise: advertise.clone(),
        volumes: formats
            .iter()
            .enumerate()
            .map(|(i, f)| VolumeEntry {
                volume_id: f.volume_id.clone(),
                index: f.volume_index,
                path: disk_paths[i].display().to_string(),
            })
            .collect(),
    };

    // step 2: standalone or cluster?
    // nodes list includes self -- filter to just remote nodes
    let remote_nodes: Vec<&String> = nodes
        .iter()
        .filter(|n| {
            let normalized = n.trim_end_matches('/');
            normalized != advertise.trim_end_matches('/')
        })
        .collect();

    if nodes.is_empty() {
        // standalone: finalize immediately
        let deployment_id = deployment_id_for(&[node_id.clone()]);
        let set_id = uuid::Uuid::new_v4().to_string();
        let members = build_members(&[local_identity.clone()]);
        finalize_volumes(disk_paths, &mut formats, &deployment_id, &set_id, members.clone())?;
        let node_volumes = vec![NodeVolumes {
            node_id: node_id.clone(),
            endpoint: advertise.clone(),
            volume_paths: disk_paths.iter().map(|p| p.display().to_string()).collect(),
        }];
        return Ok(ResolvedIdentity {
            node_id,
            deployment_id,
            set_id,
            advertise,
            nodes: Vec::new(),
            disk_paths: disk_paths.to_vec(),
            all_members: members,
            node_volumes,
        });
    }

    // step 3: cluster -- already finalized from previous boot?
    if formats[0].deployment_id.is_some() && formats[0].erasure_set.is_some() {
        let deployment_id = formats[0].deployment_id.clone().unwrap();
        let set_id = formats[0].set_id.clone().unwrap();
        let members = formats[0].erasure_set.as_ref().unwrap().members.clone();
        // rebuild node_volumes from members + nodes list
        // we know our own paths; remote paths will be discovered via probes
        let node_volumes = build_node_volumes_from_members(
            &node_id,
            &advertise,
            disk_paths,
            &members,
            nodes,
        );
        return Ok(ResolvedIdentity {
            node_id,
            deployment_id,
            set_id,
            advertise,
            nodes: nodes.to_vec(),
            disk_paths: disk_paths.to_vec(),
            all_members: members,
            node_volumes,
        });
    }

    // step 4: first boot cluster -- exchange identity with remote nodes
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("build http client: {}", e))?;

    let mut all_identities = vec![local_identity.clone()];
    let expected_nodes = nodes.len();

    tracing::info!(
        "waiting for {} remote node(s) to exchange identity",
        remote_nodes.len()
    );

    loop {
        for node in &remote_nodes {
            if all_identities.iter().any(|id| {
                let node_base = node.trim_end_matches('/');
                id.advertise.trim_end_matches('/') == node_base
            }) {
                continue;
            }

            let url = format!("{}/_admin/cluster/join", node.trim_end_matches('/'));
            let req = client.post(&url).json(&local_identity);
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(peer_identity) = resp.json::<NodeIdentity>().await {
                        if !all_identities.iter().any(|id| id.node_id == peer_identity.node_id) {
                            tracing::info!("discovered node {}", peer_identity.node_id);
                            all_identities.push(peer_identity);
                        }
                    }
                }
                _ => {}
            }
        }

        if all_identities.len() >= expected_nodes {
            break;
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // step 5: deterministic deployment_id from sorted node_ids
    let mut node_ids: Vec<String> = all_identities.iter().map(|id| id.node_id.clone()).collect();
    node_ids.sort();
    let deployment_id = deployment_id_for(&node_ids);
    let set_id = set_id_for(&node_ids);

    // step 6: build full member list and finalize
    let members = build_members(&all_identities);
    finalize_volumes(disk_paths, &mut formats, &deployment_id, &set_id, members.clone())?;

    let peer_endpoints: Vec<String> = all_identities
        .iter()
        .filter(|id| id.node_id != node_id)
        .map(|id| id.advertise.clone())
        .collect();

    let node_volumes: Vec<NodeVolumes> = all_identities
        .iter()
        .map(|id| NodeVolumes {
            node_id: id.node_id.clone(),
            endpoint: id.advertise.clone(),
            volume_paths: id.volumes.iter().map(|v| v.path.clone()).collect(),
        })
        .collect();

    Ok(ResolvedIdentity {
        node_id,
        deployment_id,
        set_id,
        advertise,
        nodes: peer_endpoints,
        disk_paths: disk_paths.to_vec(),
        all_members: members,
        node_volumes,
    })
}

fn build_members(identities: &[NodeIdentity]) -> Vec<SetMember> {
    let mut members = Vec::new();
    // sort by node_id for deterministic ordering
    let mut sorted: Vec<&NodeIdentity> = identities.iter().collect();
    sorted.sort_by_key(|id| &id.node_id);
    let mut global_index = 0u32;
    for identity in sorted {
        for vol in &identity.volumes {
            members.push(SetMember {
                volume_id: vol.volume_id.clone(),
                node_id: identity.node_id.clone(),
                index: global_index,
            });
            global_index += 1;
        }
    }
    members
}

fn deployment_id_for(sorted_node_ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"deployment:");
    for id in sorted_node_ids {
        hasher.update(id.as_bytes());
        hasher.update(b":");
    }
    let digest = hasher.finalize();
    format!("abixio-{}", hex::encode(&digest[..8]))
}

fn set_id_for(sorted_node_ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"set:");
    for id in sorted_node_ids {
        hasher.update(id.as_bytes());
        hasher.update(b":");
    }
    let digest = hasher.finalize();
    format!("set-{}", hex::encode(&digest[..8]))
}

/// Rebuild node_volumes from persisted members + the --nodes list.
/// For the local node, we know the disk_paths. For remote nodes, we use
/// the node endpoint from --nodes and the volume paths aren't known yet
/// (they'll be resolved when the remote StorageServer is probed).
fn build_node_volumes_from_members(
    local_node_id: &str,
    local_advertise: &str,
    local_disk_paths: &[PathBuf],
    members: &[SetMember],
    nodes: &[String],
) -> Vec<NodeVolumes> {
    use std::collections::HashMap;

    // group members by node_id
    let mut by_node: HashMap<String, Vec<String>> = HashMap::new();
    for m in members {
        by_node.entry(m.node_id.clone()).or_default().push(m.volume_id.clone());
    }

    let mut result = Vec::new();

    // local node
    if by_node.contains_key(local_node_id) {
        result.push(NodeVolumes {
            node_id: local_node_id.to_string(),
            endpoint: local_advertise.to_string(),
            volume_paths: local_disk_paths.iter().map(|p| p.display().to_string()).collect(),
        });
    }

    // remote nodes -- match by position in the sorted node list
    // we don't know remote paths from persisted state, so we use volume_id as placeholder
    for (nid, vol_ids) in &by_node {
        if nid == local_node_id {
            continue;
        }
        // find matching endpoint from --nodes (best effort: use first unmatched)
        let endpoint = nodes.iter()
            .find(|n| {
                let trimmed = n.trim_end_matches('/');
                trimmed != local_advertise.trim_end_matches('/')
            })
            .cloned()
            .unwrap_or_default();
        result.push(NodeVolumes {
            node_id: nid.clone(),
            endpoint,
            volume_paths: vol_ids.clone(),
        });
    }

    result
}

fn derive_advertise(listen: &str) -> String {
    // convert ":10000" or "0.0.0.0:10000" to "http://hostname:10000"
    let addr = if listen.starts_with(':') {
        format!("0.0.0.0{}", listen)
    } else {
        listen.to_string()
    };
    // try to get a real hostname
    let host: String = hostname::get()
        .ok()
        .and_then(|h: std::ffi::OsString| h.into_string().ok())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = addr.rsplit(':').next().unwrap_or("10000");
    format!("http://{}:{}", host, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_id_is_deterministic() {
        let ids = vec!["node-a".to_string(), "node-b".to_string()];
        assert_eq!(deployment_id_for(&ids), deployment_id_for(&ids));
    }

    #[test]
    fn deployment_id_differs_for_different_nodes() {
        let a = vec!["node-a".to_string()];
        let b = vec!["node-b".to_string()];
        assert_ne!(deployment_id_for(&a), deployment_id_for(&b));
    }

    #[test]
    fn build_members_is_sorted_by_node_id() {
        let ids = vec![
            NodeIdentity {
                node_id: "z-node".to_string(),
                advertise: "http://z:10000".to_string(),
                volumes: vec![VolumeEntry { volume_id: "v1".to_string(), index: 0, path: "/d1".to_string() }],
            },
            NodeIdentity {
                node_id: "a-node".to_string(),
                advertise: "http://a:10000".to_string(),
                volumes: vec![VolumeEntry { volume_id: "v2".to_string(), index: 0, path: "/d2".to_string() }],
            },
        ];
        let members = build_members(&ids);
        assert_eq!(members[0].node_id, "a-node");
        assert_eq!(members[1].node_id, "z-node");
    }
}
