//! On-disk persistence for Raft state. Three pieces, each in its
//! own file under `<first local disk>/.abixio.sys/raft/`:
//!
//! - `log.dat` — append-only log of entries (index + term + bincode
//!   `Op` bytes)
//! - `snap-<index>.msgpack` — full FSM state snapshots; the last two
//!   are kept on disk
//! - `vote.json` — current term + voted-for node id, fsynced on
//!   every update
//!
//! The modules are unit-testable in isolation so we can ship the
//! persistence layer before the openraft integration arrives.

pub mod log;
pub mod meta;
pub mod snapshot;

pub use log::{LogEntry, LogStore};
pub use meta::{Vote, VoteStore};
pub use snapshot::{SnapshotMeta, SnapshotStore};

/// Standard subdirectory under a disk root that holds all Raft
/// persistence files.
pub const RAFT_DIR: &str = ".abixio.sys/raft";
