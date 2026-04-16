//! Rule filter matching. MVP supports only prefix. Tag and And
//! filters are treated as non-matching (documented 0/10).

use s3s::dto::{LifecycleRule, LifecycleRuleFilter};

use super::types::ObjectOpts;

/// Return true if the rule's filter accepts the given object.
/// - `Filter.prefix` present: string match on key prefix.
/// - `Rule.prefix` (deprecated top-level): same.
/// - `Filter.and`, `Filter.tag`, object-size filters: treated as
///   non-matching. A rule that relies on those conditions is a no-op
///   for us, which is strictly safer than silently ignoring the
///   condition and applying the action.
pub fn matches(rule: &LifecycleRule, obj: &ObjectOpts) -> bool {
    if let Some(ref filter) = rule.filter {
        return filter_matches(filter, obj);
    }
    match rule.prefix.as_deref() {
        Some(p) if !p.is_empty() => obj.name.starts_with(p),
        _ => true, // no filter, no top-level prefix -> applies to all
    }
}

fn filter_matches(filter: &LifecycleRuleFilter, obj: &ObjectOpts) -> bool {
    // unsupported shapes: treat as non-matching
    if filter.and.is_some() || filter.tag.is_some()
        || filter.object_size_greater_than.is_some()
        || filter.object_size_less_than.is_some()
    {
        return false;
    }
    match filter.prefix.as_deref() {
        Some(p) if !p.is_empty() => obj.name.starts_with(p),
        Some(_) | None => true,
    }
}
