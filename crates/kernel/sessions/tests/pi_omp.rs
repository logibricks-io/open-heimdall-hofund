//! End-to-end Pi and omp reader tests against plugin-owned golden fixtures.

use std::path::{Path, PathBuf};

use hofund_plugin_agent_omp::OmpStore;
use hofund_plugin_agent_pi::PiStore;
use hofund_plugin_api::{SessionRecord, SessionStore};
use hofund_sessions::consent::Consent;
use hofund_sessions::index::SessionIndex;
use serde_json::{Value, json};

fn fixture(agent: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plugins/agents")
        .join(agent)
        .join("tests/fixtures")
        .join(name)
}

fn write_fixture(agent: &str, name: &str, path: &Path) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    std::fs::copy(fixture(agent, name), path).expect("fixture");
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

fn assert_golden(agent: &str, name: &str, records: &[SessionRecord]) {
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture(agent, &format!("{name}.json"))).expect("golden"),
    )
    .expect("golden JSON");
    assert_eq!(summaries(records), expected);
}

#[test]
fn pi_named_and_settings_session_dir_match_golden() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("agent");
    let custom = temp.path().join("custom");
    write_fixture(
        "pi",
        "main.jsonl",
        &root.join("sessions/work/2026-01-01T00-00-00_pi-file.jsonl"),
    );
    write_fixture(
        "pi",
        "custom.jsonl",
        &custom.join("2026-01-02T00-00-00_custom.jsonl"),
    );
    write_fixture("pi", "custom.jsonl", &custom.join("notes.jsonl"));
    write_fixture("pi", "custom.jsonl", &custom.join("nested/ignored.jsonl"));
    std::fs::create_dir_all(&root).expect("agent root");
    std::fs::write(
        root.join("settings.json"),
        serde_json::to_vec(&json!({"sessionDir": custom})).expect("settings"),
    )
    .expect("settings file");
    let store = PiStore::with_root(&root);
    assert_golden("pi", "pi", &records(&store, &temp.path().join("config")));
}

#[test]
fn omp_default_profile_xdg_and_artifacts_match_golden() {
    let temp = tempfile::tempdir().expect("temp dir");
    let default = temp.path().join(".omp/agent/sessions");
    let profile = temp.path().join(".omp/profiles/work/agent/sessions");
    let xdg = temp.path().join("xdg/omp/sessions");
    let main = default.join("work/2026-01-03T00-00-00_omp-main.jsonl");
    write_fixture("omp", "main.jsonl", &main);
    write_fixture(
        "omp",
        "artifact.jsonl",
        &main.with_extension("").join("omp-artifact.jsonl"),
    );
    write_fixture(
        "omp",
        "artifact.jsonl",
        &main.with_extension("").join("btw-history/ignored.jsonl"),
    );
    write_fixture(
        "omp",
        "profile.jsonl",
        &profile.join("work/2026-01-04T00-00-00_profile.jsonl"),
    );
    write_fixture(
        "omp",
        "xdg.jsonl",
        &xdg.join("work/2026-01-05T00-00-00_xdg.jsonl"),
    );
    let store = OmpStore::with_session_roots([default, profile, xdg]);
    assert_golden("omp", "omp", &records(&store, &temp.path().join("config")));
}

#[test]
fn pi_and_omp_resume_are_argv_without_commands() {
    let path = if cfg!(windows) {
        PathBuf::from(r"C:\work\pi.jsonl")
    } else {
        PathBuf::from("/work/pi.jsonl")
    };
    let pi = PiStore::with_session_roots("agent", None);
    assert_eq!(
        pi.resume_argv("pi:one", &path),
        Some(vec![
            "pi".to_owned(),
            "--session".to_owned(),
            path.to_string_lossy().into_owned(),
        ])
    );
    let omp = OmpStore::with_session_roots([]);
    assert_eq!(
        omp.resume_argv("omp:two", &path),
        Some(vec![
            "omp".to_owned(),
            "--resume".to_owned(),
            "two".to_owned()
        ])
    );
}
