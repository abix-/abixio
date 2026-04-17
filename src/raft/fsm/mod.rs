//! Abixio's Raft finite state machine. Holds the three tables the
//! design doc calls out: cluster membership, placement topology,
//! bucket settings. The FSM is pure in-memory; snapshots are the
//! durable form. The Raft log driver (future landing) feeds `Op`s
//! into `apply`; reads take a shared lock and return owned copies.

use std::sync::RwLock;

use crate::storage::metadata::BucketSettings;

pub mod ops;
pub mod query;

pub use ops::{ApplyError, Member, Op, Placement, PlacementVolume, State, VoterKind};

/// The state machine. Sits inside an `Arc` on the abixio side. The
/// `applied_index` cursor tracks the highest Raft log index applied
/// so far; future openraft integration uses this to answer
/// "when is my commit visible?" queries.
pub struct AbixioStateMachine {
    state: RwLock<State>,
    applied_index: RwLock<u64>,
}

impl AbixioStateMachine {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(State::default()),
            applied_index: RwLock::new(0),
        }
    }

    /// Apply one op at the given Raft log index. The index must
    /// strictly increase. Returns the new applied index or the
    /// op-level error.
    pub fn apply(&self, index: u64, op: &Op) -> Result<u64, ApplyError> {
        {
            let mut state = self
                .state
                .write()
                .expect("fsm state lock poisoned");
            op.apply(&mut *state)?;
        }
        let mut cursor = self
            .applied_index
            .write()
            .expect("applied_index lock poisoned");
        if index > *cursor {
            *cursor = index;
        }
        Ok(*cursor)
    }

    pub fn applied_index(&self) -> u64 {
        *self.applied_index.read().expect("applied_index lock poisoned")
    }

    /// Snapshot the full state for serialization. Caller can feed the
    /// returned value to `rmp_serde::to_vec` or similar.
    pub fn snapshot(&self) -> (u64, State) {
        let index = self.applied_index();
        let state = self
            .state
            .read()
            .expect("fsm state lock poisoned")
            .clone();
        (index, state)
    }

    /// Replace the state wholesale from a snapshot. Used when a
    /// secondary installs a snapshot from the primary.
    pub fn install_snapshot(&self, index: u64, state: State) {
        let mut cursor = self
            .applied_index
            .write()
            .expect("applied_index lock poisoned");
        *cursor = index;
        let mut guard = self
            .state
            .write()
            .expect("fsm state lock poisoned");
        *guard = state;
    }

    /// Read a bucket's settings. Returns None if no entry is known.
    pub fn bucket_settings(&self, bucket: &str) -> Option<BucketSettings> {
        self.state
            .read()
            .expect("fsm state lock poisoned")
            .buckets
            .get(bucket)
            .cloned()
    }

    pub fn member(&self, node_id: &str) -> Option<Member> {
        self.state
            .read()
            .expect("fsm state lock poisoned")
            .members
            .get(node_id)
            .cloned()
    }

    pub fn placement(&self) -> Placement {
        self.state
            .read()
            .expect("fsm state lock poisoned")
            .placement
            .clone()
    }

    pub fn members(&self) -> Vec<Member> {
        self.state
            .read()
            .expect("fsm state lock poisoned")
            .members
            .values()
            .cloned()
            .collect()
    }
}

impl Default for AbixioStateMachine {
    fn default() -> Self {
        Self::new()
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
    fn apply_monotonic_index() {
        let fsm = AbixioStateMachine::new();
        let a = fsm.apply(1, &add_node("a")).unwrap();
        assert_eq!(a, 1);
        let b = fsm.apply(2, &add_node("b")).unwrap();
        assert_eq!(b, 2);
        assert_eq!(fsm.applied_index(), 2);
        assert_eq!(fsm.members().len(), 2);
    }

    #[test]
    fn snapshot_install_replaces_state() {
        let fsm = AbixioStateMachine::new();
        fsm.apply(1, &add_node("a")).unwrap();
        let (idx, snap) = fsm.snapshot();
        assert_eq!(idx, 1);

        let fsm2 = AbixioStateMachine::new();
        fsm2.install_snapshot(idx, snap);
        assert_eq!(fsm2.applied_index(), 1);
        assert!(fsm2.member("a").is_some());
    }

    #[test]
    fn bucket_settings_round_trip() {
        let fsm = AbixioStateMachine::new();
        let mut settings = BucketSettings::default();
        settings.ftt = Some(3);
        fsm.apply(
            1,
            &Op::SetBucketSettings {
                bucket: "b".into(),
                settings: settings.clone(),
            },
        )
        .unwrap();
        let got = fsm.bucket_settings("b").unwrap();
        assert_eq!(got.ftt, Some(3));
    }

    #[test]
    fn placement_tracks_latest_epoch() {
        let fsm = AbixioStateMachine::new();
        fsm.apply(
            1,
            &Op::SetPlacementEpoch {
                epoch_id: 5,
                cluster_id: "c1".into(),
                volumes: vec![],
            },
        )
        .unwrap();
        assert_eq!(fsm.placement().epoch_id, 5);
    }
}
