use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{
    AgentDescription, AgentFile, AgentRoute, AgentSet, FileKind, PluginId, Slot, Visible,
};
use rusqlite::Connection;
use serde_json::{Value, json};

use super::{
    ACTIVE_WINDOW, Candidate, DEFAULT_PACKED_BUDGET, HEAD_LINES, LockOutcome, MAX_UNPACKED_SIZE,
    MovePolicy, MoveReport, PACKED_WINDOW, Rollout, StateRow, acquire_thread_lock,
    builtin_provider, collect_rollout_paths, connect_at, disconnect_at, discover_rollouts,
    move_provider, parse_config, process_candidate, read_config, read_rollout, read_state_db,
    read_state_rows, rewrite_rollout, update_state_row, valid_provider, valid_thread_id,
};
use crate::{AgentAdapter, Compiled, GatewayTarget, Session};

const NOW_SECONDS: u64 = 1_800_000_000;
const OPENAI: &str = "openai";
const HOFUND: &str = "hofund";

static FILES: &[AgentFile] = &[AgentFile::from_static(
    "config",
    FileKind::Toml,
    "~/.codex/config.toml",
    "~/.codex/config.toml",
    "~/.codex/config.toml",
    Some("CODEX_HOME"),
)
.with_env_replaces("~/.codex")];
static SETS: &[AgentSet] = &[
    AgentSet::from_static("config", "model_provider", Slot::Literal(HOFUND)),
    AgentSet::from_static(
        "config",
        "model_providers.hofund.name",
        Slot::Literal(HOFUND),
    ),
    AgentSet::from_static("config", "model_providers.hofund.base_url", Slot::V1),
    AgentSet::from_static(
        "config",
        "model_providers.hofund.wire_api",
        Slot::Literal("responses"),
    ),
    AgentSet::from_static(
        "config",
        "model_providers.hofund.experimental_bearer_token",
        Slot::Token,
    ),
];
static CODEX: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("codex"),
    "Codex CLI",
    "codex",
    FILES,
    SETS,
    Some("model"),
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_model_file("config");

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    next_id: usize,
}

struct Thread {
    id: String,
    path: PathBuf,
    original: Vec<u8>,
    modified: SystemTime,
}

impl Fixture {
    fn new(config: &str) -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("codex-home");
        fs::create_dir_all(&home).expect("home");
        fs::write(home.join("config.toml"), config).expect("config");
        let db = home.join("state_5.sqlite");
        let connection = Connection::open(db).expect("state DB");
        connection
            .execute_batch(
                "CREATE TABLE threads (\
                    id TEXT PRIMARY KEY, \
                    rollout_path TEXT NOT NULL, \
                    model_provider TEXT NOT NULL, \
                    updated_at INTEGER NOT NULL\
                 )",
            )
            .expect("threads table");
        Self {
            _temp: temp,
            home,
            next_id: 0,
        }
    }

    fn config_path(&self) -> PathBuf {
        self.home.join("config.toml")
    }

    fn add_thread(
        &mut self,
        provider: Option<&str>,
        packed: bool,
        updated_days_ago: u64,
        modified: SystemTime,
    ) -> Thread {
        self.next_id += 1;
        let id = format!("019a0000-0000-7000-8000-{:012x}", self.next_id);
        let plain = self
            .home
            .join("sessions/2026/10/01")
            .join(format!("rollout-2026-10-01T00-00-00-{id}.jsonl"));
        fs::create_dir_all(plain.parent().expect("rollout parent")).expect("sessions");
        let mut payload = json!({"id": id, "title": "keep  this title"});
        if let Some(provider) = provider {
            payload["model_provider"] = json!(provider);
        }
        let head = json!({"type": "session_meta", "payload": payload}).to_string();
        let bytes = format!(
            "{head}\n{{\"type\":\"response_item\",\"payload\":{{\"text\":\"keep  these bytes\"}}}}\n"
        )
        .into_bytes();
        let path = if packed {
            let packed_path = plain.with_extension("jsonl.zst");
            fs::write(&packed_path, zstd::bulk::compress(&bytes, 3).expect("pack")).expect("zst");
            packed_path
        } else {
            fs::write(&plain, &bytes).expect("rollout");
            plain.clone()
        };
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open mtime")
            .set_modified(modified)
            .expect("set mtime");
        let connection = Connection::open(self.home.join("state_5.sqlite")).expect("open state");
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, model_provider, updated_at) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    id,
                    plain.to_string_lossy(),
                    provider.unwrap_or(OPENAI),
                    (NOW_SECONDS - updated_days_ago * 24 * 60 * 60) as i64
                ],
            )
            .expect("thread row");
        Thread {
            id,
            path,
            original: if packed {
                zstd::bulk::compress(&bytes, 3).expect("original pack")
            } else {
                bytes
            },
            modified,
        }
    }

    fn prepare_connect(&self) {
        let config = self.config_path();
        let original = fs::read(&config).expect("original config");
        fs::write(config.with_file_name("config.toml.hofund.bak"), original).expect("backup");
        fs::write(&config, "model_provider = \"hofund\"\n").expect("connected config");
    }

    fn connect(&self, now: SystemTime, budget: u64) -> MoveReport {
        self.prepare_connect();
        connect_at(&self.config_path(), now, budget)
    }

    fn provider(&self, thread: &Thread) -> String {
        rollout_provider(&thread.path)
    }

    fn row_provider(&self, thread: &Thread) -> String {
        let connection = Connection::open(self.home.join("state_5.sqlite")).expect("open state");
        connection
            .query_row(
                "SELECT model_provider FROM threads WHERE id = ?",
                [&thread.id],
                |row| row.get(0),
            )
            .expect("thread row")
    }

    fn has_backup(&self, thread: &Thread) -> bool {
        contains_bytes(
            &self.home.join(".hofund/trash/codex-provider"),
            &thread.original,
        )
    }
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_SECONDS)
}

fn old() -> SystemTime {
    now() - Duration::from_secs(8 * 24 * 60 * 60)
}

fn rollout_provider(path: &Path) -> String {
    let bytes = fs::read(path).expect("rollout bytes");
    let bytes = if path.extension() == Some(std::ffi::OsStr::new("zst")) {
        zstd::bulk::decompress(&bytes, 8 * 1024 * 1024).expect("unpack")
    } else {
        bytes
    };
    for line in bytes.split(|byte| *byte == b'\n').take(8) {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session_meta") {
            return value
                .pointer("/payload/model_provider")
                .and_then(Value::as_str)
                .unwrap_or(OPENAI)
                .to_owned();
        }
    }
    panic!("no session meta in {}", path.display());
}

fn contains_bytes(root: &Path, wanted: &[u8]) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    for entry in entries {
        let path = entry.expect("backup entry").path();
        if path.is_dir() && contains_bytes(&path, wanted)
            || path.is_file() && fs::read(&path).is_ok_and(|bytes| bytes == wanted)
        {
            return true;
        }
    }
    false
}

fn assert_unchanged_except_provider(thread: &Thread) {
    let bytes = fs::read(&thread.path).expect("moved rollout");
    let text = String::from_utf8(bytes).expect("UTF-8 rollout");
    assert!(text.contains("\"title\":\"keep  this title\""), "{text}");
    assert!(text.contains("keep  these bytes"), "{text}");
    assert_eq!(
        fs::metadata(&thread.path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        thread.modified
    );
}

#[test]
fn connect_moves_the_previous_providers_plain_threads_and_keeps_backups() {
    let mut fixture = Fixture::new("model = \"mine\"\n");
    let missing = fixture.add_thread(None, false, 100, old());
    let other = fixture.add_thread(Some("custom"), false, 100, old());
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&missing), HOFUND);
    assert_eq!(fixture.row_provider(&missing), HOFUND);
    assert_eq!(fixture.provider(&other), "custom");
    assert_eq!(fixture.row_provider(&other), "custom");
    assert!(fixture.has_backup(&missing));
    assert!(!fixture.has_backup(&other));
    assert_unchanged_except_provider(&missing);

    let mut fixture =
        Fixture::new("model_provider = \"custom\"\n[model_providers.custom]\nname = \"Custom\"\n");
    let custom = fixture.add_thread(Some("custom"), false, 100, old());
    let openai = fixture.add_thread(Some(OPENAI), false, 100, old());
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&custom), HOFUND);
    assert_eq!(fixture.row_provider(&custom), HOFUND);
    assert_eq!(fixture.provider(&openai), OPENAI);
    assert_eq!(fixture.row_provider(&openai), OPENAI);
    assert!(fixture.has_backup(&custom));
}

#[test]
fn connect_moves_only_recent_packed_threads() {
    let mut fixture = Fixture::new("");
    let recent = fixture.add_thread(Some(OPENAI), true, 8, old());
    let ancient = fixture.add_thread(
        Some(OPENAI),
        true,
        40,
        now() - Duration::from_secs(40 * 24 * 60 * 60),
    );
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&recent), HOFUND);
    assert_eq!(fixture.row_provider(&recent), HOFUND);
    assert_eq!(fixture.provider(&ancient), OPENAI);
    assert_eq!(fixture.row_provider(&ancient), OPENAI);
    assert!(fixture.has_backup(&recent));
    assert!(!fixture.has_backup(&ancient));
    assert_eq!(
        fs::metadata(&recent.path)
            .expect("metadata")
            .modified()
            .expect("mtime"),
        recent.modified
    );
}

#[test]
fn packed_budget_stops_after_the_newest_thread_and_the_next_call_continues() {
    let mut fixture = Fixture::new("");
    let newer = fixture.add_thread(Some(OPENAI), true, 9, old());
    let newest = fixture.add_thread(Some(OPENAI), true, 8, old());
    let report = fixture.connect(now(), 1);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&newest), HOFUND);
    assert_eq!(fixture.provider(&newer), OPENAI);

    let report = connect_at(&fixture.config_path(), now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&newer), HOFUND);
}

#[test]
fn disconnect_returns_threads_to_the_restored_or_builtin_provider() {
    let mut fixture = Fixture::new("");
    let thread = fixture.add_thread(Some(HOFUND), false, 1, old());
    fs::write(
        fixture.config_path(),
        "model_provider = \"custom\"\n[model_providers.custom]\nname = \"Custom\"\n",
    )
    .expect("restored config");
    let report = disconnect_at(&fixture.config_path(), now());
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&thread), "custom");
    assert_eq!(fixture.row_provider(&thread), "custom");
    assert!(fixture.has_backup(&thread));

    fs::write(fixture.config_path(), "model = \"mine\"\n").expect("removed table");
    let report = disconnect_at(&fixture.config_path(), now());
    assert_eq!(report.moved, 0, "{report:?}");
    assert_eq!(fixture.provider(&thread), "custom");

    let thread = fixture.add_thread(Some(HOFUND), true, 1, old());
    let report = disconnect_at(&fixture.config_path(), now());
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&thread), OPENAI);
    assert_eq!(fixture.row_provider(&thread), OPENAI);

    fs::write(fixture.config_path(), "model_provider = \"hofund\"\n").expect("still connected");
    let report = disconnect_at(&fixture.config_path(), now());
    assert_eq!(report.moved, 0, "{report:?}");
    assert_eq!(fixture.provider(&thread), OPENAI);
}

#[test]
fn cc_switch_official_threads_follow_connect() {
    let mut fixture = Fixture::new("");
    let previous = fixture.add_thread(Some(OPENAI), false, 100, old());
    let official = fixture.add_thread(Some("cc-switch-official"), false, 100, old());
    let other = fixture.add_thread(Some("custom"), false, 100, old());
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 2, "{report:?}");
    assert_eq!(fixture.provider(&previous), HOFUND);
    assert_eq!(fixture.provider(&official), HOFUND);
    assert_eq!(fixture.row_provider(&official), HOFUND);
    assert_eq!(fixture.provider(&other), "custom");
}

#[test]
fn busy_rollout_is_skipped_and_reported() {
    let mut fixture = Fixture::new("");
    let quiet = fixture.add_thread(Some(OPENAI), false, 1, old());
    let busy = fixture.add_thread(Some(OPENAI), false, 1, now() - Duration::from_secs(1));
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&quiet), HOFUND);
    assert_eq!(fixture.provider(&busy), OPENAI);
    assert_eq!(fixture.row_provider(&busy), OPENAI);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains(&busy.id) && note.contains("busy")),
        "{report:?}"
    );
}

#[test]
fn repeated_connect_is_a_noop_and_profiles_leave_threads_alone() {
    let mut fixture = Fixture::new("");
    fixture.add_thread(Some(OPENAI), false, 1, old());
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1);
    let report = connect_at(&fixture.config_path(), now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report, MoveReport::default());

    let mut fixture = Fixture::new(
        "model_provider = \"custom\"\nprofile = \"relay\"\n\
         [model_providers.custom]\nname = \"Custom\"\n\
         [profiles.relay]\nmodel_provider = \"custom\"\n",
    );
    let thread = fixture.add_thread(Some("custom"), false, 1, old());
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report, MoveReport::default());
    assert_eq!(fixture.provider(&thread), "custom");
    assert_eq!(fixture.row_provider(&thread), "custom");
}

#[test]
fn malformed_rollout_missing_or_locked_db_does_not_fail_config_connect() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let codex_home = home.join(".codex");
    fs::create_dir_all(codex_home.join("sessions/2026/10/01")).expect("sessions");
    fs::write(
        codex_home.join("sessions/2026/10/01/rollout-broken.jsonl"),
        b"not json\n",
    )
    .expect("malformed rollout");
    let session = Session::new(&home).with_codex_home(&codex_home);
    let target = GatewayTarget::new("http://127.0.0.1:3425", "token", vec!["m".to_owned()]);
    let adapter = Compiled::new(&CODEX);
    adapter.apply(&session, &target).expect("config connects");
    assert!(
        fs::read_to_string(codex_home.join("config.toml"))
            .expect("config")
            .contains("model_provider = \"hofund\"")
    );
    let outcome = adapter
        .disconnect(&session, false)
        .expect("config disconnects");
    assert!(outcome.changed());

    let mut fixture = Fixture::new("");
    let thread = fixture.add_thread(Some(OPENAI), false, 1, old());
    let connection = Connection::open(fixture.home.join("state_5.sqlite")).expect("lock DB");
    connection
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("state_5.sqlite")),
        "{report:?}"
    );
    assert_eq!(fixture.provider(&thread), HOFUND);
    drop(connection);
}

#[test]
fn adapter_connect_and_disconnect_move_a_real_thread() {
    let mut fixture = Fixture::new("model = \"mine\"\n");
    let thread = fixture.add_thread(
        Some(OPENAI),
        false,
        1,
        SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60),
    );
    let session = Session::new(fixture.home.join("session-home")).with_codex_home(&fixture.home);
    let target = GatewayTarget::new("http://127.0.0.1:3425", "token", vec!["m".to_owned()]);
    let adapter = Compiled::new(&CODEX);
    adapter.apply(&session, &target).expect("apply");
    assert_eq!(fixture.provider(&thread), HOFUND);
    assert_eq!(fixture.row_provider(&thread), HOFUND);
    assert!(fixture.has_backup(&thread));

    adapter.disconnect(&session, false).expect("disconnect");
    assert_eq!(fixture.provider(&thread), OPENAI);
    assert_eq!(fixture.row_provider(&thread), OPENAI);
    assert_eq!(
        fs::read_to_string(fixture.config_path()).expect("restored"),
        "model = \"mine\"\n"
    );
}

#[test]
fn adapter_without_sessions_keeps_the_existing_lifecycle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let codex_home = home.join("codex-override");
    fs::create_dir_all(&codex_home).expect("home");
    let original = "model = \"mine\"\n# keep me\n";
    fs::write(codex_home.join("config.toml"), original).expect("config");
    let session = Session::new(&home).with_codex_home(&codex_home);
    let target = GatewayTarget::new("http://127.0.0.1:3425", "token", vec!["m".to_owned()]);
    let adapter = Compiled::new(&CODEX);
    adapter.apply(&session, &target).expect("apply");
    let outcome = adapter.disconnect(&session, false).expect("disconnect");
    assert!(outcome.warnings().is_empty(), "{:?}", outcome.warnings());
    assert_eq!(
        fs::read_to_string(codex_home.join("config.toml")).expect("restored"),
        original
    );
    assert!(!codex_home.join(".hofund").exists());
}

#[test]
fn limits_have_the_expected_exact_values() {
    assert_eq!(DEFAULT_PACKED_BUDGET, 134_217_728);
    assert_eq!(MAX_UNPACKED_SIZE, 536_870_912);
    assert_eq!(PACKED_WINDOW, Duration::from_secs(2_592_000));
    assert_eq!(ACTIVE_WINDOW, Duration::from_secs(60));
    assert_eq!(HEAD_LINES, 8);
}

#[test]
fn move_report_log_returns_the_notes() {
    let notes = vec!["first note".to_owned(), "second note".to_owned()];
    let report = MoveReport {
        moved: 1,
        notes: notes.clone(),
    };
    assert_eq!(report.log(), notes);
}

#[test]
fn thread_lock_can_be_reacquired_after_drop() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::create_dir(temp.path().join("thread-writer-locks")).expect("lock directory");
    let first = acquire_thread_lock(temp.path(), "thread-id").expect("first lock");
    assert!(matches!(&first, LockOutcome::Held(_)));
    drop(first);
    let second = acquire_thread_lock(temp.path(), "thread-id").expect("second lock");
    assert!(matches!(&second, LockOutcome::Held(_)));
}

#[test]
fn connect_reports_an_unreadable_backup_instead_of_using_the_current_config() {
    let mut fixture = Fixture::new("");
    let thread = fixture.add_thread(Some(OPENAI), false, 1, old());
    let backup = fixture
        .config_path()
        .with_file_name("config.toml.hofund.bak");
    fs::create_dir(&backup).expect("backup directory");
    let report = connect_at(&fixture.config_path(), now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 0, "{report:?}");
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("cannot read") && note.contains("hofund.bak")),
        "{report:?}"
    );
    assert_eq!(fixture.provider(&thread), OPENAI);
}

#[test]
fn connect_uses_the_current_config_when_no_backup_exists() {
    let mut fixture = Fixture::new("");
    let thread = fixture.add_thread(Some(OPENAI), false, 1, old());
    let report = connect_at(&fixture.config_path(), now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&thread), HOFUND);
    assert_eq!(fixture.row_provider(&thread), HOFUND);
}

#[test]
fn connect_does_not_move_a_provider_owned_by_an_unselected_profile() {
    let mut fixture = Fixture::new(
        "model_provider = \"custom\"\n[profiles.relay]\nmodel_provider = \"custom\"\n",
    );
    let thread = fixture.add_thread(Some("custom"), false, 1, old());
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report, MoveReport::default());
    assert_eq!(fixture.provider(&thread), "custom");
    assert_eq!(fixture.row_provider(&thread), "custom");
}

#[test]
fn connect_leaves_official_threads_alone_when_a_profile_owns_that_route() {
    let mut fixture = Fixture::new(
        "model_provider = \"openai\"\n[profiles.relay]\nmodel_provider = \"cc-switch-official\"\n",
    );
    let thread = fixture.add_thread(Some("cc-switch-official"), false, 1, old());
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report, MoveReport::default());
    assert_eq!(fixture.provider(&thread), "cc-switch-official");
    assert_eq!(fixture.row_provider(&thread), "cc-switch-official");
}

#[test]
fn move_provider_rejects_noop_and_invalid_provider_moves() {
    let mut fixture = Fixture::new("");
    let noop = fixture.add_thread(Some(OPENAI), false, 1, old());
    let mut report = MoveReport::default();
    move_provider(
        &fixture.home,
        &fixture.home,
        OPENAI,
        OPENAI,
        now(),
        MovePolicy::All,
        &mut report,
    );
    assert_eq!(report, MoveReport::default());
    assert_eq!(fixture.provider(&noop), OPENAI);

    let invalid_source = fixture.add_thread(Some("bad!"), false, 1, old());
    move_provider(
        &fixture.home,
        &fixture.home,
        "bad!",
        HOFUND,
        now(),
        MovePolicy::All,
        &mut report,
    );
    assert_eq!(report, MoveReport::default());
    assert_eq!(fixture.provider(&invalid_source), "bad!");

    let invalid_target = fixture.add_thread(Some(OPENAI), false, 1, old());
    move_provider(
        &fixture.home,
        &fixture.home,
        OPENAI,
        "bad!",
        now(),
        MovePolicy::All,
        &mut report,
    );
    assert_eq!(report, MoveReport::default());
    assert_eq!(fixture.provider(&invalid_target), OPENAI);
}

#[test]
fn rollout_exactly_at_the_active_window_boundary_is_not_busy() {
    let mut fixture = Fixture::new("");
    let thread = fixture.add_thread(Some(OPENAI), false, 1, now() - ACTIVE_WINDOW);
    let report = fixture.connect(now(), DEFAULT_PACKED_BUDGET);
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(fixture.provider(&thread), HOFUND);
}

#[test]
fn candidate_with_no_file_or_row_changes_is_not_counted() {
    let mut fixture = Fixture::new("");
    let thread = fixture.add_thread(Some(HOFUND), false, 1, old());
    let candidate = Candidate {
        id: thread.id,
        files: vec![Rollout {
            path: thread.path,
            provider: HOFUND.to_owned(),
            packed: false,
        }],
        rows: Vec::new(),
        updated_at: None,
        packed: false,
    };
    let mut report = MoveReport::default();
    process_candidate(&fixture.home, OPENAI, HOFUND, now(), candidate, &mut report);
    assert_eq!(report, MoveReport::default());
}

#[test]
fn failed_second_rewrite_restores_the_first_rollout_and_its_mtime() {
    let mut fixture = Fixture::new("");
    let first = fixture.add_thread(Some(OPENAI), false, 1, old());
    let second_path = first
        .path
        .with_file_name(format!("rollout-second-{}.jsonl", first.id));
    let second_bytes = format!(
        "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{0}\",\"id\":\"{0}\",\"model_provider\":\"openai\"}}}}\n",
        first.id
    )
    .into_bytes();
    fs::write(&second_path, &second_bytes).expect("second rollout");
    OpenOptions::new()
        .write(true)
        .open(&second_path)
        .expect("open second rollout")
        .set_modified(old())
        .expect("set second mtime");
    let candidate = Candidate {
        id: first.id.clone(),
        files: vec![
            Rollout {
                path: first.path.clone(),
                provider: OPENAI.to_owned(),
                packed: false,
            },
            Rollout {
                path: second_path.clone(),
                provider: OPENAI.to_owned(),
                packed: false,
            },
        ],
        rows: Vec::new(),
        updated_at: None,
        packed: false,
    };
    let mut report = MoveReport::default();
    process_candidate(&fixture.home, OPENAI, HOFUND, now(), candidate, &mut report);
    assert_eq!(report.moved, 0, "{report:?}");
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("was not moved") && note.contains("cannot edit")),
        "{report:?}"
    );
    assert_eq!(
        fs::read(&first.path).expect("restored bytes"),
        first.original
    );
    assert_eq!(
        fs::metadata(&first.path)
            .expect("restored metadata")
            .modified()
            .expect("restored mtime"),
        first.modified
    );
    assert_eq!(fs::read(second_path).expect("second bytes"), second_bytes);
}

#[test]
fn rewrite_rollout_ignores_session_meta_after_the_eighth_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("rollout.jsonl");
    let mut bytes = b"{\"type\":\"response_item\"}\n".repeat(HEAD_LINES);
    bytes.extend_from_slice(
        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"model_provider\":\"openai\"}}\n",
    );
    fs::write(&path, &bytes).expect("rollout");
    assert_eq!(rewrite_rollout(&path, "thread", OPENAI, HOFUND), Ok(false));
    assert_eq!(fs::read(path).expect("unchanged rollout"), bytes);
}

#[test]
fn rewrite_rollout_leaves_mismatched_foreign_and_already_moved_metadata_alone() {
    let temp = tempfile::tempdir().expect("temp dir");
    let assert_unchanged = |name: &str, metadata: Value| {
        let path = temp.path().join(name);
        let bytes = format!("{metadata}\n").into_bytes();
        fs::write(&path, &bytes).expect("rollout");
        assert_eq!(rewrite_rollout(&path, "thread", OPENAI, HOFUND), Ok(false));
        assert_eq!(fs::read(path).expect("unchanged rollout"), bytes);
    };
    assert_unchanged(
        "mismatched.jsonl",
        json!({"type": "session_meta", "payload": {"id": "other", "model_provider": OPENAI}}),
    );
    assert_unchanged(
        "foreign.jsonl",
        json!({"type": "session_meta", "payload": {"id": "thread", "model_provider": "custom"}}),
    );
    assert_unchanged(
        "already-moved.jsonl",
        json!({"type": "session_meta", "payload": {"id": "thread", "model_provider": HOFUND}}),
    );
}

#[test]
fn update_state_row_reports_whether_a_row_changed() {
    let mut fixture = Fixture::new("");
    let thread = fixture.add_thread(Some(OPENAI), false, 1, old());
    let row = StateRow {
        db: fixture.home.join("state_5.sqlite"),
        id: thread.id,
        provider: OPENAI.to_owned(),
        updated_at: None,
    };
    assert_eq!(update_state_row(&row, OPENAI, HOFUND), Ok(true));
    assert_eq!(update_state_row(&row, OPENAI, HOFUND), Ok(false));
}

#[test]
fn discover_rollouts_requires_a_valid_id_that_appears_in_the_filename() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    for (name, id) in [
        ("rollout-other.jsonl", "valid-id"),
        ("rollout-bad.id.jsonl", "bad.id"),
    ] {
        fs::write(
            sessions.join(name),
            format!(
                "{}\n",
                json!({"type": "session_meta", "payload": {"id": id, "model_provider": OPENAI}})
            ),
        )
        .expect("rollout");
    }
    let mut report = MoveReport::default();
    let groups = discover_rollouts(temp.path(), &mut report);
    assert!(groups.is_empty(), "{groups:?}");
    assert_eq!(report, MoveReport::default());
}

#[test]
fn collect_rollout_paths_handles_io_errors_and_filters_both_name_parts() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("missing");
    let mut paths = Vec::new();
    let mut report = MoveReport::default();
    collect_rollout_paths(&missing, &mut paths, &mut report);
    assert!(paths.is_empty());
    assert_eq!(report, MoveReport::default());

    let not_a_directory = temp.path().join("file");
    fs::write(&not_a_directory, b"file").expect("file");
    collect_rollout_paths(&not_a_directory, &mut paths, &mut report);
    assert_eq!(report.notes.len(), 1, "{report:?}");
    assert!(report.notes[0].contains("cannot read Codex sessions"));

    let sessions = temp.path().join("sessions");
    fs::create_dir(&sessions).expect("sessions");
    fs::write(sessions.join("rollout-id.txt"), b"wrong extension").expect("txt");
    fs::write(sessions.join("other.jsonl"), b"wrong prefix").expect("jsonl");
    let mut report = MoveReport::default();
    collect_rollout_paths(&sessions, &mut paths, &mut report);
    assert!(paths.is_empty(), "{paths:?}");
    assert_eq!(report, MoveReport::default());
}

#[test]
fn read_rollout_rejects_an_empty_provider() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("rollout.jsonl");
    fs::write(
        &path,
        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread\",\"model_provider\":\"\"}}\n",
    )
    .expect("rollout");
    assert_eq!(read_rollout(&path), Ok(None));
}

#[test]
fn read_state_rows_handles_io_errors_and_requires_the_full_database_filename() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("missing");
    let mut report = MoveReport::default();
    assert!(read_state_rows(&missing, &mut report).is_empty());
    assert_eq!(report, MoveReport::default());

    let not_a_directory = temp.path().join("file");
    fs::write(&not_a_directory, b"file").expect("file");
    assert!(read_state_rows(&not_a_directory, &mut report).is_empty());
    assert_eq!(report.notes.len(), 1, "{report:?}");
    assert!(report.notes[0].contains("cannot read Codex state directory"));

    let database_home = temp.path().join("databases");
    fs::create_dir(&database_home).expect("database directory");
    let connection = Connection::open(database_home.join("state_5.txt")).expect("state DB");
    connection
        .execute_batch(
            "CREATE TABLE threads (id TEXT, model_provider TEXT);\
             INSERT INTO threads VALUES ('thread', 'openai');",
        )
        .expect("state row");
    drop(connection);
    let mut report = MoveReport::default();
    assert!(read_state_rows(&database_home, &mut report).is_empty());
    assert_eq!(report, MoveReport::default());
}

#[test]
fn read_state_db_checks_columns_and_skips_invalid_ids_and_providers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing_provider = temp.path().join("missing-provider.sqlite");
    let connection = Connection::open(&missing_provider).expect("state DB");
    connection
        .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY)")
        .expect("threads table");
    drop(connection);
    assert!(
        read_state_db(&missing_provider)
            .expect("missing column")
            .is_empty()
    );

    let validated = temp.path().join("validated.sqlite");
    let connection = Connection::open(&validated).expect("state DB");
    connection
        .execute_batch(
            "CREATE TABLE threads (id TEXT, model_provider TEXT, updated_at INTEGER);\
             INSERT INTO threads VALUES ('valid', 'openai', 1);\
             INSERT INTO threads VALUES ('bad.id', 'openai', 2);\
             INSERT INTO threads VALUES ('other', 'bad provider', 3);\
             INSERT INTO threads VALUES ('bad.id', 'bad provider', 4);",
        )
        .expect("state rows");
    drop(connection);
    let rows = read_state_db(&validated).expect("valid rows");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].id, "valid");
    assert_eq!(rows[0].provider, OPENAI);
    assert_eq!(rows[0].updated_at, Some(1));
}

#[test]
fn read_state_db_reports_row_type_errors() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("state.sqlite");
    let connection = Connection::open(&path).expect("state DB");
    connection
        .execute_batch(
            "CREATE TABLE threads (id INTEGER, model_provider TEXT);\
             INSERT INTO threads VALUES (7, 'openai');",
        )
        .expect("invalid state row");
    drop(connection);
    let error = read_state_db(&path).expect_err("row type must be rejected");
    assert!(error.contains("cannot read Codex state"), "{error}");
}

#[test]
fn parse_config_treats_empty_strings_as_absent() {
    let config = parse_config(
        b"model_provider = \"\"\nprofile = \"\"\nsqlite_home = \"\"\n\
          [profiles.\"\"]\nmodel_provider = \"custom\"\n\
          [profiles.blank]\nmodel_provider = \"\"\n",
    )
    .expect("config");
    assert_eq!(config.provider, OPENAI);
    assert!(!config.selected_profile);
    assert!(config.profile_providers.contains("custom"));
    assert!(!config.profile_providers.contains(""));
    assert_eq!(config.sqlite_home, None);

    let config = parse_config(
        b"model_provider = \"openai\"\nprofile = \"blank\"\n\
          [profiles.blank]\nmodel_provider = \"\"\n",
    )
    .expect("config");
    assert_eq!(config.provider, OPENAI);
    assert!(!config.selected_profile);
}

#[test]
fn read_config_defaults_only_a_missing_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let missing = temp.path().join("missing.toml");
    let config = read_config(&missing).expect("missing config defaults");
    assert_eq!(config.provider, OPENAI);
    assert_eq!(config.sqlite_home, None);

    let directory = temp.path().join("config.toml");
    fs::create_dir(&directory).expect("config directory");
    let error = read_config(&directory).expect_err("directory is not a config");
    assert!(error.contains("cannot read"), "{error}");
}

#[test]
fn builtin_provider_recognizes_only_codex_builtins() {
    for provider in [OPENAI, "ollama", "lmstudio"] {
        assert!(builtin_provider(provider), "{provider}");
    }
    for provider in [HOFUND, "custom", ""] {
        assert!(!builtin_provider(provider), "{provider}");
    }
}

#[test]
fn provider_and_thread_id_validation_enforce_lengths_and_characters() {
    assert!(valid_provider(&"p".repeat(64)));
    assert!(!valid_provider(&"p".repeat(65)));
    assert!(valid_thread_id(&"t".repeat(128)));
    assert!(!valid_thread_id(&"t".repeat(129)));
    for provider in ["", "bad provider", "bad/provider", "non-ascii-\u{80}"] {
        assert!(!valid_provider(provider), "{provider:?}");
    }
    for id in ["", "bad.id", "bad id", "bad/id", "non-ascii-\u{80}"] {
        assert!(!valid_thread_id(id), "{id:?}");
    }
    assert!(valid_provider("letters_123.dots-and-dashes"));
    assert!(valid_thread_id("letters_123-and-dashes"));
}
