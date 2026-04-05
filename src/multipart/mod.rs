use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::storage::Backend;
use crate::storage::StorageError;
use crate::storage::bitrot::{md5_hex, sha256_hex};
use crate::storage::metadata::{ErasureMeta, ObjectInfo};
use crate::storage::pathing;

const UPLOAD_FILE: &str = "upload.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadMeta {
    pub upload_id: String,
    pub bucket: String,
    pub key: String,
    pub content_type: String,
    pub user_metadata: HashMap<String, String>,
    pub created_at: u64,
    pub data_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartMeta {
    pub part_number: i32,
    pub size: u64,
    pub etag: String,
}

/// Create a new multipart upload. Writes upload.json to all disks.
pub fn create_upload(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
    content_type: &str,
    user_metadata: HashMap<String, String>,
) -> Result<String, StorageError> {
    pathing::validate_bucket_name(bucket)?;
    pathing::validate_object_key(key)?;
    let upload_id = uuid::Uuid::new_v4().to_string();
    let data_dir = uuid::Uuid::new_v4().to_string();
    let now = crate::storage::metadata::unix_timestamp_secs();

    let meta = UploadMeta {
        upload_id: upload_id.clone(),
        bucket: bucket.to_string(),
        key: key.to_string(),
        content_type: content_type.to_string(),
        user_metadata,
        created_at: now,
        data_dir,
    };

    let json = serde_json::to_vec_pretty(&meta)
        .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

    let mut successes = 0;
    for disk in disks {
        let dir = upload_dir(disk, bucket, key, &upload_id)?;
        if fs::create_dir_all(&dir).is_ok() && fs::write(dir.join(UPLOAD_FILE), &json).is_ok() {
            successes += 1;
        }
    }
    if successes == 0 {
        return Err(StorageError::WriteQuorum);
    }

    Ok(upload_id)
}

/// Write a part. Erasure-encodes the part data across all disks.
pub fn put_part(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: i32,
    data: &[u8],
) -> Result<PartMeta, StorageError> {
    pathing::validate_bucket_name(bucket)?;
    pathing::validate_object_key(key)?;
    pathing::validate_upload_id(upload_id)?;
    // read upload meta to get data_dir
    let upload = read_upload(disks, bucket, key, upload_id)?;
    let etag = md5_hex(data);
    let total = data_n + parity_n;
    let part_file = format!("part.{}", part_number);

    // split + encode
    let distribution = crate::storage::erasure_encode::hash_order(
        &format!("{}/{}/{}", bucket, key, part_number),
        total,
    );

    let mut shards = crate::storage::erasure_encode::split_data(data, data_n);
    if parity_n > 0 {
        let shard_size = shards[0].len();
        for _ in 0..parity_n {
            shards.push(vec![0u8; shard_size]);
        }
        let rs = reed_solomon_erasure::galois_8::ReedSolomon::new(data_n, parity_n)
            .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
        rs.encode(&mut shards)
            .map_err(|e| StorageError::InvalidConfig(format!("encode: {}", e)))?;
    }

    // write shard to each disk
    let mut successes = 0;
    for (shard_idx, shard_data) in shards.iter().enumerate() {
        let disk_idx = distribution[shard_idx];
        if disk_idx >= disks.len() {
            continue;
        }
        let dir = upload_dir(&disks[disk_idx], bucket, key, upload_id)?;
        let data_path = dir.join(&upload.data_dir);
        if fs::create_dir_all(&data_path).is_ok()
            && fs::write(data_path.join(&part_file), shard_data).is_ok()
        {
            // write part meta
            let pm = PartFileMeta {
                part_number,
                size: data.len() as u64,
                etag: etag.clone(),
                erasure: ErasureMeta {
                    ftt: parity_n,
                    index: shard_idx,
                    distribution: distribution.clone(),
                    epoch_id: 1,
                    set_id: "multipart-local".to_string(),
                    node_ids: distribution
                        .iter()
                        .map(|_| "local".to_string())
                        .collect(),
                    volume_ids: distribution
                        .iter()
                        .map(|disk_idx| format!("vol-{}", disk_idx))
                        .collect(),
                },
                checksum: sha256_hex(shard_data),
            };
            let meta_json = serde_json::to_vec(&pm).unwrap_or_default();
            let meta_file = format!("part.{}.meta", part_number);
            let _ = fs::write(data_path.join(meta_file), meta_json);
            successes += 1;
        }
    }

    let write_quorum = if parity_n == 0 { data_n } else { data_n + 1 };
    if successes < write_quorum {
        return Err(StorageError::WriteQuorum);
    }

    Ok(PartMeta {
        part_number,
        size: data.len() as u64,
        etag,
    })
}

/// List parts for an upload.
pub fn list_parts(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<(UploadMeta, Vec<PartMeta>), StorageError> {
    pathing::validate_bucket_name(bucket)?;
    pathing::validate_object_key(key)?;
    pathing::validate_upload_id(upload_id)?;
    let upload = read_upload(disks, bucket, key, upload_id)?;
    let mut parts = Vec::new();

    // read from first responsive disk
    for disk in disks {
        let data_path = upload_dir(disk, bucket, key, upload_id)?.join(&upload.data_dir);
        if !data_path.is_dir() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&data_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".meta") && name.starts_with("part.") {
                    if let Ok(data) = fs::read(entry.path()) {
                        if let Ok(pm) = serde_json::from_slice::<PartFileMeta>(&data) {
                            // dedup by part number
                            if !parts
                                .iter()
                                .any(|p: &PartMeta| p.part_number == pm.part_number)
                            {
                                parts.push(PartMeta {
                                    part_number: pm.part_number,
                                    size: pm.size,
                                    etag: pm.etag,
                                });
                            }
                        }
                    }
                }
            }
        }
        break;
    }

    parts.sort_by_key(|p| p.part_number);
    Ok((upload, parts))
}

/// Complete a multipart upload. Moves part shards into the final object
/// directory and writes metadata. No data reassembly -- like MinIO.
pub fn complete_upload(
    disks: &[Box<dyn Backend>],
    _data_n: usize,
    _parity_n: usize,
    bucket: &str,
    key: &str,
    upload_id: &str,
    requested_parts: &[(i32, String)], // (part_number, etag)
) -> Result<ObjectInfo, StorageError> {
    pathing::validate_bucket_name(bucket)?;
    pathing::validate_object_key(key)?;
    pathing::validate_upload_id(upload_id)?;
    let (upload, available_parts) = list_parts(disks, bucket, key, upload_id)?;

    // validate all requested parts exist
    for (pn, _etag) in requested_parts {
        if !available_parts.iter().any(|p| p.part_number == *pn) {
            return Err(StorageError::ObjectNotFound);
        }
    }

    // collect part metadata and move shards to final object directory
    let mut parts = Vec::new();
    let mut total_size: u64 = 0;
    let now = crate::storage::metadata::unix_timestamp_secs();

    for (pn, _etag) in requested_parts {
        // read part meta from first responsive disk
        let mut part_meta: Option<PartFileMeta> = None;
        for disk in disks {
            let meta_path = upload_dir(disk, bucket, key, upload_id)?
                .join(&upload.data_dir)
                .join(format!("part.{}.meta", pn));
            if let Ok(data) = fs::read(&meta_path) {
                if let Ok(pm) = serde_json::from_slice::<PartFileMeta>(&data) {
                    part_meta = Some(pm);
                    break;
                }
            }
        }
        let pm = part_meta.ok_or(StorageError::ObjectNotFound)?;

        // move part shard from staging to final object dir on each disk
        let part_file = format!("part.{}", pn);
        for (disk_idx, disk) in disks.iter().enumerate() {
            let staging_path = upload_dir(disk, bucket, key, upload_id)?
                .join(&upload.data_dir)
                .join(&part_file);
            if staging_path.exists() {
                let obj_dir = pathing::object_dir(&disk_root(disk), bucket, key)?;
                let _ = fs::create_dir_all(&obj_dir);
                let final_path = obj_dir.join(&part_file);
                // rename (same filesystem = instant move, no data copy)
                if fs::rename(&staging_path, &final_path).is_err() {
                    // fallback: copy + delete if rename fails (cross-filesystem)
                    if let Ok(data) = fs::read(&staging_path) {
                        let _ = fs::write(&final_path, &data);
                        let _ = fs::remove_file(&staging_path);
                    }
                }
            }
        }

        total_size += pm.size;
        parts.push(crate::storage::metadata::PartEntry {
            number: *pn,
            size: pm.size,
            etag: pm.etag.clone(),
            erasure: pm.erasure.clone(),
            checksum: pm.checksum.clone(),
        });
    }

    // compute final ETag: MD5(concat(part_etags)) + "-N"
    let mut etag_concat = String::new();
    for (_, etag) in requested_parts {
        etag_concat.push_str(etag.trim_matches('"'));
    }
    let final_etag = format!(
        "{}-{}",
        md5_hex(etag_concat.as_bytes()),
        requested_parts.len()
    );

    // write meta.json on each disk with parts manifest
    let meta = crate::storage::metadata::ObjectMeta {
        size: total_size,
        etag: final_etag.clone(),
        content_type: upload.content_type.clone(),
        created_at: now,
        erasure: parts.first().map(|p| p.erasure.clone()).unwrap_or(ErasureMeta {
            ftt: 0,
            index: 0,
            distribution: Vec::new(),
            epoch_id: 0,
            set_id: String::new(),
            node_ids: Vec::new(),
            volume_ids: Vec::new(),
        }),
        checksum: String::new(),
        user_metadata: upload.user_metadata.clone(),
        tags: HashMap::new(),
        version_id: String::new(),
        is_latest: true,
        is_delete_marker: false,
        parts: parts.clone(),
    };

    // write meta.json to each disk
    let mut meta_successes = 0;
    for (disk_idx, disk) in disks.iter().enumerate() {
        let obj_dir = pathing::object_dir(&disk_root(disk), bucket, key)?;
        let _ = fs::create_dir_all(&obj_dir);
        let mut disk_meta = meta.clone();
        disk_meta.erasure.index = disk_idx;
        let mf = crate::storage::metadata::ObjectMetaFile {
            versions: vec![disk_meta],
        };
        let meta_path = pathing::object_meta_path(&disk_root(disk), bucket, key)?;
        if crate::storage::metadata::write_meta_file(&meta_path, &mf).is_ok() {
            meta_successes += 1;
        }
    }

    if meta_successes == 0 {
        return Err(StorageError::WriteQuorum);
    }

    // cleanup staging dirs
    for disk in disks {
        if let Ok(dir) = upload_dir(disk, bucket, key, upload_id) {
            let _ = fs::remove_dir_all(&dir);
        }
    }

    Ok(ObjectInfo {
        bucket: bucket.to_string(),
        key: key.to_string(),
        size: total_size,
        etag: final_etag,
        content_type: upload.content_type,
        created_at: now,
        user_metadata: upload.user_metadata,
        tags: HashMap::new(),
        version_id: String::new(),
        is_delete_marker: false,
    })
}

/// Abort a multipart upload.
pub fn abort_upload(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<(), StorageError> {
    pathing::validate_bucket_name(bucket)?;
    pathing::validate_object_key(key)?;
    pathing::validate_upload_id(upload_id)?;
    for disk in disks {
        let dir = upload_dir(disk, bucket, key, upload_id)?;
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(())
}

/// List all in-progress multipart uploads for a bucket.
pub fn list_uploads(
    disks: &[Box<dyn Backend>],
    bucket: &str,
) -> Result<Vec<UploadMeta>, StorageError> {
    pathing::validate_bucket_name(bucket)?;
    let mut uploads = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for disk in disks {
        let bucket_dir = multipart_bucket_dir(disk, bucket)?;
        if !bucket_dir.is_dir() {
            continue;
        }
        // walk: bucket_dir/<key>/<upload-id>/upload.json
        walk_uploads(&bucket_dir, &bucket_dir, &mut uploads, &mut seen);
        break; // first responsive disk is enough
    }

    Ok(uploads)
}

// -- helpers --

fn upload_dir(
    disk: &Box<dyn Backend>,
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<PathBuf, StorageError> {
    pathing::multipart_upload_dir(&disk_root(disk), bucket, key, upload_id)
}

fn multipart_bucket_dir(disk: &Box<dyn Backend>, bucket: &str) -> Result<PathBuf, StorageError> {
    pathing::multipart_bucket_dir(&disk_root(disk), bucket)
}

fn disk_root(disk: &Box<dyn Backend>) -> PathBuf {
    let info = disk.info();
    if let Some(path) = info.label.strip_prefix("local:") {
        PathBuf::from(path)
    } else {
        PathBuf::from(".")
    }
}

fn read_upload(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<UploadMeta, StorageError> {
    pathing::validate_bucket_name(bucket)?;
    pathing::validate_object_key(key)?;
    pathing::validate_upload_id(upload_id)?;
    for disk in disks {
        let path = upload_dir(disk, bucket, key, upload_id)?.join(UPLOAD_FILE);
        if let Ok(data) = fs::read(&path) {
            if let Ok(meta) = serde_json::from_slice::<UploadMeta>(&data) {
                return Ok(meta);
            }
        }
    }
    Err(StorageError::ObjectNotFound)
}

fn walk_uploads(
    base: &Path,
    dir: &Path,
    uploads: &mut Vec<UploadMeta>,
    seen: &mut std::collections::HashSet<String>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // check if this dir has upload.json
            let upload_path = path.join(UPLOAD_FILE);
            if upload_path.exists() {
                if let Ok(data) = fs::read(&upload_path) {
                    if let Ok(meta) = serde_json::from_slice::<UploadMeta>(&data) {
                        if seen.insert(meta.upload_id.clone()) {
                            uploads.push(meta);
                        }
                    }
                }
            } else {
                walk_uploads(base, &path, uploads, seen);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartFileMeta {
    part_number: i32,
    size: u64,
    etag: String,
    erasure: ErasureMeta,
    checksum: String,
}
