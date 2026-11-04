//! Named snapshots of managed agent settings (feature parity row 5.6).

use std::collections::BTreeMap;

use hofund_plugin_api::Slot as ApiSlot;

use crate::catalog::Catalog;
use crate::{AgentError, Session};

/// Maps `agent_id.field_key` to a string value. An empty string is the field
/// default or absent.
pub type ProfileFields = BTreeMap<String, String>;

/// Result of applying profile fields to detected agents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// How many field writes changed on-disk values.
    pub changed: usize,
}

/// Captures every detected agent's managed, non-secret fields.
///
/// # Errors
///
/// Returns [`AgentError`] when a managed file cannot be read or parsed.
pub fn snapshot_fields(catalog: &Catalog, session: &Session) -> Result<ProfileFields, AgentError> {
    catalog.profile_snapshot(session)
}

/// Writes profile values for detected agents only.
///
/// Keys are sorted with provider-like names first, then `.model`, then
/// alphabetically. Unknown agents and fields are skipped with a note. On the
/// first error, every path touched in this call is reverted in reverse order.
///
/// # Errors
///
/// Returns [`AgentError`] when a managed file cannot be read, parsed, or written.
pub fn apply_fields(
    catalog: &Catalog,
    session: &Session,
    fields: &ProfileFields,
    notes: &mut Vec<String>,
) -> Result<ApplyOutcome, AgentError> {
    catalog.profile_apply(session, fields, notes)
}

/// Reports whether applying `fields` would change nothing on this machine.
///
/// # Errors
///
/// Returns [`AgentError`] when a managed file cannot be read or parsed.
pub fn in_use(
    fields: &ProfileFields,
    catalog: &Catalog,
    session: &Session,
) -> Result<bool, AgentError> {
    catalog.profile_in_use(session, fields)
}

/// Whether a profile must not store this field name and value.
///
/// Matches the reference profile details rules: credential-like names and
/// values that look like API keys or long opaque tokens.
#[must_use]
pub fn is_profile_secret(field_key: &str, value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    secret_name(field_key) || secret_value(value)
}

pub(crate) fn profile_field_included(slot: &ApiSlot, field_key: &str, value: &str) -> bool {
    !matches!(slot, ApiSlot::Token) && !is_profile_secret(field_key, value)
}

pub(crate) fn profile_field_rank(full_key: &str) -> u8 {
    if full_key.ends_with(".provider") {
        0
    } else if full_key.ends_with(".model") {
        1
    } else {
        2
    }
}

fn secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["key", "token", "secret", "passw", "credential", "cookie"]
        .iter()
        .any(|part| lower.contains(part))
}

fn secret_value(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("sk-")
        || trimmed.starts_with("sk_")
        || trimmed.starts_with("ghp_")
        || trimmed.starts_with("gho_")
        || trimmed.starts_with("ghu_")
        || trimmed.starts_with("github_pat_")
        || trimmed.starts_with("xox")
        || trimmed.starts_with("AIza")
        || trimmed.starts_with("eyJ")
        || (trimmed.len() >= 32 && trimmed.chars().all(|ch| ch.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::{ApiSlot, profile_field_included, profile_field_rank};

    #[test]
    fn g1_17_token_slot_excluded_even_with_plain_name_and_value() {
        assert!(!profile_field_included(
            &ApiSlot::Token,
            "agent.label",
            "plain"
        ));
        assert!(profile_field_included(
            &ApiSlot::Origin,
            "agent.label",
            "plain"
        ));
    }

    #[test]
    fn g1_11_profile_field_rank_orders_provider_before_model() {
        assert!(profile_field_rank("agent.provider") < profile_field_rank("agent.model"));
        assert!(profile_field_rank("agent.model") < profile_field_rank("agent.other"));
    }
}
