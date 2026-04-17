//! Snapshot store. Each snapshot is a single file on disk named
//! `snap-<index>.msgpack` containing a bincode-encoded header
//! followed by the msgpack-encoded FSM state:
//! ```text
//! +--------------------+----------------------------+
//! | SnapshotMeta bincode | State msgpack             |
//! +--------------------+----------------------------+
//! ```
//! The meta is small and fixed-shape so consumers can decode it
//! without pulling the whole state into memory. Writing is atomic
//! via temp file + rename; after a successful write the store
//! purges older snapshots, keeping only the last two.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::raft::fsm::State;

/// Fixed-shape header describing a snapshot. Persisted as a length-
/// prefixed bincode blob at the start of the snapshot file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    pub last_applied_index: u64,
    pub last_applied_term: u64,
    pub created_at_unix_secs: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bincode: {0}")]
    Bincode(#[from] bincode::Error),
    #[error("msgpack encode: {0}")]
    MsgpackEncode(#[from] rmp_serde::encode::Error),
    #[error("msgpack decode: {0}")]
    MsgpackDecode(#[from] rmp_serde::decode::Error),
}

const HEADER_LEN_BYTES: usize = 4; // u32 little-endian

pub struct SnapshotStore {
    dir: PathBuf,
    /// Upper bound on snapshots kept on disk. The newest + one older
    /// is enough: on a crash mid-install, the previous snapshot is
    /// still readable.
    max_kept: usize,
}

impl SnapshotStore {
    pub fn open(dir: &Path) -> Result<Self, SnapshotError> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            max_kept: 2,
        })
    }

    /// Write a new snapshot. Returns the `SnapshotMeta` that was
    /// persisted. Older snapshots beyond `max_kept` are deleted.
    pub fn store(
        &self,
        last_applied_index: u64,
        last_applied_term: u64,
        state: &State,
    ) -> Result<SnapshotMeta, SnapshotError> {
        let meta = SnapshotMeta {
            last_applied_index,
            last_applied_term,
            created_at_unix_secs: unix_now_secs(),
        };
        let body = rmp_serde::to_vec(state)?;
        let header = bincode::serialize(&meta)?;
        if header.len() > u32::MAX as usize {
            return Err(SnapshotError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "meta too large",
            )));
        }

        let final_path = self.path_for(last_applied_index);
        let tmp = final_path.with_extension("msgpack.tmp");
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&(header.len() as u32).to_le_bytes())?;
            f.write_all(&header)?;
            f.write_all(&body)?;
            f.sync_data()?;
        }
        fs::rename(&tmp, &final_path)?;

        self.prune_older_than(last_applied_index)?;
        Ok(meta)
    }

    /// Load the newest on-disk snapshot, if any.
    pub fn load_latest(&self) -> Result<Option<(SnapshotMeta, State)>, SnapshotError> {
        let mut candidates = self.list_indexes()?;
        candidates.sort();
        let Some(latest) = candidates.last().copied() else {
            return Ok(None);
        };
        let path = self.path_for(latest);
        let mut f = fs::OpenOptions::new().read(true).open(&path)?;
        let mut header_len_bytes = [0u8; HEADER_LEN_BYTES];
        f.read_exact(&mut header_len_bytes)?;
        let header_len = u32::from_le_bytes(header_len_bytes) as usize;
        let mut header_buf = vec![0u8; header_len];
        f.read_exact(&mut header_buf)?;
        let meta: SnapshotMeta = bincode::deserialize(&header_buf)?;
        let mut body = Vec::new();
        f.read_to_end(&mut body)?;
        let state: State = rmp_serde::from_slice(&body)?;
        Ok(Some((meta, state)))
    }

    /// Enumerate snapshot indexes currently on disk. Exposed for
    /// tests and the prune path.
    pub fn list_indexes(&self) -> Result<Vec<u64>, SnapshotError> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(SnapshotError::Io(e)),
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(stripped) = name
                .strip_prefix("snap-")
                .and_then(|s| s.strip_suffix(".msgpack"))
            {
                if let Ok(i) = stripped.parse::<u64>() {
                    out.push(i);
                }
            }
        }
        Ok(out)
    }

    fn prune_older_than(&self, latest: u64) -> Result<(), SnapshotError> {
        let mut indexes = self.list_indexes()?;
        indexes.sort();
        // keep the last `max_kept` entries that are <= latest
        if indexes.len() <= self.max_kept {
            return Ok(());
        }
        let drop_until = indexes.len().saturating_sub(self.max_kept);
        for idx in indexes.iter().take(drop_until) {
            if *idx >= latest {
                // defensively don't delete the just-written one
                continue;
            }
            let _ = fs::remove_file(self.path_for(*idx));
        }
        Ok(())
    }

    fn path_for(&self, index: u64) -> PathBuf {
        self.dir.join(format!("snap-{}.msgpack", index))
    }
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::fsm::ops::{Op, State, VoterKind};
    use tempfile::TempDir;

    fn seeded_state() -> State {
        let mut s = State::default();
        Op::AddNode {
            node_id: "a".into(),
            advertise_s3: "http://a".into(),
            advertise_cluster: "http://a".into(),
            voter_kind: VoterKind::Voter,
            added_at_unix_secs: 1700000000,
        }
        .apply(&mut s)
        .unwrap();
        s
    }

    #[test]
    fn store_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let s = SnapshotStore::open(dir.path()).unwrap();
        let state = seeded_state();
        let meta = s.store(42, 7, &state).unwrap();
        assert_eq!(meta.last_applied_index, 42);
        assert_eq!(meta.last_applied_term, 7);

        let (got_meta, got_state) = s.load_latest().unwrap().unwrap();
        assert_eq!(got_meta, meta);
        assert_eq!(got_state, state);
    }

    #[test]
    fn load_empty_returns_none() {
        let dir = TempDir::new().unwrap();
        let s = SnapshotStore::open(dir.path()).unwrap();
        assert!(s.load_latest().unwrap().is_none());
    }

    #[test]
    fn prune_keeps_last_two() {
        let dir = TempDir::new().unwrap();
        let s = SnapshotStore::open(dir.path()).unwrap();
        let state = State::default();
        for i in [10u64, 20, 30, 40] {
            s.store(i, 1, &state).unwrap();
        }
        let mut idx = s.list_indexes().unwrap();
        idx.sort();
        assert_eq!(idx, vec![30, 40]);
    }

    #[test]
    fn latest_picks_highest_index() {
        let dir = TempDir::new().unwrap();
        let s = SnapshotStore::open(dir.path()).unwrap();
        let state = seeded_state();
        s.store(10, 1, &state).unwrap();
        s.store(25, 1, &state).unwrap();
        let (meta, _) = s.load_latest().unwrap().unwrap();
        assert_eq!(meta.last_applied_index, 25);
    }
}
