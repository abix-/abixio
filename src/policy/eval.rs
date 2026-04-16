//! Policy evaluator. Iterates statements against an `Args` describing
//! the incoming request; explicit Deny wins, matched Allow allows,
//! otherwise defers to the caller's is_owner flag (matching S3
//! semantics that the authenticated bucket owner retains access
//! outside of explicit Deny).

use super::glob::glob_match;
use super::types::{Effect, Policy, Principal, Statement};

/// Per-request view passed into the evaluator.
#[derive(Clone, Debug)]
pub struct Args<'a> {
    pub bucket: &'a str,
    pub key: Option<&'a str>,
    /// S3 action string (e.g. `s3:GetObject`).
    pub action: &'a str,
    /// Authenticated access key; empty string for anonymous.
    pub access_key: &'a str,
    /// Caller holds the cluster's root credentials.
    pub is_owner: bool,
}

impl<'a> Args<'a> {
    pub fn resource(&self) -> String {
        match self.key {
            Some(k) if !k.is_empty() => format!("arn:aws:s3:::{}/{}", self.bucket, k),
            _ => format!("arn:aws:s3:::{}", self.bucket),
        }
    }
}

impl Policy {
    /// Evaluate the policy. Returns true to allow, false to deny.
    pub fn is_allowed(&self, args: &Args) -> bool {
        let mut any_allow = false;
        let resource = args.resource();
        for stmt in &self.statements {
            if !stmt_matches(stmt, args, &resource) {
                continue;
            }
            match stmt.effect {
                Effect::Deny => return false,
                Effect::Allow => any_allow = true,
            }
        }
        any_allow || args.is_owner
    }
}

fn stmt_matches(stmt: &Statement, args: &Args, resource: &str) -> bool {
    // Conditions are unsupported -- if present, treat as non-matching.
    if stmt.condition.is_some() {
        return false;
    }
    if !principal_matches(&stmt.principal, args.access_key) {
        return false;
    }
    if !stmt.action.iter().any(|pat| glob_match(pat, args.action)) {
        return false;
    }
    if !stmt.resource.iter().any(|pat| glob_match(pat, resource)) {
        return false;
    }
    true
}

fn principal_matches(principal: &Principal, access_key: &str) -> bool {
    match principal {
        Principal::Anyone => true,
        Principal::Aws(list) => {
            if list.is_empty() {
                return false;
            }
            list.iter().any(|p| principal_entry_matches(p, access_key))
        }
    }
}

fn principal_entry_matches(entry: &str, access_key: &str) -> bool {
    if entry == "*" {
        return true;
    }
    if entry == access_key {
        return true;
    }
    // common IAM-arn shape: allow matching the tail after "user/" or
    // "root/"; saves callers from writing full ARNs against a simple
    // access-key-based identity system.
    if let Some(tail) = entry.rsplit('/').next() {
        return tail == access_key;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::types::Policy;
    use serde_json::json;

    fn policy(v: serde_json::Value) -> Policy {
        Policy::from_json(&v).unwrap()
    }

    #[test]
    fn no_policy_defers_to_owner() {
        let p = policy(json!({
            "Version": "2012-10-17",
            "Statement": [{"Effect": "Allow", "Principal": {"AWS": ["someone"]}, "Action": "s3:GetObject", "Resource": "arn:aws:s3:::b/*"}]
        }));
        // unrelated caller, no matching statement -> default to is_owner
        let allowed = p.is_allowed(&Args {
            bucket: "b",
            key: Some("k"),
            action: "s3:GetObject",
            access_key: "nobody",
            is_owner: true,
        });
        assert!(allowed);
    }

    #[test]
    fn anonymous_get_on_public_bucket() {
        let p = policy(json!({
            "Version": "2012-10-17",
            "Statement": [{"Effect": "Allow", "Principal": "*", "Action": "s3:GetObject", "Resource": "arn:aws:s3:::pub/*"}]
        }));
        assert!(p.is_allowed(&Args {
            bucket: "pub",
            key: Some("k"),
            action: "s3:GetObject",
            access_key: "",
            is_owner: false,
        }));
        // anonymous PUT on same bucket is denied
        assert!(!p.is_allowed(&Args {
            bucket: "pub",
            key: Some("k"),
            action: "s3:PutObject",
            access_key: "",
            is_owner: false,
        }));
    }

    #[test]
    fn explicit_deny_overrides_allow() {
        let p = policy(json!({
            "Version": "2012-10-17",
            "Statement": [
                {"Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": "arn:aws:s3:::b/*"},
                {"Effect": "Deny", "Principal": "*", "Action": "s3:DeleteObject", "Resource": "arn:aws:s3:::b/locked/*"}
            ]
        }));
        assert!(p.is_allowed(&Args {
            bucket: "b",
            key: Some("free/k"),
            action: "s3:DeleteObject",
            access_key: "",
            is_owner: false,
        }));
        assert!(!p.is_allowed(&Args {
            bucket: "b",
            key: Some("locked/k"),
            action: "s3:DeleteObject",
            access_key: "",
            is_owner: false,
        }));
    }

    #[test]
    fn condition_present_makes_statement_nonmatching() {
        let p = policy(json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow", "Principal": "*",
                "Action": "s3:GetObject", "Resource": "arn:aws:s3:::b/*",
                "Condition": {"IpAddress": {"aws:SourceIp": "10.0.0.0/8"}}
            }]
        }));
        assert!(!p.is_allowed(&Args {
            bucket: "b",
            key: Some("k"),
            action: "s3:GetObject",
            access_key: "",
            is_owner: false,
        }));
    }

    #[test]
    fn action_wildcard_allows_any_op() {
        let p = policy(json!({
            "Version": "2012-10-17",
            "Statement": [{"Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": "arn:aws:s3:::b/*"}]
        }));
        for op in ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"] {
            assert!(
                p.is_allowed(&Args { bucket: "b", key: Some("k"), action: op, access_key: "", is_owner: false }),
                "op {} should be allowed under s3:*",
                op
            );
        }
    }

    #[test]
    fn aws_principal_matches_access_key() {
        let p = policy(json!({
            "Version": "2012-10-17",
            "Statement": [{"Effect": "Allow", "Principal": {"AWS": ["alice"]}, "Action": "s3:*", "Resource": "arn:aws:s3:::b/*"}]
        }));
        assert!(p.is_allowed(&Args { bucket: "b", key: Some("k"), action: "s3:GetObject", access_key: "alice", is_owner: false }));
        assert!(!p.is_allowed(&Args { bucket: "b", key: Some("k"), action: "s3:GetObject", access_key: "bob", is_owner: false }));
    }
}
