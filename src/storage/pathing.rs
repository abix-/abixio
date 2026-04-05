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

    // --- comprehensive security coverage ---

    #[test]
    fn bucket_name_rejects_all_hostile_variants() {
        // traversal
        assert!(validate_bucket_name("..").is_err());
        assert!(validate_bucket_name("../x").is_err());
        assert!(validate_bucket_name("a..b").is_err());
        // reserved
        assert!(validate_bucket_name("abixio").is_err());
        // IP address
        assert!(validate_bucket_name("127.0.0.1").is_err());
        assert!(validate_bucket_name("192.168.1.1").is_err());
        // length
        assert!(validate_bucket_name("").is_err());
        assert!(validate_bucket_name(&"a".repeat(64)).is_err());
        // forbidden chars
        assert!(validate_bucket_name("UPPER").is_err());
        assert!(validate_bucket_name("has space").is_err());
        assert!(validate_bucket_name("has/slash").is_err());
        assert!(validate_bucket_name("has\\back").is_err());
        assert!(validate_bucket_name("a\x00b").is_err());
        // leading/trailing non-alphanumeric
        assert!(validate_bucket_name("-leading").is_err());
        assert!(validate_bucket_name("trailing-").is_err());
        assert!(validate_bucket_name(".leading").is_err());
        assert!(validate_bucket_name("trailing.").is_err());
        // adjacent special chars
        assert!(validate_bucket_name("a.-b").is_err());
        assert!(validate_bucket_name("a-.b").is_err());
    }

    #[test]
    fn bucket_name_accepts_valid_edge_cases() {
        assert!(validate_bucket_name("a").is_ok());
        assert!(validate_bucket_name("abc").is_ok());
        assert!(validate_bucket_name(&"a".repeat(63)).is_ok());
        assert!(validate_bucket_name("my.bucket.name").is_ok());
        assert!(validate_bucket_name("my-bucket-123").is_ok());
        assert!(validate_bucket_name("0bucket").is_ok());
    }

    #[test]
    fn key_rejects_all_hostile_variants() {
        // empty
        assert!(validate_object_key("").is_err());
        // too long
        assert!(validate_object_key(&"a".repeat(1025)).is_err());
        // traversal
        assert!(validate_object_key("..").is_err());
        assert!(validate_object_key("../x").is_err());
        assert!(validate_object_key("a/../../b").is_err());
        assert!(validate_object_key(".").is_err());
        assert!(validate_object_key("a/./b").is_err());
        // null byte
        assert!(validate_object_key("a\x00b").is_err());
        // double slash
        assert!(validate_object_key("a//b").is_err());
        // leading slash
        assert!(validate_object_key("/a/b").is_err());
        // windows chars
        assert!(validate_object_key("a:b").is_err());
        assert!(validate_object_key("a*b").is_err());
        assert!(validate_object_key("a?b").is_err());
        assert!(validate_object_key("a\"b").is_err());
        assert!(validate_object_key("a|b").is_err());
        assert!(validate_object_key("a<b").is_err());
        assert!(validate_object_key("a>b").is_err());
        assert!(validate_object_key("a\\b").is_err());
        // whitespace padding on segment
        assert!(validate_object_key(" a").is_err());
        assert!(validate_object_key("a ").is_err());
        assert!(validate_object_key("a/ b").is_err());
    }

    #[test]
    fn key_accepts_valid_edge_cases() {
        assert!(validate_object_key("a").is_ok());
        assert!(validate_object_key(&"a".repeat(1024)).is_ok());
        assert!(validate_object_key("a/b/c/d/e/f/g").is_ok());
        assert!(validate_object_key("file.txt").is_ok());
        assert!(validate_object_key("dir/file.tar.gz").is_ok());
        assert!(validate_object_key("a-b_c").is_ok());
    }

    #[test]
    fn prefix_rejects_hostile_variants() {
        assert!(validate_object_prefix("../x").is_err());
        assert!(validate_object_prefix("a/../b").is_err());
        assert!(validate_object_prefix("./hidden").is_err());
        assert!(validate_object_prefix("a//b").is_err());
        assert!(validate_object_prefix("a\\b").is_err());
    }

    #[test]
    fn prefix_accepts_valid_edge_cases() {
        assert!(validate_object_prefix("").is_ok());
        assert!(validate_object_prefix("a/").is_ok());
        assert!(validate_object_prefix("a/b/").is_ok());
    }

    #[test]
    fn prefix_rejects_leading_slash() {
        assert!(validate_object_prefix("/").is_err());
        assert!(validate_object_prefix("/a").is_err());
    }

    #[test]
    fn version_id_rejects_all_hostile_variants() {
        assert!(validate_version_id("").is_err());
        assert!(validate_version_id("../escape").is_err());
        assert!(validate_version_id("not-a-uuid").is_err());
        assert!(validate_version_id("a".repeat(256).as_str()).is_err());
        assert!(validate_version_id("<script>").is_err());
        assert!(validate_version_id("a\x00b").is_err());
    }

    #[test]
    fn version_id_accepts_valid_uuid() {
        assert!(validate_version_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_version_id("550e8400e29b41d4a716446655440000").is_ok());
    }

    #[test]
    fn upload_id_rejects_all_hostile_variants() {
        assert!(validate_upload_id("").is_err());
        assert!(validate_upload_id("../escape").is_err());
        assert!(validate_upload_id("not-a-uuid").is_err());
        assert!(validate_upload_id("a\x00b").is_err());
    }

    #[test]
    fn upload_id_accepts_valid_uuid() {
        assert!(validate_upload_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn safe_join_rejects_escape_from_root() {
        let root = std::path::Path::new("/data/volumes/v1");
        assert!(safe_join(root, &["..", "etc", "passwd"]).is_err());
        assert!(safe_join(root, &["bucket", "..", "..", "etc"]).is_err());
    }

    #[test]
    fn safe_join_allows_nested_within_root() {
        let root = std::path::Path::new("/data/volumes/v1");
        assert!(safe_join(root, &["bucket", "key"]).is_ok());
        assert!(safe_join(root, &["bucket", "a", "b", "c"]).is_ok());
    }

    #[test]
    fn object_dir_rejects_hostile_combinations() {
        let root = std::path::Path::new("/data");
        assert!(object_dir(root, "..", "key").is_err());
        assert!(object_dir(root, "bucket", "..").is_err());
        assert!(object_dir(root, "bucket", "../escape").is_err());
        assert!(object_dir(root, "../x", "key").is_err());
    }

    #[test]
    fn multipart_upload_dir_rejects_hostile_combinations() {
        let root = std::path::Path::new("/data");
        assert!(multipart_upload_dir(root, "..", "key", "550e8400-e29b-41d4-a716-446655440000").is_err());
        assert!(multipart_upload_dir(root, "bucket", "../x", "550e8400-e29b-41d4-a716-446655440000").is_err());
        assert!(multipart_upload_dir(root, "bucket", "key", "not-a-uuid").is_err());
    }
}
