//! Typed operations that go into the Raft log. Each `Op` variant is
//! a single atomic change to the cluster state machine. Serialization
//! uses bincode for compactness on the wire; the log entry is an
//! `Op` bincode blob.
//!
//! Adding a variant is a format-version change: older nodes must be
//! upgraded before they can replay a log containing the new variant,
//! or a snapshot install needs to bring them past it. Keep ops small
//! and composable.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::storage::metadata::BucketSettings;

/// Role a member plays in the Raft cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VoterKind {
    /// Participates in quorum.
    Voter,
    /// Replicates the log but does not vote. Reserved for future
    /// read-replica work; no voter-observer transitions shipped yet.
    Observer,
}

/// A placement volume as tracked inside the FSM. Mirrors the shape
/// already in `crate::cluster::placement::PlacementVolume`, but
/// lives here so the FSM has zero upstream dependencies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementVolume {
    pub backend_index: usize,
    pub node_id: String,
    pub volume_id: String,
}

/// Every mutation committed through Raft is one of these.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Op {
    /// Add a node to cluster membership. The primary issues this
    /// after a successful `/_admin/raft/join` from the new node.
    AddNode {
        node_id: String,
        advertise_s3: String,
        advertise_cluster: String,
        voter_kind: VoterKind,
        added_at_unix_secs: u64,
    },
    /// Remove a node from cluster membership.
    RemoveNode { node_id: String },
    /// Replace the placement topology atomically. `epoch_id` must
    /// be strictly greater than the current epoch.
    SetPlacementEpoch {
        epoch_id: u64,
        cluster_id: String,
        volumes: Vec<PlacementVolume>,
    },
    /// Upsert bucket settings. The apply function copies the value
    /// into the state table; readers see the new value on the next
    /// local FSM read after commit.
    SetBucketSettings {
        bucket: String,
        settings: BucketSettings,
    },
    /// Remove a bucket's settings entry.
    DeleteBucketSettings { bucket: String },
}

/// FSM state (the three tables the plan specifies). Serialized whole
/// for snapshots via msgpack.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// Cluster membership keyed by `node_id`.
    pub members: HashMap<String, Member>,
    /// Placement topology.
    pub placement: Placement,
    /// Bucket settings keyed by bucket name.
    pub buckets: HashMap<String, BucketSettings>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub node_id: String,
    pub advertise_s3: String,
    pub advertise_cluster: String,
    pub voter_kind: VoterKind,
    pub added_at_unix_secs: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub epoch_id: u64,
    pub cluster_id: String,
    pub volumes: Vec<PlacementVolume>,
}

/// Error returned by `Op::apply` when the op cannot be committed to
/// the provided state. The Raft log never contains invalid ops in
/// steady state — the primary rejects malformed submissions. This
/// error is for defensive programming + tests.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApplyError {
    #[error("epoch must increase monotonically: have {have}, got {got}")]
    EpochRegression { have: u64, got: u64 },
    #[error("node {0} not found")]
    UnknownNode(String),
    #[error("bucket {0} not found")]
    UnknownBucket(String),
}

impl Op {
    /// Apply this op to `state` in-place. Pure — no IO, no async.
    /// Returns on first validation failure; the caller never sees a
    /// half-applied state because every variant mutates at most one
    /// table in one step.
    pub fn apply(&self, state: &mut State) -> Result<(), ApplyError> {
        match self {
            Op::AddNode {
                node_id,
                advertise_s3,
                advertise_cluster,
                voter_kind,
                added_at_unix_secs,
            } => {
                state.members.insert(
                    node_id.clone(),
                    Member {
                        node_id: node_id.clone(),
                        advertise_s3: advertise_s3.clone(),
                        advertise_cluster: advertise_cluster.clone(),
                        voter_kind: *voter_kind,
                        added_at_unix_secs: *added_at_unix_secs,
                    },
                );
                Ok(())
            }
            Op::RemoveNode { node_id } => {
                if state.members.remove(node_id).is_none() {
                    return Err(ApplyError::UnknownNode(node_id.clone()));
                }
                Ok(())
            }
            Op::SetPlacementEpoch {
                epoch_id,
                cluster_id,
                volumes,
            } => {
                if *epoch_id <= state.placement.epoch_id && state.placement.epoch_id != 0 {
                    return Err(ApplyError::EpochRegression {
                        have: state.placement.epoch_id,
                        got: *epoch_id,
                    });
                }
                state.placement = Placement {
                    epoch_id: *epoch_id,
                    cluster_id: cluster_id.clone(),
                    volumes: volumes.clone(),
                };
                Ok(())
            }
            Op::SetBucketSettings { bucket, settings } => {
                state.buckets.insert(bucket.clone(), settings.clone());
                Ok(())
            }
            Op::DeleteBucketSettings { bucket } => {
                if state.buckets.remove(bucket).is_none() {
                    return Err(ApplyError::UnknownBucket(bucket.clone()));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_node(id: &str) -> Op {
        Op::AddNode {
            node_id: id.to_string(),
            advertise_s3: format!("http://{}:10000", id),
            advertise_cluster: format!("http://{}:10000", id),
            voter_kind: VoterKind::Voter,
            added_at_unix_secs: 1700000000,
        }
    }

    #[test]
    fn add_and_remove_node() {
        let mut s = State::default();
        add_node("a").apply(&mut s).unwrap();
        add_node("b").apply(&mut s).unwrap();
        assert_eq!(s.members.len(), 2);

        Op::RemoveNode { node_id: "a".into() }.apply(&mut s).unwrap();
        assert_eq!(s.members.len(), 1);
        assert!(s.members.contains_key("b"));
    }

    #[test]
    fn remove_unknown_node_errors() {
        let mut s = State::default();
        let err = Op::RemoveNode { node_id: "nope".into() }
            .apply(&mut s)
            .unwrap_err();
        assert_eq!(err, ApplyError::UnknownNode("nope".into()));
    }

    #[test]
    fn epoch_must_increase() {
        let mut s = State::default();
        Op::SetPlacementEpoch {
            epoch_id: 5,
            cluster_id: "c1".into(),
            volumes: vec![],
        }
        .apply(&mut s)
        .unwrap();
        let err = Op::SetPlacementEpoch {
            epoch_id: 3,
            cluster_id: "c1".into(),
            volumes: vec![],
        }
        .apply(&mut s)
        .unwrap_err();
        assert_eq!(err, ApplyError::EpochRegression { have: 5, got: 3 });
    }

    #[test]
    fn bucket_settings_upsert_and_delete() {
        let mut s = State::default();
        let mut settings = BucketSettings::default();
        settings.ftt = Some(2);
        Op::SetBucketSettings {
            bucket: "b".into(),
            settings: settings.clone(),
        }
        .apply(&mut s)
        .unwrap();
        assert_eq!(s.buckets.get("b").unwrap().ftt, Some(2));

        Op::DeleteBucketSettings { bucket: "b".into() }
            .apply(&mut s)
            .unwrap();
        assert!(s.buckets.is_empty());
    }

    #[test]
    fn op_roundtrips_through_bincode() {
        let op = add_node("a");
        let bytes = bincode::serialize(&op).unwrap();
        let got: Op = bincode::deserialize(&bytes).unwrap();
        assert_eq!(got, op);
    }

    #[test]
    fn state_roundtrips_through_msgpack() {
        let mut s = State::default();
        add_node("a").apply(&mut s).unwrap();
        add_node("b").apply(&mut s).unwrap();
        Op::SetPlacementEpoch {
            epoch_id: 2,
            cluster_id: "c1".into(),
            volumes: vec![PlacementVolume {
                backend_index: 0,
                node_id: "a".into(),
                volume_id: "v-a-0".into(),
            }],
        }
        .apply(&mut s)
        .unwrap();

        let bytes = rmp_serde::to_vec(&s).unwrap();
        let got: State = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(got, s);
    }
}
