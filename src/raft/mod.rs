//! Raft-backed control plane (design: `docs/raft.md`).
//!
//! This landing ships the data model only:
//!
//! - `fsm::Op` — the typed mutation enum that would land in the
//!   Raft log
//! - `fsm::AbixioStateMachine` — in-memory state + applied-index
//!   cursor with apply/snapshot/install helpers
//! - `fsm::query` — read helpers over the state
//!
//! The openraft integration (log store, network, runtime) lands in
//! subsequent commits. Until those arrive, nothing in this module
//! is wired into the hot path; see `src/cluster` for the
//! probe-based clustering that is in use today.

pub mod adapter;
pub mod fsm;
pub mod storage;
pub mod types;

pub use adapter::{AbixioFsmAdapter, AbixioLogAdapter};
pub use fsm::{AbixioStateMachine, Op, State, VoterKind};
pub use storage::{LogEntry, LogStore, SnapshotMeta, SnapshotStore, Vote, VoteStore};
pub use types::{AbixioNode, AppResponse, NodeId, TypeConfig};
