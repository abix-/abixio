use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::storage::Backend;
use crate::storage::StorageError;
use crate::storage::bitrot::{md5_hex, sha256_hex};
use crate::storage::metadata::{ErasureMeta, ObjectInfo, PutOptions};

const MULTIPART_DIR: &str = ".abixio.sys/multipart";
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
        let dir = upload_dir(disk, bucket, key, &upload_id);
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
        let dir = upload_dir(&disks[disk_idx], bucket, key, upload_id);
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
                    data: data_n,
                    parity: parity_n,
                    index: shard_idx,
                    distribution: distribution.clone(),
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
    let upload = read_upload(disks, bucket, key, upload_id)?;
    let mut parts = Vec::new();

    // read from first responsive disk
    for disk in disks {
        let data_path = upload_dir(disk, bucket, key, upload_id).join(&upload.data_dir);
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
                            if !parts.iter().any(|p: &PartMeta| p.part_number == pm.part_number) {
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

/// Complete a multipart upload. Assembles parts into final object.
pub fn complete_upload(
    disks: &[Box<dyn Backend>],
    data_n: usize,
    parity_n: usize,
    bucket: &str,
    key: &str,
    upload_id: &str,
    requested_parts: &[(i32, String)], // (part_number, etag)
) -> Result<ObjectInfo, StorageError> {
    let (upload, available_parts) = list_parts(disks, bucket, key, upload_id)?;

    // validate all requested parts exist
    for (pn, _etag) in requested_parts {
        if !available_parts.iter().any(|p| p.part_number == *pn) {
            return Err(StorageError::ObjectNotFound);
        }
    }

    // read and concatenate all part data
    let mut full_data = Vec::new();
    let total = data_n + parity_n;

    for (pn, _etag) in requested_parts {
        let part_file = format!("part.{}", pn);

        // read part meta from first responsive disk to get distribution
        let mut part_meta: Option<PartFileMeta> = None;
        for disk in disks {
            let meta_path = upload_dir(disk, bucket, key, upload_id)
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
        let distribution = &pm.erasure.distribution;

        // read shards from all disks
        let mut shard_slots: Vec<Option<Vec<u8>>> = vec![None; total];
        for (shard_idx, &disk_idx) in distribution.iter().enumerate() {
            if disk_idx >= disks.len() {
                continue;
            }
            let shard_path = upload_dir(&disks[disk_idx], bucket, key, upload_id)
                .join(&upload.data_dir)
                .join(&part_file);
            if let Ok(shard_data) = fs::read(&shard_path) {
                shard_slots[shard_idx] = Some(shard_data);
            }
        }

        // erasure decode
        let good = shard_slots.iter().filter(|s| s.is_some()).count();
        if good < data_n {
            return Err(StorageError::ReadQuorum);
        }
        if parity_n > 0 && good < total {
            let rs = reed_solomon_erasure::galois_8::ReedSolomon::new(data_n, parity_n)
                .map_err(|e| StorageError::InvalidConfig(format!("reed-solomon: {}", e)))?;
            rs.reconstruct(&mut shard_slots)
                .map_err(|_| StorageError::ReadQuorum)?;
        }

        let mut part_data = Vec::with_capacity(pm.size as usize);
        for shard in shard_slots.iter().take(data_n).flatten() {
            part_data.extend_from_slice(shard);
        }
        part_data.truncate(pm.size as usize);
        full_data.extend_from_slice(&part_data);
    }

    // compute final ETag: MD5(concat(part_etags)) + "-N"
    let mut etag_concat = String::new();
    for (_, etag) in requested_parts {
        etag_concat.push_str(etag.trim_matches('"'));
    }
    let final_etag = format!("{}-{}", md5_hex(etag_concat.as_bytes()), requested_parts.len());

    // write final object using normal put_object path
    let opts = PutOptions {
        content_type: upload.content_type.clone(),
        user_metadata: upload.user_metadata.clone(),
        ..Default::default()
    };

    // use the Store's put_object -- but we need to go through the ErasureSet.
    // instead, call the erasure encode directly, matching how put_object works.
    let info = crate::storage::erasure_encode::encode_and_write_with_mrf(
        disks, data_n, parity_n, bucket, key, &full_data, opts, None,
    )?;

    // override the etag with the multipart etag
    let final_info = ObjectInfo {
        etag: final_etag,
        ..info
    };

    // cleanup: delete upload dir from all disks
    for disk in disks {
        let dir = upload_dir(disk, bucket, key, upload_id);
        let _ = fs::remove_dir_all(&dir);
    }

    Ok(final_info)
}

/// Abort a multipart upload.
pub fn abort_upload(
    disks: &[Box<dyn Backend>],
    bucket: &str,
    key: &str,
    upload_id: &str,
) -> Result<(), StorageError> {
    for disk in disks {
        let dir = upload_dir(disk, bucket, key, upload_id);
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(())
}

/// List all in-progress multipart uploads for a bucket.
pub fn list_uploads(
    disks: &[Box<dyn Backend>],
    bucket: &str,
) -> Result<Vec<UploadMeta>, StorageError> {
    let mut uploads = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for disk in disks {
        let bucket_dir = multipart_bucket_dir(disk, bucket);
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

fn upload_dir(disk: &Box<dyn Backend>, bucket: &str, key: &str, upload_id: &str) -> PathBuf {
    disk_root(disk).join(MULTIPART_DIR).join(bucket).join(key).join(upload_id)
}

fn multipart_bucket_dir(disk: &Box<dyn Backend>, bucket: &str) -> PathBuf {
    disk_root(disk).join(MULTIPART_DIR).join(bucket)
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
    for disk in disks {
        let path = upload_dir(disk, bucket, key, upload_id).join(UPLOAD_FILE);
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
