//! Needle format for log-structured storage.
//!
//! Each needle is a self-describing, checksummed record containing one shard
//! of one object (metadata + data packed together). Needles are appended to
//! segment files and read back via mmap.

use super::metadata::ObjectMeta;
use super::StorageError;

/// Needle header magic number.
const NEEDLE_MAGIC: u32 = 0xAB1C_4E00;

/// Header size in bytes.
pub const HEADER_SIZE: usize = 24;

/// Needle flags.
pub const FLAG_NORMAL: u8 = 0;
pub const FLAG_DELETE: u8 = 1;

/// Serialized needle header (24 bytes, fixed layout).
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct RawHeader {
    magic: u32,
    flags: u8,
    bucket_len: u8,
    key_len: u16,
    meta_len: u16,
    data_len: u32,
    checksum: u64,
    _pad: u16,
}

/// A needle ready to be appended to a segment.
pub struct Needle {
    pub flags: u8,
    pub bucket: String,
    pub key: String,
    pub meta_bytes: Vec<u8>,  // msgpack-serialized ObjectMeta
    pub data: Vec<u8>,        // shard data
}

/// Location of a needle within a segment (returned after append).
#[derive(Debug, Clone, Copy)]
pub struct NeedleLocation {
    pub segment_id: u32,
    pub offset: u32,
    pub meta_offset: u32, // absolute offset of meta within segment
    pub meta_len: u16,
    pub data_offset: u32, // absolute offset of data within segment
    pub data_len: u32,
    pub created_at: u32,
}

impl Needle {
    /// Create a needle for a normal PUT.
    pub fn new(bucket: &str, key: &str, meta: &ObjectMeta, shard_data: &[u8]) -> Result<Self, StorageError> {
        let meta_bytes = rmp_serde::to_vec(meta)
            .map_err(|e| StorageError::Internal(format!("msgpack serialize: {}", e)))?;
        Ok(Self {
            flags: FLAG_NORMAL,
            bucket: bucket.to_string(),
            key: key.to_string(),
            meta_bytes,
            data: shard_data.to_vec(),
        })
    }

    /// Create a tombstone needle for DELETE.
    pub fn tombstone(bucket: &str, key: &str) -> Self {
        Self {
            flags: FLAG_DELETE,
            bucket: bucket.to_string(),
            key: key.to_string(),
            meta_bytes: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Total serialized size of this needle.
    pub fn serialized_size(&self) -> usize {
        HEADER_SIZE + self.bucket.len() + self.key.len() + self.meta_bytes.len() + self.data.len()
    }

    /// Write the needle directly into a destination buffer (e.g. mmap).
    /// Zero allocation, one copy of each field. The destination must be
    /// at least `serialized_size()` bytes. Returns bytes written.
    pub fn serialize_into(&self, dest: &mut [u8]) -> usize {
        let total = self.serialized_size();
        debug_assert!(dest.len() >= total);

        // write payload fields after header, then compute checksum over
        // the payload region in-place (no intermediate Vec)
        let mut off = HEADER_SIZE;
        dest[off..off + self.bucket.len()].copy_from_slice(self.bucket.as_bytes());
        off += self.bucket.len();
        dest[off..off + self.key.len()].copy_from_slice(self.key.as_bytes());
        off += self.key.len();
        dest[off..off + self.meta_bytes.len()].copy_from_slice(&self.meta_bytes);
        off += self.meta_bytes.len();
        dest[off..off + self.data.len()].copy_from_slice(&self.data);
        off += self.data.len();

        // checksum over payload region already in dest
        let checksum = xxhash_rust::xxh64::xxh64(&dest[HEADER_SIZE..total], 0);

        // write header at the front
        let header = RawHeader {
            magic: NEEDLE_MAGIC,
            flags: self.flags,
            bucket_len: self.bucket.len() as u8,
            key_len: self.key.len() as u16,
            meta_len: self.meta_bytes.len() as u16,
            data_len: self.data.len() as u32,
            checksum,
            _pad: 0,
        };
        // SAFETY: RawHeader is repr(C, packed), no padding, all primitive types
        let header_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &header as *const RawHeader as *const u8,
                HEADER_SIZE,
            )
        };
        dest[..HEADER_SIZE].copy_from_slice(header_bytes);

        total
    }

    /// Serialize the needle into a byte buffer for appending to a segment.
    /// Prefer `serialize_into` when writing to mmap to avoid allocation.
    pub fn serialize(&self) -> Vec<u8> {
        let total = self.serialized_size();
        let mut buf = vec![0u8; total];
        self.serialize_into(&mut buf);
        buf
    }

    /// Deserialize a needle from a byte slice (e.g. mmap region).
    /// Returns (needle_flags, bucket, key, meta_bytes, data, total_size) or error.
    pub fn deserialize(buf: &[u8]) -> Result<(u8, String, String, Vec<u8>, Vec<u8>, usize), StorageError> {
        if buf.len() < HEADER_SIZE {
            return Err(StorageError::Internal("needle too short for header".into()));
        }

        // read header
        let header: RawHeader = unsafe {
            std::ptr::read_unaligned(buf.as_ptr() as *const RawHeader)
        };

        let h_magic = { header.magic };
        let h_flags = { header.flags };
        let h_bucket_len = { header.bucket_len };
        let h_key_len = { header.key_len };
        let h_meta_len = { header.meta_len };
        let h_data_len = { header.data_len };
        let h_checksum = { header.checksum };

        if h_magic != NEEDLE_MAGIC {
            return Err(StorageError::Internal(format!(
                "bad needle magic: {:#x}", h_magic
            )));
        }

        let bucket_len = h_bucket_len as usize;
        let key_len = h_key_len as usize;
        let meta_len = h_meta_len as usize;
        let data_len = h_data_len as usize;
        let total = HEADER_SIZE + bucket_len + key_len + meta_len + data_len;

        if buf.len() < total {
            return Err(StorageError::Internal("needle truncated".into()));
        }

        // verify checksum
        let payload = &buf[HEADER_SIZE..total];
        let computed = xxhash_rust::xxh64::xxh64(payload, 0);
        if computed != h_checksum {
            return Err(StorageError::Internal(format!(
                "needle checksum mismatch: expected {:#x}, got {:#x}",
                h_checksum, computed
            )));
        }

        let mut offset = HEADER_SIZE;
        let bucket = std::str::from_utf8(&buf[offset..offset + bucket_len])
            .map_err(|e| StorageError::Internal(format!("bad bucket utf8: {}", e)))?
            .to_string();
        offset += bucket_len;

        let key = std::str::from_utf8(&buf[offset..offset + key_len])
            .map_err(|e| StorageError::Internal(format!("bad key utf8: {}", e)))?
            .to_string();
        offset += key_len;

        let meta_bytes = buf[offset..offset + meta_len].to_vec();
        offset += meta_len;

        let data = buf[offset..offset + data_len].to_vec();

        Ok((h_flags, bucket, key, meta_bytes, data, total))
    }

    /// Read needle metadata from a byte slice without copying data.
    /// Returns (flags, bucket, key, meta_offset, meta_len, data_offset, data_len, total_size).
    pub fn read_header(buf: &[u8]) -> Result<(u8, String, String, usize, usize, usize, usize, usize), StorageError> {
        if buf.len() < HEADER_SIZE {
            return Err(StorageError::Internal("needle too short for header".into()));
        }

        let header: RawHeader = unsafe {
            std::ptr::read_unaligned(buf.as_ptr() as *const RawHeader)
        };

        let h_magic = { header.magic };
        let h_flags = { header.flags };
        let h_bucket_len = { header.bucket_len };
        let h_key_len = { header.key_len };
        let h_meta_len = { header.meta_len };
        let h_data_len = { header.data_len };
        let h_checksum = { header.checksum };

        if h_magic != NEEDLE_MAGIC {
            return Err(StorageError::Internal(format!(
                "bad needle magic: {:#x}", h_magic
            )));
        }

        let bucket_len = h_bucket_len as usize;
        let key_len = h_key_len as usize;
        let meta_len = h_meta_len as usize;
        let data_len = h_data_len as usize;
        let total = HEADER_SIZE + bucket_len + key_len + meta_len + data_len;

        if buf.len() < total {
            return Err(StorageError::Internal("needle truncated".into()));
        }

        // verify checksum
        let payload = &buf[HEADER_SIZE..total];
        let computed = xxhash_rust::xxh64::xxh64(payload, 0);
        if computed != h_checksum {
            return Err(StorageError::Internal("needle checksum mismatch".into()));
        }

        let bucket_offset = HEADER_SIZE;
        let bucket = std::str::from_utf8(&buf[bucket_offset..bucket_offset + bucket_len])
            .map_err(|e| StorageError::Internal(format!("bad bucket utf8: {}", e)))?
            .to_string();
        let key_offset = bucket_offset + bucket_len;
        let key = std::str::from_utf8(&buf[key_offset..key_offset + key_len])
            .map_err(|e| StorageError::Internal(format!("bad key utf8: {}", e)))?
            .to_string();
        let meta_offset = key_offset + key_len;
        let data_offset = meta_offset + meta_len;

        Ok((h_flags, bucket, key, meta_offset, meta_len, data_offset, data_len, total))
    }

    /// Deserialize ObjectMeta from msgpack bytes.
    pub fn decode_meta(meta_bytes: &[u8]) -> Result<ObjectMeta, StorageError> {
        rmp_serde::from_slice(meta_bytes)
            .map_err(|e| StorageError::Internal(format!("msgpack deserialize: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::metadata::{ErasureMeta, ObjectMeta};

    fn test_meta() -> ObjectMeta {
        ObjectMeta {
            size: 4096,
            etag: "abc123".to_string(),
            content_type: "application/octet-stream".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 1,
                index: 0,
                epoch_id: 1,
                volume_ids: vec!["vol-1".to_string()],
            },
            checksum: "deadbeef".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
                inline_data: None,
        }
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let meta = test_meta();
        let data = vec![0x42u8; 1365]; // ~1.3KB shard
        let needle = Needle::new("mybucket", "config.json", &meta, &data).unwrap();

        let buf = needle.serialize();
        assert!(buf.len() > HEADER_SIZE);

        let (flags, bucket, key, meta_bytes, got_data, total) =
            Needle::deserialize(&buf).unwrap();
        assert_eq!(flags, FLAG_NORMAL);
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "config.json");
        assert_eq!(got_data, data);
        assert_eq!(total, buf.len());

        let got_meta = Needle::decode_meta(&meta_bytes).unwrap();
        assert_eq!(got_meta.size, 4096);
        assert_eq!(got_meta.etag, "abc123");
    }

    #[test]
    fn tombstone_round_trip() {
        let needle = Needle::tombstone("mybucket", "deleted.txt");
        let buf = needle.serialize();

        let (flags, bucket, key, meta_bytes, data, _) =
            Needle::deserialize(&buf).unwrap();
        assert_eq!(flags, FLAG_DELETE);
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "deleted.txt");
        assert!(meta_bytes.is_empty());
        assert!(data.is_empty());
    }

    #[test]
    fn checksum_detects_corruption() {
        let meta = test_meta();
        let needle = Needle::new("b", "k", &meta, &[1, 2, 3]).unwrap();
        let mut buf = needle.serialize();

        // corrupt one byte in the data section
        let last = buf.len() - 1;
        buf[last] ^= 0xFF;

        assert!(Needle::deserialize(&buf).is_err());
    }

    #[test]
    fn bad_magic_rejected() {
        let buf = vec![0u8; 64];
        assert!(Needle::deserialize(&buf).is_err());
    }

    #[test]
    fn truncated_needle_rejected() {
        let meta = test_meta();
        let needle = Needle::new("b", "k", &meta, &[1, 2, 3]).unwrap();
        let buf = needle.serialize();

        // truncate
        assert!(Needle::deserialize(&buf[..HEADER_SIZE + 2]).is_err());
    }

    #[test]
    fn read_header_returns_offsets() {
        let meta = test_meta();
        let data = vec![0x42u8; 100];
        let needle = Needle::new("mybucket", "obj", &meta, &data).unwrap();
        let buf = needle.serialize();

        let (flags, bucket, key, meta_offset, meta_len, data_offset, data_len, total) =
            Needle::read_header(&buf).unwrap();
        assert_eq!(flags, FLAG_NORMAL);
        assert_eq!(bucket, "mybucket");
        assert_eq!(key, "obj");
        assert_eq!(data_len, 100);
        assert_eq!(total, buf.len());

        // verify offsets point to correct data
        assert_eq!(&buf[data_offset..data_offset + data_len], &data[..]);

        let got_meta = Needle::decode_meta(&buf[meta_offset..meta_offset + meta_len]).unwrap();
        assert_eq!(got_meta.size, 4096);
    }

    #[test]
    fn msgpack_smaller_than_json() {
        let meta = test_meta();
        let json_size = serde_json::to_vec(&meta).unwrap().len();
        let msgpack_size = rmp_serde::to_vec(&meta).unwrap().len();
        // msgpack should be significantly smaller
        assert!(msgpack_size < json_size, "msgpack {} >= json {}", msgpack_size, json_size);
    }
}
