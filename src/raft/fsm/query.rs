//! Read helpers over the FSM state. All functions take a shared
//! lock, clone the subset they return, and release. Reads are
//! therefore locally consistent with the latest applied Raft log
//! entry on this node. Linearizable reads (round trip through the
//! primary) will be added as a separate helper when we need them.

use super::{AbixioStateMachine, Member, Placement};
use crate::storage::metadata::BucketSettings;

pub fn bucket_settings(fsm: &AbixioStateMachine, bucket: &str) -> Option<BucketSettings> {
    fsm.bucket_settings(bucket)
}

pub fn members(fsm: &AbixioStateMachine) -> Vec<Member> {
    fsm.members()
}

pub fn placement(fsm: &AbixioStateMachine) -> Placement {
    fsm.placement()
}
