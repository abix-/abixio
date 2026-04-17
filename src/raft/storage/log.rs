//! Append-only Raft log store.
//!
//! Record layout on disk (little-endian, fixed-size header):
//! ```text
//! +------------+------------+------------+-----------+
//! | index u64  | term  u64  | len u32    | op bytes  |
//! +------------+------------+------------+-----------+
//! ```
//! Entries are bincode-serialized `Op` values from
//! `crate::raft::fsm::ops::Op`, but the log store is agnostic to
//! the payload — it just stores opaque bytes. `LogEntry` holds the
//! raw bytes so the caller decodes after read.
//!
//! Design notes:
//! - Records are fsynced on commit (`append` + explicit `sync`).
//! - An in-memory index maps `log_index -> byte_offset` for O(1)
//!   seeks; rebuilt on open by scanning the file from 0.
//! - `truncate_after(i)` and `purge_before(i)` are both expressible
//!   as file-level rewrites; they are expected to be rare (conflict
//!   resolution and post-snapshot compaction).
//! - No segmentation. A single `log.dat` is fine until a cluster
//!   runs long enough to make it inconvenient; a future landing can
//!   add rotation without changing the record format.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const LOG_FILE: &str = "log.dat";
const HEADER_LEN: usize = 8 + 8 + 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    pub index: u64,
    pub term: u64,
    pub op_bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("log corruption: {0}")]
    Corruption(String),
    #[error("entry not found: index {0}")]
    NotFound(u64),
    #[error("non-monotonic append: expected index > {prev}, got {got}")]
    NonMonotonic { prev: u64, got: u64 },
}

pub struct LogStore {
    path: PathBuf,
    file: File,
    /// index -> (byte offset, term). Keyed by log index, ordered for
    /// cheap `last_log_id` / `first_log_id` queries.
    index: BTreeMap<u64, (u64, u64)>,
    file_len: u64,
}

impl LogStore {
    /// Open or create the log at `<dir>/log.dat`. Rebuilds the
    /// in-memory index by scanning existing records.
    pub fn open(dir: &Path) -> Result<Self, LogError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(LOG_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        let mut store = Self {
            path,
            file,
            index: BTreeMap::new(),
            file_len: 0,
        };
        store.rebuild_index()?;
        Ok(store)
    }

    fn rebuild_index(&mut self) -> Result<(), LogError> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(&mut self.file);
        let mut offset: u64 = 0;
        let mut header = [0u8; HEADER_LEN];
        loop {
            match reader.read_exact(&mut header) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(LogError::Io(e)),
            }
            let index = u64::from_le_bytes(header[0..8].try_into().unwrap());
            let term = u64::from_le_bytes(header[8..16].try_into().unwrap());
            let len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
            // Skip the body.
            reader.seek_relative(len as i64).map_err(LogError::Io)?;
            self.index.insert(index, (offset, term));
            offset += HEADER_LEN as u64 + len as u64;
        }
        self.file_len = offset;
        Ok(())
    }

    /// Append entries. Indexes must strictly increase and exceed any
    /// existing index in the log. Callers should call
    /// `truncate_after` first if they need to overwrite a conflicting
    /// suffix.
    pub fn append(&mut self, entries: &[LogEntry]) -> Result<(), LogError> {
        let mut prev = self.last_log_index().unwrap_or(0);
        self.file.seek(SeekFrom::End(0))?;
        let mut offset = self.file_len;
        for e in entries {
            if prev != 0 && e.index <= prev {
                return Err(LogError::NonMonotonic { prev, got: e.index });
            }
            let len = e.op_bytes.len();
            if len > u32::MAX as usize {
                return Err(LogError::Corruption("entry too large".into()));
            }
            let mut header = [0u8; HEADER_LEN];
            header[0..8].copy_from_slice(&e.index.to_le_bytes());
            header[8..16].copy_from_slice(&e.term.to_le_bytes());
            header[16..20].copy_from_slice(&(len as u32).to_le_bytes());
            self.file.write_all(&header)?;
            self.file.write_all(&e.op_bytes)?;
            self.index.insert(e.index, (offset, e.term));
            offset += HEADER_LEN as u64 + len as u64;
            prev = e.index;
        }
        self.file_len = offset;
        self.file.sync_data()?;
        Ok(())
    }

    pub fn read(&mut self, index: u64) -> Result<LogEntry, LogError> {
        let (offset, _term) = *self
            .index
            .get(&index)
            .ok_or(LogError::NotFound(index))?;
        self.file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; HEADER_LEN];
        self.file.read_exact(&mut header)?;
        let got_index = u64::from_le_bytes(header[0..8].try_into().unwrap());
        let term = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let len = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
        if got_index != index {
            return Err(LogError::Corruption(format!(
                "index mismatch at offset {}: wanted {}, got {}",
                offset, index, got_index
            )));
        }
        let mut op_bytes = vec![0u8; len];
        self.file.read_exact(&mut op_bytes)?;
        Ok(LogEntry { index, term, op_bytes })
    }

    /// Read every entry in `[start, end]` inclusive.
    pub fn read_range(&mut self, start: u64, end: u64) -> Result<Vec<LogEntry>, LogError> {
        if end < start {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity((end - start + 1) as usize);
        for idx in start..=end {
            if self.index.contains_key(&idx) {
                out.push(self.read(idx)?);
            }
        }
        Ok(out)
    }

    /// Remove entries with index > `last_kept`. Used to reconcile
    /// the log with a new primary that has overwritten the tail.
    /// Rewrites the file to the first `last_kept` entries and rebuilds
    /// the index. Rare.
    pub fn truncate_after(&mut self, last_kept: u64) -> Result<(), LogError> {
        let cut_offset = match self.index.get(&last_kept) {
            Some(&(off, _)) => {
                // include entry at last_kept; start discarding after
                // the end of that record
                let entry = self.read(last_kept)?;
                off + HEADER_LEN as u64 + entry.op_bytes.len() as u64
            }
            None => {
                // nothing at or past last_kept exists; no-op
                if self.last_log_index().unwrap_or(0) <= last_kept {
                    return Ok(());
                }
                0
            }
        };
        self.file.set_len(cut_offset)?;
        self.file.sync_data()?;
        self.file_len = cut_offset;
        self.index = self
            .index
            .split_off(&0)
            .into_iter()
            .filter(|(i, _)| *i <= last_kept)
            .collect();
        Ok(())
    }

    /// Remove entries with index < `first_kept`. Typical after a
    /// snapshot install: prefix of the log is no longer needed.
    /// Implemented as a rewrite to a temp file + atomic rename.
    pub fn purge_before(&mut self, first_kept: u64) -> Result<(), LogError> {
        let kept_indexes: Vec<u64> = self
            .index
            .range(first_kept..)
            .map(|(i, _)| *i)
            .collect();
        if kept_indexes.is_empty() && self.index.is_empty() {
            return Ok(());
        }
        if kept_indexes.len() == self.index.len() {
            // nothing to drop
            return Ok(());
        }

        let tmp_path = self.path.with_extension("dat.tmp");
        let mut tmp = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;

        // copy kept entries sequentially
        let mut offsets: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
        let mut new_offset: u64 = 0;
        for idx in &kept_indexes {
            let entry = self.read(*idx)?;
            let len = entry.op_bytes.len() as u32;
            let mut header = [0u8; HEADER_LEN];
            header[0..8].copy_from_slice(&entry.index.to_le_bytes());
            header[8..16].copy_from_slice(&entry.term.to_le_bytes());
            header[16..20].copy_from_slice(&len.to_le_bytes());
            tmp.write_all(&header)?;
            tmp.write_all(&entry.op_bytes)?;
            offsets.insert(entry.index, (new_offset, entry.term));
            new_offset += HEADER_LEN as u64 + len as u64;
        }
        tmp.sync_data()?;
        drop(tmp);

        // atomic rename
        std::fs::rename(&tmp_path, &self.path)?;
        // re-open file + rebuild index
        self.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?;
        self.index = offsets;
        self.file_len = new_offset;
        Ok(())
    }

    pub fn last_log_index(&self) -> Option<u64> {
        self.index.keys().next_back().copied()
    }

    pub fn last_log_term(&self) -> Option<u64> {
        self.index.values().next_back().map(|(_, t)| *t)
    }

    pub fn first_log_index(&self) -> Option<u64> {
        self.index.keys().next().copied()
    }

    pub fn entry_count(&self) -> usize {
        self.index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entry(i: u64, t: u64, body: &[u8]) -> LogEntry {
        LogEntry {
            index: i,
            term: t,
            op_bytes: body.to_vec(),
        }
    }

    #[test]
    fn open_append_read_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut s = LogStore::open(dir.path()).unwrap();
        s.append(&[entry(1, 1, b"one"), entry(2, 1, b"two")]).unwrap();
        assert_eq!(s.last_log_index(), Some(2));
        assert_eq!(s.last_log_term(), Some(1));
        assert_eq!(s.read(1).unwrap().op_bytes, b"one");
        assert_eq!(s.read(2).unwrap().op_bytes, b"two");
    }

    #[test]
    fn reopen_rebuilds_index() {
        let dir = TempDir::new().unwrap();
        {
            let mut s = LogStore::open(dir.path()).unwrap();
            s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 2, b"c")])
                .unwrap();
        }
        let mut s2 = LogStore::open(dir.path()).unwrap();
        assert_eq!(s2.entry_count(), 3);
        assert_eq!(s2.read(2).unwrap().op_bytes, b"b");
        assert_eq!(s2.last_log_term(), Some(2));
    }

    #[test]
    fn monotonic_append_enforced() {
        let dir = TempDir::new().unwrap();
        let mut s = LogStore::open(dir.path()).unwrap();
        s.append(&[entry(5, 1, b"x")]).unwrap();
        let err = s.append(&[entry(3, 1, b"y")]).unwrap_err();
        assert!(matches!(err, LogError::NonMonotonic { .. }));
    }

    #[test]
    fn truncate_after_drops_tail() {
        let dir = TempDir::new().unwrap();
        let mut s = LogStore::open(dir.path()).unwrap();
        s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 1, b"c")])
            .unwrap();
        s.truncate_after(1).unwrap();
        assert_eq!(s.last_log_index(), Some(1));
        assert!(s.read(2).is_err());
        // reopen and verify persistence
        let s2 = LogStore::open(dir.path()).unwrap();
        assert_eq!(s2.last_log_index(), Some(1));
    }

    #[test]
    fn purge_before_drops_prefix() {
        let dir = TempDir::new().unwrap();
        let mut s = LogStore::open(dir.path()).unwrap();
        s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 1, b"c"), entry(4, 1, b"d")])
            .unwrap();
        s.purge_before(3).unwrap();
        assert_eq!(s.first_log_index(), Some(3));
        assert_eq!(s.last_log_index(), Some(4));
        assert!(s.read(1).is_err());
        // reopen and verify persistence
        let s2 = LogStore::open(dir.path()).unwrap();
        assert_eq!(s2.first_log_index(), Some(3));
    }

    #[test]
    fn read_range_inclusive() {
        let dir = TempDir::new().unwrap();
        let mut s = LogStore::open(dir.path()).unwrap();
        s.append(&[entry(1, 1, b"a"), entry(2, 1, b"b"), entry(3, 2, b"c")])
            .unwrap();
        let got = s.read_range(1, 3).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].term, 2);
    }

    #[test]
    fn read_missing_index_returns_not_found() {
        let dir = TempDir::new().unwrap();
        let mut s = LogStore::open(dir.path()).unwrap();
        s.append(&[entry(1, 1, b"a")]).unwrap();
        assert!(matches!(s.read(99), Err(LogError::NotFound(99))));
    }

    #[test]
    fn variable_length_entries() {
        let dir = TempDir::new().unwrap();
        let mut s = LogStore::open(dir.path()).unwrap();
        s.append(&[
            entry(1, 1, b"tiny"),
            entry(2, 1, &vec![0xAA; 5000]),
            entry(3, 1, b""),
        ])
        .unwrap();
        assert_eq!(s.read(2).unwrap().op_bytes.len(), 5000);
        assert!(s.read(3).unwrap().op_bytes.is_empty());
    }
}
