//! End-to-end Pi and omp reader tests against plugin-owned golden fixtures.

use std::path::{Path, PathBuf};

use hofund_plugin_agent_omp::OmpStore;
use hofund_plugin_agent_pi::PiStore;
use hofund_plugin_api::{SessionRecord, SessionStore, SessionTokens};
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

fn write_jsonl(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    let mut contents = lines
        .iter()
        .map(|line| serde_json::to_string(line).expect("JSONL line"))
        .collect::<Vec<_>>()
        .join("\n");
    contents.push('\n');
    std::fs::write(path, contents).expect("JSONL file");
}

fn assert_fork_usage(store: &dyn SessionStore, sessions: &Path, config: &Path) {
    let mut parent_lines = vec![
        json!({"type":"session","id":"parent","timestamp":"2026-02-01T12:00:00Z"}),
        json!({"type":"message","message":{"role":"user","content":"Parent prompt"},"timestamp":"2026-02-01T12:00:01Z"}),
        json!({"type":"message","message":{"role":"assistant","model":"model-parent","content":[],"usage":{"input":10,"output":4,"cacheRead":2,"cacheWrite":1}},"timestamp":"2026-02-01T12:00:02Z"}),
    ];
    let mut fork_lines = vec![
        json!({"type":"session","id":"child","parentSession":"parent","timestamp":"2026-02-01T13:00:00Z"}),
    ];
    fork_lines.extend(parent_lines.iter().skip(1).cloned());
    fork_lines.extend([
        json!({"type":"message","message":{"role":"user","content":"Child prompt"},"timestamp":"2026-02-01T13:00:01Z"}),
        json!({"type":"message","message":{"role":"assistant","model":"model-child","content":[],"usage":{"input":10,"output":4,"cacheRead":2,"cacheWrite":1}},"timestamp":"2026-02-01T13:00:02Z"}),
    ]);
    parent_lines.push(
        json!({"type":"message","message":{"role":"assistant","model":"model-parent","content":[],"usage":{"input":10,"output":4,"cacheRead":2,"cacheWrite":1}},"timestamp":"2026-02-01T13:00:03Z"}),
    );
    write_jsonl(
        &sessions.join("2026-02-01T12-00-00_parent-file.jsonl"),
        &parent_lines,
    );
    write_jsonl(
        &sessions.join("2026-02-01T13-00-00_child-file.jsonl"),
        &fork_lines,
    );

    let records = records(store, config);
    assert_eq!(records.len(), 2);
    let parent_id = format!("{}:parent", store.agent());
    let child_id = format!("{}:child", store.agent());
    let parent = records
        .iter()
        .find(|record| record.id == parent_id)
        .expect("parent record");
    let child = records
        .iter()
        .find(|record| record.id == child_id)
        .expect("child record");
    let parent_tokens = SessionTokens::new(20, 8, 4, 2);
    let post_fork = SessionTokens::new(10, 4, 2, 1);
    assert_eq!(child.forked_from.as_deref(), Some(parent_id.as_str()));
    assert!(child.fork_usage_filtered);
    assert_eq!(child.tokens(), post_fork);
    assert_eq!(child.days.len(), 1);
    assert_eq!(child.days[0].model, "model-child");
    assert_eq!(child.days[0].tokens, post_fork);
    assert_eq!(parent.tokens(), parent_tokens);
}

#[test]
fn pi_fork_keeps_post_fork_usage_when_parent_is_present() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("agent");
    let store = PiStore::with_session_roots(&root, None);
    assert_fork_usage(
        &store,
        &root.join("sessions/work"),
        &temp.path().join("config"),
    );
}

#[test]
fn omp_fork_keeps_post_fork_usage_when_parent_is_present() {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = temp.path().join("sessions");
    let store = OmpStore::with_session_roots([root.clone()]);
    assert_fork_usage(&store, &root.join("work"), &temp.path().join("config"));
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
