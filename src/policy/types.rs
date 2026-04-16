//! Typed view of a bucket policy parsed out of the JSON blob stored
//! on `BucketSettings.policy`. The wire format is AWS's standard
//! policy document; the typed view is intentionally minimal and drops
//! fields abixio does not enforce (NotAction/NotResource/
//! NotPrincipal, Condition values, policy variables).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct Policy {
    pub version: String,
    pub statements: Vec<Statement>,
}

#[derive(Clone, Debug)]
pub struct Statement {
    pub sid: Option<String>,
    pub effect: Effect,
    pub principal: Principal,
    pub action: Vec<String>,
    pub resource: Vec<String>,
    /// Raw condition block, unparsed. Presence means "we cannot
    /// evaluate safely" and the statement is treated as non-matching.
    pub condition: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Clone, Debug)]
pub enum Principal {
    /// `"Principal": "*"` -- everyone, including anonymous callers.
    Anyone,
    /// `{"AWS": ["...", "..."]}` -- explicit list. Strings are matched
    /// literally or by the tail segment after `user/` for ARN shapes.
    Aws(Vec<String>),
}

impl Policy {
    /// Parse a stored policy JSON `Value` into the typed shape.
    /// Returns None for null / empty / unrecognized policies so
    /// callers can fall through to the existing owner-only behavior.
    pub fn from_json(value: &Value) -> Option<Policy> {
        let obj = value.as_object()?;
        let version = obj.get("Version").and_then(Value::as_str).unwrap_or("").to_string();
        if version.is_empty() {
            return None;
        }
        let statements_raw = obj.get("Statement")?;
        let stmts = match statements_raw {
            Value::Array(arr) => arr.iter().map(Self::parse_statement).collect(),
            single @ Value::Object(_) => vec![Self::parse_statement(single)],
            _ => return None,
        };
        let statements: Vec<Statement> = stmts.into_iter().flatten().collect();
        if statements.is_empty() {
            return None;
        }
        Some(Policy { version, statements })
    }

    fn parse_statement(v: &Value) -> Option<Statement> {
        let obj = v.as_object()?;
        let sid = obj.get("Sid").and_then(Value::as_str).map(str::to_string);
        let effect = match obj.get("Effect").and_then(Value::as_str)? {
            "Allow" => Effect::Allow,
            "Deny" => Effect::Deny,
            _ => return None,
        };
        let principal = parse_principal(obj.get("Principal"))?;
        let action = string_or_array(obj.get("Action"));
        let resource = string_or_array(obj.get("Resource"));
        if action.is_empty() || resource.is_empty() {
            return None;
        }
        let condition = obj.get("Condition").cloned().filter(|c| !c.is_null());
        Some(Statement {
            sid,
            effect,
            principal,
            action,
            resource,
            condition,
        })
    }
}

fn parse_principal(v: Option<&Value>) -> Option<Principal> {
    match v {
        None => Some(Principal::Anyone), // absent = any
        Some(Value::String(s)) if s == "*" => Some(Principal::Anyone),
        Some(Value::Object(m)) => {
            if let Some(aws) = m.get("AWS") {
                Some(Principal::Aws(string_or_array(Some(aws))))
            } else {
                // unsupported principal kinds (Federated, Service) -> empty list
                // which will never match anything.
                Some(Principal::Aws(Vec::new()))
            }
        }
        Some(_) => None,
    }
}

fn string_or_array(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_public_read() {
        let j = json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Sid": "PublicRead",
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:GetObject",
                "Resource": "arn:aws:s3:::pub/*"
            }]
        });
        let p = Policy::from_json(&j).unwrap();
        assert_eq!(p.version, "2012-10-17");
        assert_eq!(p.statements.len(), 1);
        assert_eq!(p.statements[0].effect, Effect::Allow);
        assert!(matches!(p.statements[0].principal, Principal::Anyone));
    }

    #[test]
    fn parse_deny_and_allow_combo() {
        let j = json!({
            "Version": "2012-10-17",
            "Statement": [
                {"Effect": "Allow", "Principal": "*", "Action": "s3:*", "Resource": "arn:aws:s3:::b/*"},
                {"Effect": "Deny", "Principal": "*", "Action": "s3:DeleteObject", "Resource": "arn:aws:s3:::b/locked/*"}
            ]
        });
        let p = Policy::from_json(&j).unwrap();
        assert_eq!(p.statements.len(), 2);
        assert_eq!(p.statements[1].effect, Effect::Deny);
    }

    #[test]
    fn parse_rejects_missing_version() {
        let j = json!({"Statement": []});
        assert!(Policy::from_json(&j).is_none());
    }

    #[test]
    fn parse_keeps_condition_as_opaque() {
        let j = json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": "*",
                "Action": "s3:GetObject",
                "Resource": "arn:aws:s3:::b/*",
                "Condition": {"IpAddress": {"aws:SourceIp": "10.0.0.0/24"}}
            }]
        });
        let p = Policy::from_json(&j).unwrap();
        assert!(p.statements[0].condition.is_some());
    }

    #[test]
    fn parse_aws_principal_list() {
        let j = json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Principal": {"AWS": ["alice", "bob"]},
                "Action": "s3:GetObject",
                "Resource": "arn:aws:s3:::b/*"
            }]
        });
        let p = Policy::from_json(&j).unwrap();
        match &p.statements[0].principal {
            Principal::Aws(keys) => assert_eq!(keys, &vec!["alice".to_string(), "bob".to_string()]),
            _ => panic!("expected Aws principal"),
        }
    }
}
