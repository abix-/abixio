//! Log segment files for log-structured storage.
//!
//! Each segment is a pre-allocated append-only file containing needles.
//! Segments go through a lifecycle: NEW -> ACTIVE -> SEALED -> (GC) -> DEAD.
//! Sealed segments are mmap'd for zero-copy GET reads.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::needle::{self, Needle, NeedleLocation, HEADER_SIZE};
use super::StorageError;

/// Segment superblock magic.
const SEGMENT_MAGIC: [u8; 4] = *b"ABXL";

/// Superblock size in bytes.
pub const SUPERBLOCK_SIZE: usize = 32;

/// Default segment pre-allocation size (64MB).
pub const DEFAULT_SEGMENT_SIZE: usize = 64 * 1024 * 1024;

/// Superblock layout (32 bytes).
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Superblock {
    magic: [u8; 4],
    version: u32,
    segment_id: u32,
    created_at: u64,
    _reserved: [u8; 12],
}

/// An active segment open for appending.
pub struct ActiveSegment {
    segment_id: u32,
    path: PathBuf,
    file: std::fs::File,
    write_offset: usize,
    capacity: usize,
}

/// A sealed segment, read-only and mmap'd for GET.
pub struct SealedSegment {
    segment_id: u32,
    path: PathBuf,
    mmap: memmap2::Mmap,
    size: usize, // valid data size (not pre-allocated size)
}

impl ActiveSegment {
    /// Create a new pre-allocated segment file.
    pub fn create(dir: &Path, segment_id: u32, capacity: usize) -> Result<Self, StorageError> {
        std::fs::create_dir_all(dir)?;
        let filename = format!("segment-{:06}.dat", segment_id);
        let path = dir.join(&filename);

        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;

        // pre-allocate
        file.set_len(capacity as u64)?;

        // write superblock
        let created_at = super::metadata::unix_timestamp_secs();
        let superblock = Superblock {
            magic: SEGMENT_MAGIC,
            version: 1,
            segment_id,
            created_at,
            _reserved: [0u8; 12],
        };
        let sb_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &superblock as *const Superblock as *const u8,
                SUPERBLOCK_SIZE,
            )
        };
        {
            let mut writer = std::io::BufWriter::new(&file);
            writer.write_all(sb_bytes)?;
            writer.flush()?;
        } // drop BufWriter to release borrow

        Ok(Self {
            segment_id,
            path,
            file,
            write_offset: SUPERBLOCK_SIZE,
            capacity,
        })
    }

    /// Append a needle to this segment. Returns the needle location.
    /// Returns None if the needle doesn't fit (segment full).
    pub fn append(&mut self, needle: &Needle) -> Result<Option<NeedleLocation>, StorageError> {
        let buf = needle.serialize();
        if self.write_offset + buf.len() > self.capacity {
            return Ok(None); // segment full
        }

        self.file.seek(SeekFrom::Start(self.write_offset as u64))?;
        self.file.write_all(&buf)?;

        let needle_offset = self.write_offset as u32;

        // compute field offsets within the segment
        let bucket_len = needle.bucket.len();
        let key_len = needle.key.len();
        let meta_offset = needle_offset + HEADER_SIZE as u32 + bucket_len as u32 + key_len as u32;
        let data_offset = meta_offset + needle.meta_bytes.len() as u32;

        let location = NeedleLocation {
            segment_id: self.segment_id,
            offset: needle_offset,
            meta_offset,
            meta_len: needle.meta_bytes.len() as u16,
            data_offset,
            data_len: needle.data.len() as u32,
            created_at: 0, // caller sets from ObjectMeta
        };

        self.write_offset += buf.len();
        Ok(Some(location))
    }

    /// Fsync the segment file to disk.
    pub fn fsync(&self) -> Result<(), StorageError> {
        self.file.sync_all()?;
        Ok(())
    }

    /// How many bytes are available for more needles.
    pub fn remaining(&self) -> usize {
        self.capacity.saturating_sub(self.write_offset)
    }

    /// Whether this segment is full (can't fit even a minimal needle).
    pub fn is_full(&self) -> bool {
        self.remaining() < HEADER_SIZE + 64 // minimum needle ~88 bytes
    }

    /// Current write offset (valid data size).
    pub fn data_size(&self) -> usize {
        self.write_offset
    }

    /// Segment ID.
    pub fn id(&self) -> u32 {
        self.segment_id
    }

    /// Seal this segment: flush, convert to read-only mmap.
    pub fn seal(self) -> Result<SealedSegment, StorageError> {
        self.file.sync_all()?;
        let size = self.write_offset;
        // re-open read-only for mmap
        let file = std::fs::File::open(&self.path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        Ok(SealedSegment {
            segment_id: self.segment_id,
            path: self.path,
            mmap,
            size,
        })
    }
}

impl SealedSegment {
    /// Open an existing sealed segment from disk and mmap it.
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| StorageError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;

        if mmap.len() < SUPERBLOCK_SIZE {
            return Err(StorageError::Internal("segment too small for superblock".into()));
        }

        // read superblock
        let sb: Superblock = unsafe {
            std::ptr::read_unaligned(mmap.as_ptr() as *const Superblock)
        };
        let sb_magic = { sb.magic };
        let sb_segment_id = { sb.segment_id };

        if sb_magic != SEGMENT_MAGIC {
            return Err(StorageError::Internal("bad segment magic".into()));
        }

        // scan to find valid data size (find last valid needle)
        let mut offset = SUPERBLOCK_SIZE;
        while offset + HEADER_SIZE <= mmap.len() {
            match Needle::read_header(&mmap[offset..]) {
                Ok((_, _, _, _, _, _, _, total)) => offset += total,
                Err(_) => break, // end of valid data
            }
        }

        Ok(Self {
            segment_id: sb_segment_id,
            path: path.to_path_buf(),
            mmap,
            size: offset,
        })
    }

    /// Get a byte slice from this segment's mmap at the given offset and length.
    pub fn slice(&self, offset: usize, len: usize) -> Option<&[u8]> {
        if offset + len <= self.mmap.len() {
            Some(&self.mmap[offset..offset + len])
        } else {
            None
        }
    }

    /// Get the full mmap as a Bytes (for zero-copy GET).
    pub fn as_bytes(&self) -> &[u8] {
        &self.mmap
    }

    /// Scan all needles in this segment, calling the visitor for each.
    /// Visitor receives: (flags, bucket, key, meta_offset, meta_len, data_offset, data_len, needle_offset).
    pub fn scan<F>(&self, mut visitor: F) -> Result<(), StorageError>
    where
        F: FnMut(u8, &str, &str, usize, usize, usize, usize, usize),
    {
        let mut offset = SUPERBLOCK_SIZE;
        while offset + HEADER_SIZE <= self.size {
            match Needle::read_header(&self.mmap[offset..]) {
                Ok((flags, bucket, key, meta_off, meta_len, data_off, data_len, total)) => {
                    // offsets are relative to needle start, convert to segment-absolute
                    visitor(
                        flags,
                        &bucket,
                        &key,
                        offset + meta_off,
                        meta_len,
                        offset + data_off,
                        data_len,
                        offset,
                    );
                    offset += total;
                }
                Err(_) => break,
            }
        }
        Ok(())
    }

    /// Segment ID.
    pub fn id(&self) -> u32 {
        self.segment_id
    }

    /// Valid data size (not pre-allocated size).
    pub fn data_size(&self) -> usize {
        self.size
    }

    /// File path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::metadata::{ErasureMeta, ObjectMeta};
    use tempfile::TempDir;

    fn test_meta() -> ObjectMeta {
        ObjectMeta {
            size: 4096,
            etag: "abc".to_string(),
            content_type: "text/plain".to_string(),
            created_at: 1700000000,
            erasure: ErasureMeta {
                ftt: 0,
                index: 0,
                epoch_id: 1,
                volume_ids: vec!["v1".to_string()],
            },
            checksum: "dead".to_string(),
            user_metadata: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
            version_id: String::new(),
            is_latest: true,
            is_delete_marker: false,
            parts: Vec::new(),
        }
    }

    #[test]
    fn create_and_append() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("log");
        let mut seg = ActiveSegment::create(&dir, 1, 1024 * 1024).unwrap();

        let needle = Needle::new("bucket", "key1", &test_meta(), &[1, 2, 3]).unwrap();
        let loc = seg.append(&needle).unwrap().unwrap();
        assert_eq!(loc.segment_id, 1);
        assert_eq!(loc.offset, SUPERBLOCK_SIZE as u32);
        assert!(loc.data_len > 0);

        // append another
        let needle2 = Needle::new("bucket", "key2", &test_meta(), &[4, 5, 6]).unwrap();
        let loc2 = seg.append(&needle2).unwrap().unwrap();
        assert!(loc2.offset > loc.offset);
    }

    #[test]
    fn segment_full_returns_none() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("log");
        // tiny segment: superblock + barely any room
        let mut seg = ActiveSegment::create(&dir, 1, SUPERBLOCK_SIZE + 50).unwrap();

        let needle = Needle::new("bucket", "key", &test_meta(), &[1; 100]).unwrap();
        let result = seg.append(&needle).unwrap();
        assert!(result.is_none()); // doesn't fit
    }

    #[test]
    fn seal_and_read() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("log");
        let mut seg = ActiveSegment::create(&dir, 1, 1024 * 1024).unwrap();

        let data = vec![0x42u8; 100];
        let needle = Needle::new("b", "k", &test_meta(), &data).unwrap();
        let loc = seg.append(&needle).unwrap().unwrap();
        seg.fsync().unwrap();

        let sealed = seg.seal().unwrap();
        // read shard data via mmap slice
        let slice = sealed.slice(loc.data_offset as usize, loc.data_len as usize).unwrap();
        assert_eq!(slice, &data[..]);

        // read meta
        let meta_slice = sealed.slice(loc.meta_offset as usize, loc.meta_len as usize).unwrap();
        let meta = Needle::decode_meta(meta_slice).unwrap();
        assert_eq!(meta.size, 4096);
    }

    #[test]
    fn open_existing_segment() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("log");
        let mut seg = ActiveSegment::create(&dir, 42, 1024 * 1024).unwrap();

        let needle1 = Needle::new("b", "k1", &test_meta(), &[1, 2, 3]).unwrap();
        let needle2 = Needle::new("b", "k2", &test_meta(), &[4, 5, 6]).unwrap();
        seg.append(&needle1).unwrap();
        seg.append(&needle2).unwrap();
        seg.fsync().unwrap();

        let path = dir.join("segment-000042.dat");
        let sealed = SealedSegment::open(&path).unwrap();
        assert_eq!(sealed.id(), 42);

        // scan should find both needles
        let mut count = 0;
        sealed.scan(|flags, bucket, key, _, _, _, _, _| {
            assert_eq!(flags, needle::FLAG_NORMAL);
            assert_eq!(bucket, "b");
            assert!(key == "k1" || key == "k2");
            count += 1;
        }).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn scan_with_tombstone() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("log");
        let mut seg = ActiveSegment::create(&dir, 1, 1024 * 1024).unwrap();

        let needle1 = Needle::new("b", "k1", &test_meta(), &[1]).unwrap();
        let tomb = Needle::tombstone("b", "k1");
        seg.append(&needle1).unwrap();
        seg.append(&tomb).unwrap();
        seg.fsync().unwrap();

        let sealed = seg.seal().unwrap();
        let mut entries = Vec::new();
        sealed.scan(|flags, _, key, _, _, _, _, _| {
            entries.push((flags, key.to_string()));
        }).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, needle::FLAG_NORMAL);
        assert_eq!(entries[1].0, needle::FLAG_DELETE);
    }

    #[test]
    fn partial_write_stops_scan() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("log");
        let mut seg = ActiveSegment::create(&dir, 1, 1024 * 1024).unwrap();

        let needle1 = Needle::new("b", "k1", &test_meta(), &[1, 2, 3]).unwrap();
        seg.append(&needle1).unwrap();
        seg.fsync().unwrap();

        let path = dir.join("segment-000001.dat");

        // corrupt the area after the first needle (simulates partial write)
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            f.seek(SeekFrom::Start(seg.data_size() as u64)).unwrap();
            f.write_all(&[0xFF; 24]).unwrap(); // garbage header
        }

        let sealed = SealedSegment::open(&path).unwrap();
        let mut count = 0;
        sealed.scan(|_, _, _, _, _, _, _, _| { count += 1; }).unwrap();
        assert_eq!(count, 1); // only the first valid needle
    }
}
