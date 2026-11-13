//! End-to-end Claude Desktop session reader tests through the session index.

use std::path::{Path, PathBuf};

use hofund_plugin_agent_claude::{ClaudeDesktopStore, ClaudeStore};
use hofund_plugin_api::{SessionRecord, SessionStore};
use hofund_sessions::consent::Consent;
use hofund_sessions::index::SessionIndex;
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/agents/claude-code/tests/fixtures")
        .join(name)
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("destination");
    for entry in std::fs::read_dir(source).expect("fixture directory") {
        let entry = entry.expect("entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("fixture copy");
        }
    }
}

fn records(store: &dyn SessionStore, config: &Path) -> Vec<SessionRecord> {
    let mut consent = Consent::new(config);
    consent.settle();
    consent.set_reads(Some(true), []).expect("consent");
    let stores = [store];
    let mut index = SessionIndex::load(config, &consent);
    index.refresh(&consent, &stores).expect("refresh");
    let mut records = index.list(&consent);
    records.sort_by(|left, right| left.id.cmp(&right.id));
    records
}

fn summaries(records: &[SessionRecord]) -> Value {
    records
        .iter()
        .map(|record| {
            json!({
                "id": record.id,
                "agent": record.agent,
                "app": record.app,
                "title": record.title,
                "cwd": record.cwd,
                "start_ms": record.start_ms,
                "last_ms": record.last_ms,
                "tokens": record.tokens(),
                "models": record.models,
                "subagents": record.subagents,
            })
        })
        .collect()
}

#[test]
fn desktop_sessions_match_golden() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("desktop");
    copy_tree(&fixture("desktop/data"), &root);
    let store = ClaudeDesktopStore::with_roots([root.clone()]);
    let records = records(&store, &temp.path().join("config"));
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture("desktop/desktop-index.json")).expect("golden"),
    )
    .expect("golden JSON");
    assert_eq!(summaries(&records), expected);
}

#[test]
fn code_tab_entrypoint_sets_app_usage_bucket() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("agent");
    let projects = root.join("projects/app");
    std::fs::create_dir_all(&projects).expect("projects");
    std::fs::copy(
        fixture("desktop/entrypoint.jsonl"),
        projects.join("desktop-tab.jsonl"),
    )
    .expect("fixture");
    let store = ClaudeStore::with_root(&root);
    let records = records(&store, &temp.path().join("config"));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].agent, "claude");
    assert_eq!(records[0].app, "claude-desktop");
}
