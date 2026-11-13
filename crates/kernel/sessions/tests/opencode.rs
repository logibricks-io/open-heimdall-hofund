//! End-to-end OpenCode reader tests against plugin-owned golden fixtures.

use std::path::{Path, PathBuf};

use hofund_plugin_agent_opencode::OpenCodeStore;
use hofund_plugin_api::{SessionRecord, SessionStore};
use hofund_sessions::consent::Consent;
use hofund_sessions::index::SessionIndex;
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plugins/agents/opencode/tests/fixtures")
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

fn records(root: &Path, config: &Path) -> Vec<SessionRecord> {
    let store = OpenCodeStore::with_root(root);
    let mut consent = Consent::new(config);
    consent.settle();
    consent.set_reads(Some(true), []).expect("consent");
    let stores: [&dyn SessionStore; 1] = [&store];
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

fn assert_golden(name: &str, records: &[SessionRecord]) {
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture(&format!("{name}.json"))).expect("golden"),
    )
    .expect("golden JSON");
    assert_eq!(summaries(records), expected);
}

fn database_records(name: &str) -> Vec<SessionRecord> {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("data");
    std::fs::create_dir(&root).expect("data root");
    let connection = rusqlite::Connection::open(root.join("opencode.db")).expect("database");
    connection
        .execute_batch(&std::fs::read_to_string(fixture(&format!("{name}.sql"))).expect("SQL"))
        .expect("schema fixture");
    drop(connection);
    records(&root, &temp.path().join("config"))
}

#[test]
fn opencode_v1_golden() {
    assert_golden("v1", &database_records("v1"));
}

#[test]
fn opencode_v2_groups_child_and_matches_golden() {
    assert_golden("v2", &database_records("v2"));
}

#[test]
fn opencode_mixed_migration_reads_only_uncopied_legacy_rows() {
    assert_golden("mixed", &database_records("mixed"));
}

#[test]
fn opencode_completed_migration_skips_legacy() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("data");
    std::fs::create_dir(&root).expect("data root");
    let database = root.join("opencode.db");
    let connection = rusqlite::Connection::open(&database).expect("database");
    connection
        .execute_batch(&std::fs::read_to_string(fixture("mixed.sql")).expect("SQL"))
        .expect("schema fixture");
    connection
        .execute(
            "UPDATE kv SET value = '{\"phase\":\"completed\"}' WHERE key = 'migration.v1-v2'",
            [],
        )
        .expect("complete migration");
    drop(connection);
    let records = records(&root, &temp.path().join("config"));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "opencode:ses_shared");
    assert_eq!(records[0].tokens().input, 5);
}

#[test]
fn opencode_json_fallback_golden() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("data");
    copy_tree(&fixture("json"), &root);
    assert_golden("json", &records(&root, &temp.path().join("config")));
}

#[test]
fn opencode_consent_off_lists_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("data");
    copy_tree(&fixture("json"), &root);
    let config = temp.path().join("config");
    let store = OpenCodeStore::with_root(&root);
    let mut consent = Consent::new(&config);
    consent.settle();
    let stores: [&dyn SessionStore; 1] = [&store];
    let mut index = SessionIndex::load(&config, &consent);
    index.refresh(&consent, &stores).expect("refresh");
    assert_eq!(index.list(&consent), Vec::<SessionRecord>::new());
    assert!(!config.join("state").join("sessions-index.json").exists());
}

#[cfg(unix)]
#[test]
fn opencode_related_symlink_escape_is_refused() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("data");
    copy_tree(&fixture("json"), &root);
    let outside = temp.path().join("outside.json");
    std::fs::write(&outside, "{\"role\":\"assistant\"}\n").expect("outside");
    let message = root.join("storage/message/ses_json/m_assistant.json");
    std::fs::remove_file(&message).expect("replace fixture");
    symlink(&outside, &message).expect("escape link");
    let store = OpenCodeStore::with_root(&root);
    let mut consent = Consent::new(temp.path().join("config"));
    consent.settle();
    consent.set_reads(Some(true), []).expect("consent");
    let stores: [&dyn SessionStore; 1] = [&store];
    let mut index = SessionIndex::load(&temp.path().join("config"), &consent);
    assert!(index.refresh(&consent, &stores).is_err());
}
