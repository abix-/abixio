//! `AbixioRaft` — high-level handle that owns the openraft instance,
//! the log + snapshot + vote stores, and the state machine. Built
//! once per node at startup, cloned into everything that needs to
//! call out to Raft (admin endpoints, storage server RPC handlers,
//! VolumePool bucket-settings writes, singleton gates).
//!
//! Nothing here is wired into `main.rs` yet; the wrapper exists so
//! the next landing can swap its VolumePool calls for Raft submits
//! without also standing up the instance at the same time.

use std::path::Path;
use std::sync::Arc;

use openraft::{Config, Raft};

use crate::raft::adapter::{AbixioFsmAdapter, AbixioLogAdapter, AbixioNetworkFactory};
use crate::raft::fsm::AbixioStateMachine;
use crate::raft::storage::{LogStore, SnapshotStore, VoteStore};
use crate::raft::types::{AppResponse, NodeId, TypeConfig};

pub type AbixioRaftInstance = Raft<TypeConfig>;

pub struct AbixioRaft {
    pub node_id: NodeId,
    pub raft: AbixioRaftInstance,
    pub fsm: Arc<AbixioStateMachine>,
}

impl AbixioRaft {
    /// Build the instance on top of a raft state directory. The
    /// directory holds log.dat, vote.json, and snap-*.msgpack. One
    /// directory per node (not shared across disks).
    pub async fn open(
        node_id: NodeId,
        dir: &Path,
        access_key: String,
        secret_key: String,
        no_auth: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let log_store = LogStore::open(dir)?;
        let vote_store = VoteStore::open(dir)?;
        let snap_store = SnapshotStore::open(dir)?;

        let fsm_inner = AbixioStateMachine::new();
        let fsm_adapter = AbixioFsmAdapter::new(fsm_inner, snap_store);
        let fsm_handle = fsm_adapter.state_machine();

        let log_adapter = AbixioLogAdapter::new(log_store, vote_store);
        let network = AbixioNetworkFactory::new(access_key, secret_key, no_auth);

        let config = Config {
            heartbeat_interval: 150,
            election_timeout_min: 300,
            election_timeout_max: 600,
            max_payload_entries: 512,
            ..Default::default()
        };
        let config = Arc::new(config.validate()?);

        let raft = Raft::new(node_id, config, network, log_adapter, fsm_adapter).await?;
        Ok(Self { node_id, raft, fsm: fsm_handle })
    }

    /// True when this node is the Raft leader. Used by singleton
    /// task gates (lifecycle, scanner).
    pub async fn is_primary(&self) -> bool {
        matches!(
            self.raft.metrics().borrow().state,
            openraft::ServerState::Leader,
        )
    }

    /// Submit a typed op through the cluster. On success returns the
    /// committed `AppResponse.applied_index`. Returns the openraft
    /// client-write error unchanged for the caller to map into HTTP.
    pub async fn submit(
        &self,
        op: crate::raft::fsm::Op,
    ) -> Result<
        AppResponse,
        openraft::error::RaftError<
            NodeId,
            openraft::error::ClientWriteError<NodeId, crate::raft::types::AbixioNode>,
        >,
    > {
        let resp = self.raft.client_write(op).await?;
        Ok(resp.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raft::fsm::ops::VoterKind;
    use crate::raft::types::AbixioNode;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    #[tokio::test]
    async fn single_node_bootstrap_submit_and_read() {
        let dir = TempDir::new().unwrap();
        let handle = AbixioRaft::open(
            1,
            dir.path(),
            String::new(),
            String::new(),
            true,
        )
        .await
        .unwrap();

        // Bootstrap a single-node cluster: initial membership is just
        // this node. openraft requires every voter be announced via
        // `initialize`.
        let mut members = BTreeMap::new();
        members.insert(
            1u64,
            AbixioNode {
                advertise_s3: "http://localhost:10000".into(),
                advertise_cluster: "http://localhost:10000".into(),
                voter_kind: VoterKind::Voter,
            },
        );
        handle.raft.initialize(members).await.unwrap();

        // Wait for leadership. Single-node election should complete
        // within a heartbeat.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if handle.is_primary().await {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("node did not become primary within 2s");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Submit an AddNode op through Raft; observe it in the FSM.
        let resp = handle
            .submit(crate::raft::fsm::Op::AddNode {
                node_id: "node-b".into(),
                advertise_s3: "http://b:10000".into(),
                advertise_cluster: "http://b:10000".into(),
                voter_kind: VoterKind::Voter,
                added_at_unix_secs: 1700000000,
            })
            .await
            .unwrap();

        assert!(resp.applied_index > 0);
        assert!(handle.fsm.member("node-b").is_some());

        handle.raft.shutdown().await.unwrap();
    }
}
