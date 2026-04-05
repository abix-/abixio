use std::path::{Component, Path, PathBuf};

use super::StorageError;

const MAX_KEY_LEN: usize = 1024;
const WINDOWS_FORBIDDEN: [char; 8] = [':', '*', '?', '"', '|', '<', '>', '\\'];

pub fn validate_bucket_name(bucket: &str) -> Result<(), StorageError> {
    let bucket = bucket.trim();
    if bucket.is_empty() || bucket.len() > 63 {
        return Err(StorageError::InvalidBucketName(bucket.to_string()));
    }
    if bucket == "abixio" {
        return Err(StorageError::InvalidBucketName(bucket.to_string()));
    }
    if bucket.parse::<std::net::Ipv4Addr>().is_ok() {
        return Err(StorageError::InvalidBucketName(bucket.to_string()));
    }
    if bucket.contains("..") || bucket.contains(".-") || bucket.contains("-.") {
        return Err(StorageError::InvalidBucketName(bucket.to_string()));
    }
    if !bucket
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
    {
        return Err(StorageError::InvalidBucketName(bucket.to_string()));
    }
    if !bucket
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        || !bucket
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(StorageError::InvalidBucketName(bucket.to_string()));
    }
    Ok(())
}

pub fn validate_object_key(key: &str) -> Result<(), StorageError> {
    if key.is_empty() || key.len() > MAX_KEY_LEN {
        return Err(StorageError::InvalidObjectKey(key.to_string()));
    }
    if key.starts_with('/') || key.contains('\0') || key.contains("//") {
        return Err(StorageError::InvalidObjectKey(key.to_string()));
    }
    validate_key_segments(key)
}

pub fn validate_object_prefix(prefix: &str) -> Result<(), StorageError> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.len() > MAX_KEY_LEN || prefix.starts_with('/') || prefix.contains('\0') || prefix.contains("//") {
        return Err(StorageError::InvalidObjectKey(prefix.to_string()));
    }
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(());
    }
    validate_key_segments(trimmed)
}

pub fn validate_version_id(version_id: &str) -> Result<(), StorageError> {
    if uuid::Uuid::parse_str(version_id).is_err() {
        return Err(StorageError::InvalidVersionId(version_id.to_string()));
    }
    Ok(())
}

pub fn validate_upload_id(upload_id: &str) -> Result<(), StorageError> {
    if uuid::Uuid::parse_str(upload_id).is_err() {
        return Err(StorageError::InvalidUploadId(upload_id.to_string()));
    }
    Ok(())
}

pub fn bucket_dir(root: &Path, bucket: &str) -> Result<PathBuf, StorageError> {
    validate_bucket_name(bucket)?;
    safe_join(root, &[bucket])
}

pub fn object_dir(root: &Path, bucket: &str, key: &str) -> Result<PathBuf, StorageError> {
    validate_bucket_name(bucket)?;
    validate_object_key(key)?;
    let mut parts = Vec::with_capacity(1 + key.split('/').count());
    parts.push(bucket);
    parts.extend(key.split('/'));
    safe_join(root, &parts)
}

pub fn object_meta_path(root: &Path, bucket: &str, key: &str) -> Result<PathBuf, StorageError> {
    Ok(object_dir(root, bucket, key)?.join("meta.json"))
}

pub fn object_shard_path(root: &Path, bucket: &str, key: &str) -> Result<PathBuf, StorageError> {
    Ok(object_dir(root, bucket, key)?.join("shard.dat"))
}

pub fn version_dir(root: &Path, bucket: &str, key: &str, version_id: &str) -> Result<PathBuf, StorageError> {
    validate_version_id(version_id)?;
    Ok(object_dir(root, bucket, key)?.join(version_id))
}

pub fn version_shard_path(root: &Path, bucket: &str, key: &str, version_id: &str) -> Result<PathBuf, StorageError> {
    Ok(version_dir(root, bucket, key, version_id)?.join("shard.dat"))
}

pub fn bucket_settings_dir(root: &Path, bucket: &str) -> Result<PathBuf, StorageError> {
    validate_bucket_name(bucket)?;
    safe_join(root, &[".abixio.sys", "buckets", bucket])
}

pub fn bucket_settings_path(root: &Path, bucket: &str) -> Result<PathBuf, StorageError> {
    Ok(bucket_settings_dir(root, bucket)?.join("settings.json"))
}

pub fn multipart_bucket_dir(root: &Path, bucket: &str) -> Result<PathBuf, StorageError> {
    validate_bucket_name(bucket)?;
    safe_join(root, &[".abixio.sys", "multipart", bucket])
}

pub fn multipart_upload_dir(root: &Path, bucket: &str, key: &str, upload_id: &str) -> Result<PathBuf, StorageError> {
    validate_bucket_name(bucket)?;
    validate_object_key(key)?;
    validate_upload_id(upload_id)?;
    let mut parts = vec![".abixio.sys", "multipart", bucket];
    parts.extend(key.split('/'));
    parts.push(upload_id);
    safe_join(root, &parts)
}

fn validate_key_segments(key: &str) -> Result<(), StorageError> {
    for segment in key.split('/') {
        if segment.is_empty() || segment.trim() != segment || segment == "." || segment == ".." {
            return Err(StorageError::InvalidObjectKey(key.to_string()));
        }
        if segment.chars().any(|c| WINDOWS_FORBIDDEN.contains(&c)) {
            return Err(StorageError::InvalidObjectKey(key.to_string()));
        }
    }
    Ok(())
}

fn safe_join(root: &Path, parts: &[&str]) -> Result<PathBuf, StorageError> {
    let mut candidate = root.to_path_buf();
    for part in parts {
        candidate.push(part);
    }
    let normalized = normalize_path(&candidate);
    let normalized_root = normalize_path(root);
    if normalized.starts_with(&normalized_root) {
        Ok(normalized)
    } else {
        Err(StorageError::InvalidObjectKey(candidate.to_string_lossy().to_string()))
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_keys_are_valid() {
        assert!(validate_object_key("a/b/c.txt").is_ok());
    }

    #[test]
    fn hostile_key_segments_are_rejected() {
        assert!(validate_object_key("a/../b").is_err());
        assert!(validate_object_key("./x").is_err());
        assert!(validate_object_key("a//b").is_err());
        assert!(validate_object_key("a\\b").is_err());
    }

    #[test]
    fn identifiers_must_be_uuids() {
        assert!(validate_version_id("not-a-uuid").is_err());
        assert!(validate_upload_id("../escape").is_err());
    }
}
