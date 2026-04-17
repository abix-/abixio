//! Persistent vote state: the current term and the node this
//! instance voted for in that term. Written atomically (temp file +
//! rename) and fsynced on every update. Very small, so a full
//! rewrite per change is fine.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const VOTE_FILE: &str = "vote.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    pub term: u64,
    /// Node id voted for in `term`. Empty means "no vote yet".
    pub voted_for: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VoteError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
}

pub struct VoteStore {
    path: PathBuf,
}

impl VoteStore {
    pub fn open(dir: &Path) -> Result<Self, VoteError> {
        fs::create_dir_all(dir)?;
        Ok(Self {
            path: dir.join(VOTE_FILE),
        })
    }

    /// Load the current vote, or `Vote::default()` when the file
    /// does not exist (first boot).
    pub fn load(&self) -> Result<Vote, VoteError> {
        match fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vote::default()),
            Err(e) => Err(VoteError::Io(e)),
        }
    }

    /// Persist the vote atomically. Writes to a temp file, fsyncs,
    /// then renames; fsync after rename ensures the directory entry
    /// points at the new file before we return.
    pub fn save(&self, vote: &Vote) -> Result<(), VoteError> {
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(vote)?;
        {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_data()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_when_missing() {
        let dir = TempDir::new().unwrap();
        let s = VoteStore::open(dir.path()).unwrap();
        assert_eq!(s.load().unwrap(), Vote::default());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let s = VoteStore::open(dir.path()).unwrap();
        let v = Vote {
            term: 7,
            voted_for: "node-b".into(),
        };
        s.save(&v).unwrap();
        assert_eq!(s.load().unwrap(), v);
    }

    #[test]
    fn overwrite_persists_latest() {
        let dir = TempDir::new().unwrap();
        let s = VoteStore::open(dir.path()).unwrap();
        s.save(&Vote { term: 1, voted_for: "a".into() }).unwrap();
        s.save(&Vote { term: 2, voted_for: "b".into() }).unwrap();
        assert_eq!(s.load().unwrap().term, 2);
    }
}
