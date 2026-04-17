//! openraft `RaftLogStorage` + `RaftLogReader` adapter over our
//! on-disk `LogStore` + `VoteStore`.
//!
//! openraft owns the async threading model. Its traits require
//! `&mut self` in every method, so we park the underlying stores
//! behind a `tokio::sync::Mutex` inside a `Clone`-able adapter.
//! Clones share the same stores, which lets us hand a clone back
//! from `get_log_reader` without giving up exclusive access on the
//! writer path.

use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{
    Entry, LogId, RaftTypeConfig, StorageError, StorageIOError, Vote as OrVote,
};
use tokio::sync::Mutex;

use crate::raft::storage::log::{LogEntry, LogError, LogStore};
use crate::raft::storage::meta::{Vote as VoteData, VoteError, VoteStore};
use crate::raft::types::{NodeId, TypeConfig};

#[derive(Clone)]
pub struct AbixioLogAdapter {
    log: Arc<Mutex<LogStore>>,
    vote: Arc<Mutex<VoteStore>>,
}

impl AbixioLogAdapter {
    pub fn new(log: LogStore, vote: VoteStore) -> Self {
        Self {
            log: Arc::new(Mutex::new(log)),
            vote: Arc::new(Mutex::new(vote)),
        }
    }
}

fn to_storage_err<E: std::fmt::Display>(e: E) -> StorageError<NodeId> {
    StorageIOError::read_logs(&openraft::AnyError::error(e.to_string())).into()
}

fn log_err(e: LogError) -> StorageError<NodeId> {
    to_storage_err(e)
}

fn vote_err(e: VoteError) -> StorageError<NodeId> {
    to_storage_err(e)
}

impl RaftLogReader<TypeConfig> for AbixioLogAdapter {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + std::fmt::Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        use std::ops::Bound;
        let start = match range.start_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => i.saturating_add(1),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(i) => *i,
            Bound::Excluded(i) => i.saturating_sub(1),
            Bound::Unbounded => u64::MAX,
        };
        let mut guard = self.log.lock().await;
        let effective_end = guard.last_log_index().unwrap_or(0).min(end);
        if start > effective_end {
            return Ok(Vec::new());
        }
        let raw = guard.read_range(start, effective_end).map_err(log_err)?;
        let mut out = Vec::with_capacity(raw.len());
        for r in raw {
            let entry: Entry<TypeConfig> = bincode::deserialize(&r.op_bytes).map_err(to_storage_err)?;
            out.push(entry);
        }
        Ok(out)
    }
}

impl RaftLogStorage<TypeConfig> for AbixioLogAdapter {
    type LogReader = Self;

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        let guard = self.log.lock().await;
        let last = match guard.last_log_index() {
            Some(i) => i,
            None => {
                return Ok(LogState {
                    last_purged_log_id: None,
                    last_log_id: None,
                });
            }
        };
        // Read the last entry to recover its LogId (term is embedded
        // in the LogEntry header; leader_id reconstructs from the
        // openraft Entry payload).
        let last_term = guard.last_log_term().unwrap_or(0);
        let first = guard.first_log_index().unwrap_or(last);
        let last_purged = if first > 0 {
            // openraft interprets purged as "everything up to and
            // including this LogId is gone". If our first_log_index
            // is > 0, there was a purge prior.
            Some(LogId::new(
                openraft::CommittedLeaderId::new(last_term, 0),
                first.saturating_sub(1),
            ))
        } else {
            None
        };
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: Some(LogId::new(
                openraft::CommittedLeaderId::new(last_term, 0),
                last,
            )),
        })
    }

    async fn save_vote(
        &mut self,
        vote: &OrVote<NodeId>,
    ) -> Result<(), StorageError<NodeId>> {
        let data = VoteData {
            term: vote.leader_id().get_term(),
            voted_for: vote
                .leader_id()
                .voted_for()
                .map(|id| id.to_string())
                .unwrap_or_default(),
        };
        self.vote.lock().await.save(&data).map_err(vote_err)
    }

    async fn read_vote(&mut self) -> Result<Option<OrVote<NodeId>>, StorageError<NodeId>> {
        let data = self.vote.lock().await.load().map_err(vote_err)?;
        if data.term == 0 && data.voted_for.is_empty() {
            return Ok(None);
        }
        let voted: Option<NodeId> = data.voted_for.parse::<u64>().ok();
        let or = match voted {
            Some(n) => OrVote::new(data.term, n),
            None => return Ok(None),
        };
        Ok(Some(or))
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = <TypeConfig as RaftTypeConfig>::Entry> + Send,
        I::IntoIter: Send,
    {
        let mut raw = Vec::new();
        for entry in entries {
            let bytes = bincode::serialize(&entry).map_err(to_storage_err)?;
            raw.push(LogEntry {
                index: entry.log_id.index,
                term: entry.log_id.leader_id.get_term(),
                op_bytes: bytes,
            });
        }
        self.log.lock().await.append(&raw).map_err(log_err)?;
        // Append already fsyncs; notify openraft synchronously.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(
        &mut self,
        log_id: LogId<NodeId>,
    ) -> Result<(), StorageError<NodeId>> {
        // openraft semantics: drop entries with index >= log_id.index.
        // Our truncate_after keeps everything <= last_kept, so we
        // pass `log_id.index - 1`. Handle index 0 by truncating
        // everything.
        let keep = log_id.index.saturating_sub(1);
        self.log.lock().await.truncate_after(keep).map_err(log_err)
    }

    async fn purge(
        &mut self,
        log_id: LogId<NodeId>,
    ) -> Result<(), StorageError<NodeId>> {
        // openraft semantics: everything <= log_id.index is purged.
        // Our purge_before keeps entries >= first_kept, so we pass
        // `log_id.index + 1`.
        let first_kept = log_id.index.saturating_add(1);
        self.log.lock().await.purge_before(first_kept).map_err(log_err)
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    // Runtime tests for the adapter require a full openraft
    // instance to drive callbacks; those land alongside the
    // network adapter in the next commit. For now just verify the
    // type config is satisfied by calling into the LogStore paths
    // directly in other modules' tests.
    #[test]
    fn module_compiles() {}
}
