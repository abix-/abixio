//! Lifecycle evaluator data model. One `Event` (Action + RuleID +
//! Due) per object version, decided by walking the rule list against
//! an `ObjectOpts` snapshot.

use std::time::SystemTime;

/// A single lifecycle decision for one object version.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Event {
    pub action: Action,
    pub rule_id: String,
    /// Unix seconds at which the action came due. Zero means "not
    /// applicable" (e.g. for Action::None) and is distinguishable from
    /// "due now" which is set to the current time.
    pub due_unix_secs: u64,
}

/// Actions the enforcement engine knows how to perform. Transition
/// and restore variants are deliberately out of scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Action {
    /// No rule matched; leave the object alone.
    #[default]
    None,
    /// Delete the current version. On a versioned bucket this produces
    /// a delete marker. On an unversioned bucket it removes the object.
    Delete,
    /// Delete one specific noncurrent version.
    DeleteVersion,
    /// Delete the current version plus every noncurrent version.
    DeleteAllVersions,
    /// An orphan delete marker (no surviving non-delete versions) has
    /// expired; remove it.
    DeleteExpiredMarker,
}

impl Action {
    pub fn is_noop(self) -> bool {
        matches!(self, Action::None)
    }
}

/// Per-version snapshot passed to the evaluator. Built from
/// `storage::metadata::ObjectMeta` + position in the version list.
#[derive(Clone, Debug)]
pub struct ObjectOpts {
    pub name: String,
    /// Unix seconds. Comes from `ObjectMeta.created_at`.
    pub mod_time_unix_secs: u64,
    pub version_id: String,
    pub is_latest: bool,
    pub is_delete_marker: bool,
    pub num_versions: usize,
    /// Mod time of the version that replaced this one (i.e. the next-
    /// newer version). Zero for the latest or when unknown. Used for
    /// noncurrent-days math.
    pub successor_mod_time_unix_secs: u64,
    /// Byte size (post-erasure original size). Only used if size
    /// filters are ever enabled; MVP ignores it.
    pub size: u64,
}

impl ObjectOpts {
    /// True when this version is the only one left and it is a delete
    /// marker -- safe to reap under lifecycle expiration rules.
    pub fn expired_object_delete_marker(&self) -> bool {
        self.is_delete_marker && self.num_versions == 1
    }
}

/// Current unix seconds helper. Wrapping `SystemTime` keeps the
/// callers free of time-crate dependencies and makes tests able to
/// inject a fixed `now`.
pub fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compute the unix-seconds instant at which an object becomes
/// eligible for action given its mod time and a `days` threshold.
/// Uses the simple "mod_time + days * 86400" rule -- tests set `now`
/// accordingly.
pub fn expected_expiry(mod_time_unix_secs: u64, days: i32) -> u64 {
    if days <= 0 {
        return mod_time_unix_secs;
    }
    mod_time_unix_secs.saturating_add((days as u64).saturating_mul(86_400))
}
