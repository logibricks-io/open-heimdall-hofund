//! Merge native session rows with read-only gateway projections.

use std::collections::BTreeSet;
use std::path::Path;

use hofund_plugin_api::SessionRecord;
use hofund_usage::{GatewaySession, Ledger, UsageError, gateway_sessions};

use crate::gateway_hidden::{gateway_hidden, load_gateway_hidden};

/// Merges gateway projections after native rows, dropping duplicates and hidden sessions.
///
/// # Errors
///
/// Returns [`UsageError`] when the ledger cannot be read.
pub fn merge_gateway_sessions(
    config_dir: &Path,
    native: Vec<SessionRecord>,
    ledger: &Ledger,
    extra_keys: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<SessionRecord>, UsageError> {
    let hidden = load_gateway_hidden(config_dir);
    let native_keys = native_gateway_keys(&native, extra_keys);
    let projected = gateway_sessions(ledger, None)?;
    let mut added = Vec::new();
    for session in projected {
        if native_keys.contains(&(session.agent.clone(), session.id.clone())) {
            continue;
        }
        if gateway_hidden(&hidden, &session.agent, &session.id, session.last_unix) {
            continue;
        }
        added.push(gateway_to_record(session));
    }
    let mut out = native;
    out.extend(added);
    out.sort_by(|left, right| {
        right
            .last_ms
            .cmp(&left.last_ms)
            .then_with(|| left.agent.cmp(&right.agent))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(out)
}

fn native_gateway_keys(
    native: &[SessionRecord],
    extra: impl IntoIterator<Item = (String, String)>,
) -> BTreeSet<(String, String)> {
    let mut keys = BTreeSet::new();
    for record in native {
        keys.insert((record.agent.clone(), record.id.clone()));
        if !record.app.is_empty() {
            keys.insert((record.app.clone(), record.id.clone()));
        }
        for id in &record.copies {
            keys.insert((record.agent.clone(), id.clone()));
        }
        for id in &record.subagents {
            keys.insert((record.agent.clone(), id.clone()));
        }
    }
    for pair in extra {
        keys.insert(pair);
    }
    keys
}

fn gateway_to_record(session: GatewaySession) -> SessionRecord {
    let mut record = SessionRecord::new(session.agent, session.id, std::path::PathBuf::new());
    record.title = session.title;
    record.start_ms = session.start_unix.saturating_mul(1000);
    record.last_ms = session.last_unix.saturating_mul(1000);
    record.cost = session.cost_usd;
    record.unpriced = session.unpriced;
    record.read_only = true;
    record.gateway = true;
    for model in session.models {
        record.add_usage(
            model.model,
            hofund_plugin_api::SessionTokens::new(
                model.input_tokens,
                model.output_tokens,
                model.cache_read_tokens,
                model
                    .cache_write_5m_tokens
                    .saturating_add(model.cache_write_1h_tokens),
            )
            .with_cache_write_1h(model.cache_write_1h_tokens),
        );
    }
    record
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::SessionRecord;
    use hofund_usage::{Ledger, Row};

    use super::*;

    #[test]
    fn r33_external_added_drops_native_duplicates() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        ledger
            .record(&Row {
                created_unix: 10,
                model: "m".to_owned(),
                upstream: "up".to_owned(),
                input_tokens: Some(1),
                output_tokens: Some(1),
                latency_ms: 1,
                status: "200".to_owned(),
                client_agent: "fixture".to_owned(),
                session_id: "s1".to_owned(),
                ..Row::default()
            })
            .expect("row");
        let native = vec![SessionRecord::new("fixture", "s1", "path.jsonl")];
        let merged = merge_gateway_sessions(dir.path(), native, &ledger, []).expect("merge");
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].gateway);
        let added = merge_gateway_sessions(dir.path(), Vec::new(), &ledger, []).expect("merge");
        assert_eq!(added.len(), 1);
        assert!(added[0].gateway);
        assert!(added[0].read_only);
    }

    #[test]
    fn r33_external_added_honors_copies_subagents_app_and_extras() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        for (agent, session) in [
            ("fixture", "main"),
            ("fixture", "copy-1"),
            ("fixture", "sub-1"),
            ("desktop", "main"),
            ("other", "pi-dup"),
        ] {
            ledger
                .record(&Row {
                    created_unix: 10,
                    model: "m".to_owned(),
                    upstream: "up".to_owned(),
                    latency_ms: 1,
                    status: "200".to_owned(),
                    client_agent: agent.to_owned(),
                    session_id: session.to_owned(),
                    ..Row::default()
                })
                .expect("row");
        }
        let mut native = SessionRecord::new("fixture", "main".to_owned(), "main.jsonl");
        native.copies.push("copy-1".into());
        native.subagents.push("sub-1".into());
        native.app = "desktop".into();
        let merged = merge_gateway_sessions(
            dir.path(),
            vec![native],
            &ledger,
            [("pi".into(), "pi-dup".into())],
        )
        .expect("merge");
        assert_eq!(merged.len(), 2);
        assert!(
            merged
                .iter()
                .any(|record| record.id == "main" && !record.gateway)
        );
        assert!(
            merged
                .iter()
                .any(|record| record.agent == "other" && record.id == "pi-dup" && record.gateway)
        );
        let gateway_only =
            merge_gateway_sessions(dir.path(), Vec::new(), &ledger, []).expect("merge");
        assert_eq!(gateway_only.len(), 5);
        assert!(gateway_only.iter().all(|record| record.gateway));
    }
}
