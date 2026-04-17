//! openraft `RaftStateMachine` + `RaftSnapshotBuilder` adapter over
//! our `AbixioStateMachine` + `SnapshotStore`.
//!
//! openraft tracks two pieces of metadata alongside the app state
//! that our own FSM does not model: the last-applied `LogId`, and
//! the last-seen membership configuration. We hold both in local
//! Arc<Mutex<..>>s. They get persisted into the snapshot body so
//! secondaries learn them on install.

use std::io::Cursor;
use std::sync::Arc;

use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine, Snapshot};
use openraft::{
    EntryPayload, LogId, RaftTypeConfig, SnapshotMeta as OrSnapshotMeta, StorageError,
    StorageIOError, StoredMembership,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::raft::fsm::ops::State;
use crate::raft::fsm::AbixioStateMachine;
use crate::raft::storage::snapshot::SnapshotStore;
use crate::raft::types::{AbixioNode, AppResponse, NodeId, TypeConfig};

#[derive(Clone)]
pub struct AbixioFsmAdapter {
    fsm: Arc<AbixioStateMachine>,
    snap_store: Arc<Mutex<SnapshotStore>>,
    last_applied: Arc<Mutex<Option<LogId<NodeId>>>>,
    last_membership: Arc<Mutex<StoredMembership<NodeId, AbixioNode>>>,
    last_snapshot_id: Arc<Mutex<u64>>,
}

/// Payload we serialize into a snapshot body. Bundles the FSM state
/// with the two openraft-maintained metadata fields so a secondary
/// can rebuild both views from a single install.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SnapshotBody {
    state: State,
    last_applied: Option<LogId<NodeId>>,
    last_membership: StoredMembership<NodeId, AbixioNode>,
}

impl AbixioFsmAdapter {
    pub fn new(fsm: AbixioStateMachine, snap_store: SnapshotStore) -> Self {
        Self {
            fsm: Arc::new(fsm),
            snap_store: Arc::new(Mutex::new(snap_store)),
            last_applied: Arc::new(Mutex::new(None)),
            last_membership: Arc::new(Mutex::new(StoredMembership::default())),
            last_snapshot_id: Arc::new(Mutex::new(0)),
        }
    }

    pub fn state_machine(&self) -> Arc<AbixioStateMachine> {
        Arc::clone(&self.fsm)
    }
}

fn to_storage_err<E: std::fmt::Display>(e: E) -> StorageError<NodeId> {
    StorageIOError::read_logs(&openraft::AnyError::error(e.to_string())).into()
}

impl RaftSnapshotBuilder<TypeConfig> for AbixioFsmAdapter {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let last_applied = self.last_applied.lock().await.clone();
        let last_membership = self.last_membership.lock().await.clone();
        let (applied_idx, state) = self.fsm.snapshot();
        let body = SnapshotBody {
            state,
            last_applied: last_applied.clone(),
            last_membership: last_membership.clone(),
        };
        let bytes = rmp_serde::to_vec(&body).map_err(to_storage_err)?;

        let applied_term = last_applied
            .as_ref()
            .map(|l| l.leader_id.get_term())
            .unwrap_or(0);
        self.snap_store
            .lock()
            .await
            .store(applied_idx, applied_term, &body.state)
            .map_err(to_storage_err)?;

        let snap_id = {
            let mut g = self.last_snapshot_id.lock().await;
            *g += 1;
            format!("snap-{}-{}", applied_idx, *g)
        };
        Ok(Snapshot {
            meta: OrSnapshotMeta {
                last_log_id: last_applied,
                last_membership,
                snapshot_id: snap_id,
            },
            snapshot: Box::new(Cursor::new(bytes)),
        })
    }
}

impl RaftStateMachine<TypeConfig> for AbixioFsmAdapter {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<
        (Option<LogId<NodeId>>, StoredMembership<NodeId, AbixioNode>),
        StorageError<NodeId>,
    > {
        let applied = self.last_applied.lock().await.clone();
        let membership = self.last_membership.lock().await.clone();
        Ok((applied, membership))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<AppResponse>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = <TypeConfig as RaftTypeConfig>::Entry> + Send,
        I::IntoIter: Send,
    {
        let mut out = Vec::new();
        for entry in entries {
            let index = entry.log_id.index;
            match entry.payload {
                EntryPayload::Blank => {
                    // Blank entries advance the log without touching
                    // FSM data. openraft emits them at leader startup
                    // and for no-op heartbeats. Bump last_applied
                    // only -- our FSM has no "noop" op and does not
                    // need to record blanks.
                }
                EntryPayload::Normal(ref op) => {
                    self.fsm
                        .apply(index, op)
                        .map_err(|e| to_storage_err(e.to_string()))?;
                }
                EntryPayload::Membership(ref m) => {
                    let stored = StoredMembership::new(Some(entry.log_id.clone()), m.clone());
                    *self.last_membership.lock().await = stored;
                }
            }
            *self.last_applied.lock().await = Some(entry.log_id);
            out.push(AppResponse { applied_index: index });
        }
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &OrSnapshotMeta<NodeId, AbixioNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let bytes = snapshot.into_inner();
        let body: SnapshotBody = rmp_serde::from_slice(&bytes).map_err(to_storage_err)?;
        let idx = meta.last_log_id.as_ref().map(|l| l.index).unwrap_or(0);
        self.fsm.install_snapshot(idx, body.state);
        *self.last_applied.lock().await = meta.last_log_id.clone();
        *self.last_membership.lock().await = meta.last_membership.clone();
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        let loaded = self.snap_store.lock().await.load_latest().map_err(to_storage_err)?;
        let Some((meta, state)) = loaded else {
            return Ok(None);
        };
        let last_applied = self.last_applied.lock().await.clone();
        let last_membership = self.last_membership.lock().await.clone();
        let body = SnapshotBody {
            state,
            last_applied: last_applied.clone(),
            last_membership: last_membership.clone(),
        };
        let bytes = rmp_serde::to_vec(&body).map_err(to_storage_err)?;
        let snap_id = format!("snap-{}-replay", meta.last_applied_index);
        Ok(Some(Snapshot {
            meta: OrSnapshotMeta {
                last_log_id: last_applied,
                last_membership,
                snapshot_id: snap_id,
            },
            snapshot: Box::new(Cursor::new(bytes)),
        }))
    }
}

