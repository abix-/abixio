use md5::{Digest, Md5};
use sha2::Sha256;

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

/// Verify a shard checksum. Tries blake3 first (new format), falls back to SHA256
/// (legacy format) for backward compatibility with existing on-disk data.
pub fn verify_shard_checksum(data: &[u8], stored_checksum: &str) -> bool {
    blake3_hex(data) == stored_checksum || sha256_hex(data) == stored_checksum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hello() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn md5_hello() {
        assert_eq!(md5_hex(b"hello"), "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn sha256_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn md5_empty() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
    }
}
