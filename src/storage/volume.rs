use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const SYS_DIR: &str = ".abixio.sys";
const VOLUME_FILE: &str = "volume.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VolumeFormat {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "deployment_id")]
    pub cluster_id: Option<String>,
    pub node_id: String,
    pub volume_id: String,
    pub volume_index: u32,
    pub created_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(alias = "erasure_set")]
    #[serde(alias = "pool")]
    pub cluster: Option<ClusterMembers>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClusterMembers {
    pub members: Vec<SetMember>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetMember {
    pub volume_id: String,
    pub node_id: String,
    pub index: u32,
}

fn volume_path(disk_root: &Path) -> PathBuf {
    disk_root.join(SYS_DIR).join(VOLUME_FILE)
}

pub fn read_volume_format(disk_root: &Path) -> Option<VolumeFormat> {
    let path = volume_path(disk_root);
    fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
}

pub fn write_volume_format(disk_root: &Path, format: &VolumeFormat) -> std::io::Result<()> {
    let dir = disk_root.join(SYS_DIR);
    fs::create_dir_all(&dir)?;
    let data = serde_json::to_vec_pretty(format)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(volume_path(disk_root), data)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn generate_volume_format(node_id: &str, volume_index: u32) -> VolumeFormat {
    VolumeFormat {
        version: 1,
        cluster_id: None,
        node_id: node_id.to_string(),
        volume_id: uuid::Uuid::new_v4().to_string(),
        volume_index,
        created_at: unix_now(),
        cluster: None,
    }
}

/// Read volume.json from all disk paths. Returns None if no disks have it.
/// Returns Err if some disks have it and others don't (mixed state), or if
/// node_id/cluster_id disagree across disks.
pub fn load_volumes(disk_paths: &[PathBuf]) -> Result<Option<Vec<VolumeFormat>>, String> {
    let formats: Vec<Option<VolumeFormat>> = disk_paths
        .iter()
        .map(|p| read_volume_format(p))
        .collect();
    let present: Vec<&VolumeFormat> = formats.iter().filter_map(|f| f.as_ref()).collect();

    if present.is_empty() {
        return Ok(None);
    }
    if present.len() != disk_paths.len() {
        return Err(format!(
            "mixed state: {} of {} volumes have volume.json",
            present.len(),
            disk_paths.len()
        ));
    }

    // validate consistency
    let node_id = &present[0].node_id;
    for fmt in &present {
        if fmt.node_id != *node_id {
            return Err(format!(
                "node_id mismatch across volumes: {} vs {}",
                node_id, fmt.node_id
            ));
        }
        if fmt.cluster_id != present[0].cluster_id {
            return Err("cluster_id mismatch across volumes".to_string());
        }
    }

    // check for duplicate volume_ids
    let mut seen = std::collections::BTreeSet::new();
    for fmt in &present {
        if !seen.insert(&fmt.volume_id) {
            return Err(format!("duplicate volume_id: {}", fmt.volume_id));
        }
    }

    Ok(Some(formats.into_iter().flatten().collect()))
}

/// Format fresh disks with generated identity. Returns the formats written.
pub fn format_volumes(disk_paths: &[PathBuf]) -> Result<Vec<VolumeFormat>, String> {
    let node_id = uuid::Uuid::new_v4().to_string();
    let mut formats = Vec::new();
    for (i, path) in disk_paths.iter().enumerate() {
        let fmt = generate_volume_format(&node_id, i as u32);
        write_volume_format(path, &fmt).map_err(|e| format!("write volume.json: {}", e))?;
        formats.push(fmt);
    }
    Ok(formats)
}

/// Finalize volumes with cluster_id and full member list.
pub fn finalize_volumes(
    disk_paths: &[PathBuf],
    formats: &mut [VolumeFormat],
    cluster_id: &str,
    members: Vec<SetMember>,
) -> Result<(), String> {
    let cm = ClusterMembers { members };
    for (i, fmt) in formats.iter_mut().enumerate() {
        fmt.cluster_id = Some(cluster_id.to_string());
        fmt.cluster = Some(cm.clone());
        write_volume_format(&disk_paths[i], fmt)
            .map_err(|e| format!("write volume.json: {}", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn format_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let paths = vec![dir.path().to_path_buf()];
        let formats = format_volumes(&paths).unwrap();
        assert_eq!(formats.len(), 1);
        assert_eq!(formats[0].version, 1);
        assert!(formats[0].cluster_id.is_none());

        let loaded = load_volumes(&paths).unwrap().unwrap();
        assert_eq!(loaded, formats);
    }

    #[test]
    fn load_empty_disks_returns_none() {
        let dir = TempDir::new().unwrap();
        let paths = vec![dir.path().to_path_buf()];
        assert!(load_volumes(&paths).unwrap().is_none());
    }

    #[test]
    fn finalize_writes_deployment_and_members() {
        let dir = TempDir::new().unwrap();
        let paths = vec![dir.path().to_path_buf()];
        let mut formats = format_volumes(&paths).unwrap();
        let members = vec![SetMember {
            volume_id: formats[0].volume_id.clone(),
            node_id: formats[0].node_id.clone(),
            index: 0,
        }];
        finalize_volumes(&paths, &mut formats, "deploy-1", members).unwrap();

        let loaded = load_volumes(&paths).unwrap().unwrap();
        assert_eq!(loaded[0].cluster_id.as_deref(), Some("deploy-1"));
        assert!(loaded[0].cluster.is_some());
    }

    #[test]
    fn mixed_state_is_error() {
        let dir = TempDir::new().unwrap();
        let d1 = dir.path().join("d1");
        let d2 = dir.path().join("d2");
        fs::create_dir_all(&d1).unwrap();
        fs::create_dir_all(&d2).unwrap();
        // only format d1
        let _ = format_volumes(&[d1.clone()]).unwrap();
        let result = load_volumes(&[d1, d2]);
        assert!(result.is_err());
    }
}
