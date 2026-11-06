//! In-blob refresh markers, fingerprints, and Codex home ownership (R-47b reconcile).

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::store::{StoreMode, canonical_codex_home};

pub(crate) const BASELINE_ABSENT: &str = "ABSENT";

pub(crate) const FIELD_ATTEMPT_ID: &str = "refresh_attempt_id";
pub(crate) const FIELD_CANONICAL_HOME: &str = "refresh_canonical_home";
pub(crate) const FIELD_STORE_LOCATION_FP: &str = "refresh_store_location_fp";
pub(crate) const FIELD_BASELINE_TUPLE_FP: &str = "refresh_baseline_tuple_fp";
pub(crate) const FIELD_CANDIDATE_TUPLE_FP: &str = "refresh_candidate_tuple_fp";
pub(crate) const FIELD_INVALID: &str = "refresh_invalid";

/// Active refresh marker state parsed from the protected account secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshMarkers {
    Invalid,
    CandidatePending {
        attempt_id: String,
        canonical_home: String,
        store_location_fp: String,
        baseline_tuple_fp: String,
        candidate_tuple_fp: String,
    },
    InFlight {
        attempt_id: String,
        canonical_home: String,
        store_location_fp: String,
        baseline_tuple_fp: String,
    },
}

pub(crate) fn fingerprint_tuple_absent() -> &'static str {
    BASELINE_ABSENT
}

pub(crate) fn fingerprint_tuple(
    access: Option<&SecretString>,
    refresh: Option<&SecretString>,
    id_token: Option<&SecretString>,
    store_account_id: Option<&str>,
) -> String {
    let mut material = String::new();
    material.push_str(access.map(secret_text).unwrap_or(""));
    material.push('\0');
    material.push_str(refresh.map(secret_text).unwrap_or(""));
    material.push('\0');
    material.push_str(id_token.map(secret_text).unwrap_or(""));
    material.push('\0');
    material.push_str(store_account_id.unwrap_or(""));
    hex16(&sha256(material.as_bytes())[..8])
}

pub(crate) fn fingerprint_location(keychain: bool, mode: StoreMode) -> String {
    let surface = if keychain { "keychain" } else { "file" };
    format!("{surface}:{mode:?}")
}

pub(crate) fn parse_markers(blob: &SecretObject) -> Option<RefreshMarkers> {
    if blob.flag(FIELD_INVALID) {
        return Some(RefreshMarkers::Invalid);
    }
    let attempt_id = blob.text(FIELD_ATTEMPT_ID)?.to_owned();
    let canonical_home = blob.text(FIELD_CANONICAL_HOME)?.to_owned();
    let store_location_fp = blob.text(FIELD_STORE_LOCATION_FP)?.to_owned();
    let baseline_tuple_fp = blob.text(FIELD_BASELINE_TUPLE_FP)?.to_owned();
    if let Some(candidate_tuple_fp) = blob.text(FIELD_CANDIDATE_TUPLE_FP) {
        return Some(RefreshMarkers::CandidatePending {
            attempt_id,
            canonical_home,
            store_location_fp,
            baseline_tuple_fp,
            candidate_tuple_fp: candidate_tuple_fp.to_owned(),
        });
    }
    Some(RefreshMarkers::InFlight {
        attempt_id,
        canonical_home,
        store_location_fp,
        baseline_tuple_fp,
    })
}

pub(crate) fn with_inflight(
    blob: SecretObject,
    attempt_id: &str,
    canonical_home: &str,
    store_location_fp: &str,
    baseline_tuple_fp: &str,
) -> SecretObject {
    blob.with_text(FIELD_ATTEMPT_ID, attempt_id)
        .with_text(FIELD_CANONICAL_HOME, canonical_home)
        .with_text(FIELD_STORE_LOCATION_FP, store_location_fp)
        .with_text(FIELD_BASELINE_TUPLE_FP, baseline_tuple_fp)
        .without(FIELD_CANDIDATE_TUPLE_FP)
        .without(FIELD_INVALID)
}

pub(crate) fn with_pending(
    blob: SecretObject,
    candidate_tuple_fp: &str,
    access: &SecretString,
    refresh: Option<&SecretString>,
    id_token: Option<&SecretString>,
    store_account_id: Option<&str>,
) -> SecretObject {
    let mut next = blob
        .with_text(FIELD_CANDIDATE_TUPLE_FP, candidate_tuple_fp)
        .with_secret("access_token", access);
    if let Some(refresh) = refresh {
        next = next.with_secret("refresh_token", refresh);
    }
    if let Some(id_token) = id_token {
        next = next.with_secret("id_token", id_token);
    }
    if let Some(account_id) = store_account_id {
        next = next.with_text("account_id", account_id);
    }
    next
}

pub(crate) fn clear_markers(blob: SecretObject) -> SecretObject {
    blob.without(FIELD_ATTEMPT_ID)
        .without(FIELD_CANONICAL_HOME)
        .without(FIELD_STORE_LOCATION_FP)
        .without(FIELD_BASELINE_TUPLE_FP)
        .without(FIELD_CANDIDATE_TUPLE_FP)
        .without(FIELD_INVALID)
}

#[expect(dead_code, reason = "Task 4 conflict paths set INVALID in follow-up")]
pub(crate) fn set_invalid(blob: SecretObject) -> SecretObject {
    clear_markers(blob).with_flag(FIELD_INVALID, true)
}

/// Lists Codex accounts whose stored `codex_home` canonicalizes to `canonical_home`.
pub(crate) async fn home_owners(
    host: &HostContext,
    canonical_home: &str,
) -> Result<Vec<AccountId>, PluginError> {
    let rows = host.accounts().list().await?;
    let mut owners = Vec::new();
    for row in rows {
        let id = row.id().clone();
        let Some(secret) = host.accounts().secret(&id).await? else {
            continue;
        };
        let blob = SecretObject::parse(&secret)?;
        if blob.flag(crate::tokens::LAPSED) {
            continue;
        }
        let Some(home) = blob.text("codex_home") else {
            continue;
        };
        if canonical_codex_home(Path::new(home)) == canonical_home {
            owners.push(id);
        }
    }
    owners.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    owners.dedup();
    Ok(owners)
}

/// Enforces the unique-home rule for the lease holder (0 or 1 matching owner).
pub(crate) fn assert_unique_owner(
    owners: &[AccountId],
    lease_id: &AccountId,
) -> Result<(), PluginError> {
    match owners.len() {
        0 => Ok(()),
        1 if owners[0] == *lease_id => Ok(()),
        1 => Err(PluginError::auth(
            "another Codex account owns this home directory",
        )),
        _ => Err(PluginError::auth(
            "multiple Codex accounts share this home directory",
        )),
    }
}

fn hex16(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(16), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_wins_over_legacy_inflight() {
        let blob = SecretObject::new()
            .with_text(FIELD_ATTEMPT_ID, "a1")
            .with_text(FIELD_CANONICAL_HOME, "/tmp/codex")
            .with_text(FIELD_STORE_LOCATION_FP, "file:File")
            .with_text(FIELD_BASELINE_TUPLE_FP, BASELINE_ABSENT)
            .with_text(FIELD_CANDIDATE_TUPLE_FP, "cand");
        assert!(matches!(
            parse_markers(&blob),
            Some(RefreshMarkers::CandidatePending { .. })
        ));
    }

    #[test]
    fn baseline_absent_when_store_signed_out() {
        assert_eq!(fingerprint_tuple_absent(), "ABSENT");
    }

    #[test]
    fn inflight_marker_without_candidate() {
        let blob = with_inflight(
            SecretObject::new(),
            "attempt-1",
            "/tmp/home",
            "file:File",
            BASELINE_ABSENT,
        );
        assert!(matches!(
            parse_markers(&blob),
            Some(RefreshMarkers::InFlight { .. })
        ));
    }

    #[test]
    fn clear_markers_removes_refresh_fields() {
        let blob = with_inflight(SecretObject::new(), "a", "/h", "loc", BASELINE_ABSENT);
        let cleared = clear_markers(blob);
        assert!(parse_markers(&cleared).is_none());
    }
}
