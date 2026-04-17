//! openraft TypeConfig for abixio's control-plane Raft.
//!
//! The three app-specific types are:
//!
//! - `NodeId = String` — we already use string UUIDs for node ids
//! - `AbixioNode` — advertise addresses + voter kind
//! - `Op` — the app-data enum from `fsm::ops`
//! - `AppResponse` — returned from `apply`; carries the applied
//!   index or an apply-time error string for defensive logging
//!
//! The snapshot body is a plain in-memory buffer (`Cursor<Vec<u8>>`)
//! because we expect the FSM state to stay small (MB, not GB). If
//! that ever changes we swap it for a file-backed reader.

use std::io::Cursor;

use openraft::TokioRuntime;
use serde::{Deserialize, Serialize};

use super::fsm::ops::{Op, VoterKind};

/// The public per-member view advertised through Raft. Populated by
/// `Op::AddNode`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbixioNode {
    pub advertise_s3: String,
    pub advertise_cluster: String,
    pub voter_kind: VoterKind,
}

impl Default for AbixioNode {
    fn default() -> Self {
        Self {
            advertise_s3: String::new(),
            advertise_cluster: String::new(),
            voter_kind: VoterKind::Voter,
        }
    }
}

impl std::fmt::Display for AbixioNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.advertise_cluster)
    }
}

/// Per-`apply` response. openraft returns these out of its
/// `Raft::client_write`. We carry the applied index so callers can
/// observe commit ordering in logs + tests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppResponse {
    pub applied_index: u64,
}

/// openraft NodeId. We use a `u64` internally for raft-layer
/// identity; the human-readable string `node_id` is kept as the
/// payload of `AbixioNode` and used at the admin API boundary.
/// Mapping between the two lives in `RaftIdMap` (future landing).
pub type NodeId = u64;

openraft::declare_raft_types! {
    /// Type config wiring up the adapter to openraft.
    pub TypeConfig:
        D = Op,
        R = AppResponse,
        NodeId = NodeId,
        Node = AbixioNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = TokioRuntime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abixio_node_default() {
        let n = AbixioNode::default();
        assert_eq!(n.voter_kind, VoterKind::Voter);
    }

    #[test]
    fn abixio_node_round_trip() {
        let n = AbixioNode {
            advertise_s3: "http://a:10000".into(),
            advertise_cluster: "http://a:10000".into(),
            voter_kind: VoterKind::Voter,
        };
        let bytes = bincode::serialize(&n).unwrap();
        let got: AbixioNode = bincode::deserialize(&bytes).unwrap();
        assert_eq!(got, n);
    }
}
