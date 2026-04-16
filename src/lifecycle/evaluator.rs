//! Lifecycle evaluator. Given a bucket lifecycle configuration and a
//! single object version, compute the action the enforcement engine
//! should take. Pure (no IO). Walks enabled rules that match, collects
//! candidate events, picks the one with the earliest due time.

use s3s::dto::{BucketLifecycleConfiguration, ExpirationStatus, LifecycleRule};

use super::filter;
use super::types::{expected_expiry, Action, Event, ObjectOpts};

pub struct Evaluator<'a> {
    cfg: &'a BucketLifecycleConfiguration,
}

impl<'a> Evaluator<'a> {
    pub fn new(cfg: &'a BucketLifecycleConfiguration) -> Self {
        Self { cfg }
    }

    /// Pick a single action for this object version. `newer_noncurrent_versions`
    /// is the number of noncurrent versions newer than this one that
    /// the caller has decided to keep; it feeds the
    /// NoncurrentVersionExpiration `newer_noncurrent_versions`
    /// threshold.
    pub fn eval(
        &self,
        obj: &ObjectOpts,
        now_unix_secs: u64,
        newer_noncurrent_versions: usize,
    ) -> Event {
        if obj.mod_time_unix_secs == 0 {
            return Event::default();
        }

        let mut chosen: Option<Event> = None;

        for rule in &self.cfg.rules {
            if rule.status.as_str() != ExpirationStatus::ENABLED {
                continue;
            }
            if !filter::matches(rule, obj) {
                continue;
            }

            // expired orphan delete marker: ExpiredObjectDeleteMarker
            if obj.expired_object_delete_marker() {
                if let Some(ref exp) = rule.expiration {
                    if exp.expired_object_delete_marker == Some(true) {
                        chosen = earlier(
                            chosen,
                            Event {
                                action: Action::DeleteExpiredMarker,
                                rule_id: rule_id(rule),
                                due_unix_secs: now_unix_secs,
                            },
                        );
                        continue;
                    }
                    // days-based cleanup also reaps delete markers
                    if let Some(days) = exp.days {
                        let due = expected_expiry(obj.mod_time_unix_secs, days);
                        if now_unix_secs >= due {
                            chosen = earlier(
                                chosen,
                                Event {
                                    action: Action::DeleteExpiredMarker,
                                    rule_id: rule_id(rule),
                                    due_unix_secs: due,
                                },
                            );
                            continue;
                        }
                    }
                }
            }

            // noncurrent version expiration (only for non-latest versions)
            if !obj.is_latest {
                if let Some(ref ncve) = rule.noncurrent_version_expiration {
                    let retained_enough = match ncve.newer_noncurrent_versions {
                        Some(n) if n > 0 => newer_noncurrent_versions >= n as usize,
                        _ => true,
                    };
                    let base = if obj.successor_mod_time_unix_secs > 0 {
                        obj.successor_mod_time_unix_secs
                    } else {
                        obj.mod_time_unix_secs
                    };
                    let days = ncve.noncurrent_days.unwrap_or(0);
                    let due = expected_expiry(base, days);
                    if retained_enough && now_unix_secs >= due {
                        chosen = earlier(
                            chosen,
                            Event {
                                action: Action::DeleteVersion,
                                rule_id: rule_id(rule),
                                due_unix_secs: due,
                            },
                        );
                        continue;
                    }
                }
            }

            // current-version expiration by Days (unversioned or latest in versioned bucket)
            if obj.is_latest && !obj.is_delete_marker {
                if let Some(ref exp) = rule.expiration {
                    if let Some(days) = exp.days {
                        let due = expected_expiry(obj.mod_time_unix_secs, days);
                        if now_unix_secs >= due {
                            chosen = earlier(
                                chosen,
                                Event {
                                    action: Action::Delete,
                                    rule_id: rule_id(rule),
                                    due_unix_secs: due,
                                },
                            );
                        }
                    }
                }
            }
        }

        chosen.unwrap_or_default()
    }
}

fn rule_id(rule: &LifecycleRule) -> String {
    rule.id.clone().unwrap_or_default()
}

/// Keep the event whose due time is earliest (first rule that
/// matures wins).
fn earlier(a: Option<Event>, b: Event) -> Option<Event> {
    match a {
        None => Some(b),
        Some(existing) if existing.due_unix_secs <= b.due_unix_secs => Some(existing),
        Some(_) => Some(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s3s::dto::{
        AbortIncompleteMultipartUpload, BucketLifecycleConfiguration, ExpirationStatus,
        LifecycleExpiration, LifecycleRule, LifecycleRuleFilter, NoncurrentVersionExpiration,
    };

    fn enabled_rule() -> LifecycleRule {
        LifecycleRule {
            id: Some("r1".to_string()),
            status: ExpirationStatus::from_static(ExpirationStatus::ENABLED),
            abort_incomplete_multipart_upload: None,
            expiration: None,
            filter: None,
            noncurrent_version_expiration: None,
            noncurrent_version_transitions: None,
            prefix: None,
            transitions: None,
        }
    }

    fn opts(name: &str, age_days: u64, is_latest: bool) -> ObjectOpts {
        let now = 2_000_000_000u64;
        ObjectOpts {
            name: name.to_string(),
            mod_time_unix_secs: now - age_days * 86_400,
            version_id: String::new(),
            is_latest,
            is_delete_marker: false,
            num_versions: 1,
            successor_mod_time_unix_secs: 0,
            size: 0,
        }
    }

    #[test]
    fn disabled_rule_noop() {
        let mut rule = enabled_rule();
        rule.status = ExpirationStatus::from_static(ExpirationStatus::DISABLED);
        rule.expiration = Some(LifecycleExpiration {
            days: Some(1),
            ..Default::default()
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let ev = Evaluator::new(&cfg).eval(&opts("k", 10, true), 2_000_000_000, 0);
        assert_eq!(ev.action, Action::None);
    }

    #[test]
    fn expiration_days_matches() {
        let mut rule = enabled_rule();
        rule.expiration = Some(LifecycleExpiration {
            days: Some(5),
            ..Default::default()
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let ev = Evaluator::new(&cfg).eval(&opts("k", 10, true), 2_000_000_000, 0);
        assert_eq!(ev.action, Action::Delete);
        assert_eq!(ev.rule_id, "r1");
    }

    #[test]
    fn expiration_days_not_yet_matured() {
        let mut rule = enabled_rule();
        rule.expiration = Some(LifecycleExpiration {
            days: Some(30),
            ..Default::default()
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let ev = Evaluator::new(&cfg).eval(&opts("k", 5, true), 2_000_000_000, 0);
        assert_eq!(ev.action, Action::None);
    }

    #[test]
    fn prefix_filter_excludes() {
        let mut rule = enabled_rule();
        rule.filter = Some(LifecycleRuleFilter {
            prefix: Some("tmp/".to_string()),
            ..Default::default()
        });
        rule.expiration = Some(LifecycleExpiration {
            days: Some(0),
            ..Default::default()
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let ev = Evaluator::new(&cfg).eval(&opts("keep/b", 10, true), 2_000_000_000, 0);
        assert_eq!(ev.action, Action::None);
        let ev2 = Evaluator::new(&cfg).eval(&opts("tmp/a", 10, true), 2_000_000_000, 0);
        assert_eq!(ev2.action, Action::Delete);
    }

    #[test]
    fn noncurrent_version_expiration() {
        let mut rule = enabled_rule();
        rule.noncurrent_version_expiration = Some(NoncurrentVersionExpiration {
            noncurrent_days: Some(3),
            newer_noncurrent_versions: None,
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let mut obj = opts("k", 10, false);
        obj.successor_mod_time_unix_secs = 2_000_000_000u64 - 5 * 86_400;
        let ev = Evaluator::new(&cfg).eval(&obj, 2_000_000_000, 0);
        assert_eq!(ev.action, Action::DeleteVersion);
    }

    #[test]
    fn noncurrent_respects_newer_version_threshold() {
        let mut rule = enabled_rule();
        rule.noncurrent_version_expiration = Some(NoncurrentVersionExpiration {
            noncurrent_days: Some(3),
            newer_noncurrent_versions: Some(2),
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let mut obj = opts("k", 10, false);
        obj.successor_mod_time_unix_secs = 2_000_000_000u64 - 5 * 86_400;
        // only 1 newer kept: not eligible yet (need >= 2)
        let ev = Evaluator::new(&cfg).eval(&obj, 2_000_000_000, 1);
        assert_eq!(ev.action, Action::None);
        // 2 newer kept: eligible
        let ev2 = Evaluator::new(&cfg).eval(&obj, 2_000_000_000, 2);
        assert_eq!(ev2.action, Action::DeleteVersion);
    }

    #[test]
    fn expired_delete_marker_flagged() {
        let mut rule = enabled_rule();
        rule.expiration = Some(LifecycleExpiration {
            expired_object_delete_marker: Some(true),
            ..Default::default()
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let mut obj = opts("k", 0, true);
        obj.is_delete_marker = true;
        obj.num_versions = 1;
        let ev = Evaluator::new(&cfg).eval(&obj, 2_000_000_000, 0);
        assert_eq!(ev.action, Action::DeleteExpiredMarker);
    }

    #[test]
    fn tag_filter_unsupported_is_noop() {
        use s3s::dto::Tag;
        let mut rule = enabled_rule();
        rule.filter = Some(LifecycleRuleFilter {
            tag: Some(Tag {
                key: Some("k".to_string()),
                value: Some("v".to_string()),
            }),
            ..Default::default()
        });
        rule.expiration = Some(LifecycleExpiration {
            days: Some(0),
            ..Default::default()
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        let ev = Evaluator::new(&cfg).eval(&opts("k", 10, true), 2_000_000_000, 0);
        assert_eq!(ev.action, Action::None, "tag filter should be ignored as non-matching");
    }

    #[test]
    fn abort_multipart_field_parses_and_is_accessible() {
        // We don't enforce AbortIncompleteMultipartUpload in the evaluator -- that
        // runs over the multipart temp dir in the engine. Test the rule shape here.
        let mut rule = enabled_rule();
        rule.abort_incomplete_multipart_upload = Some(AbortIncompleteMultipartUpload {
            days_after_initiation: Some(7),
        });
        let cfg = BucketLifecycleConfiguration { rules: vec![rule] };
        assert_eq!(
            cfg.rules[0]
                .abort_incomplete_multipart_upload
                .as_ref()
                .and_then(|a| a.days_after_initiation),
            Some(7)
        );
    }
}
