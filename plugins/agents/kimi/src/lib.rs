//! Kimi Code session discovery and `wire.jsonl` parsing.
//!
//! The kernel supplies a consented root and depth-checked JSON values. This
//! crate owns only Kimi Code paths and record format.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, TimeZone, Utc};
use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionLocator, SessionParse, SessionParser,
    SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "kimi";

const GATEWAY_PREFIX: &str = "hofund/";

/// Kimi Code session store under `KIMI_CODE_HOME` or `~/.kimi-code`.
#[derive(Clone, Debug)]
pub struct KimiStore {
    locator: KimiLocator,
}

impl KimiStore {
    /// Uses `KIMI_CODE_HOME` or `~/.kimi-code`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_root(kimi_home())
    }

    /// Uses an explicit root for embedding and tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            locator: KimiLocator { root },
        }
    }
}

impl Default for KimiStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only Kimi Code locator.
#[derive(Clone, Debug)]
pub struct KimiLocator {
    root: PathBuf,
}

impl SessionLocator for KimiLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("sessions").is_dir()
    }
}

impl SessionStore for KimiStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let sessions = root.join("sessions");
        if !sessions.is_dir() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        collect_wires(&sessions, &mut paths)?;
        let mut files = Vec::new();
        for path in paths {
            let Some(session_id) = session_dir_id(&path) else {
                continue;
            };
            if !safe_id(&session_id) {
                continue;
            }
            let main = wire_agent_name(&path).is_some_and(|name| name == "main");
            let mut file = SessionFile::new(session_id, path);
            if main {
                let state = session_dir(&file.path).join("state.json");
                if state.is_file() {
                    file = file.with_related(vec![state]);
                }
            }
            files.push(file);
            let _ = main;
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        let snapshot = session_snapshot(&self.locator.root, file);
        let restored = state
            .map(json::from_str::<KimiState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid Kimi parser state"))?;
        let fresh = KimiState::new(file, &self.locator.root, &snapshot);
        let mut kim = if let Some(mut old) = restored {
            if old.session_id == fresh.session_id
                && old.carry.created_ms == fresh.carry.created_ms
                && old.carry.forked == fresh.carry.forked
            {
                old.apply_snapshot(&snapshot);
                old
            } else {
                fresh
            }
        } else {
            fresh
        };
        kim.home.clone_from(&self.locator.root);
        Ok(Box::new(KimiParser { state: kim }))
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id.strip_prefix(&format!("{AGENT}:"))?;
        if !safe_id(id) {
            return None;
        }
        Some(vec![
            "kimi".to_owned(),
            "--session".to_owned(),
            id.to_owned(),
        ])
    }
}

fn collect_wires(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), PluginError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|_| PluginError::io("cannot list Kimi sessions"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list Kimi sessions"))?;
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list Kimi sessions"))?;
        if file_type.is_dir() {
            collect_wires(&path, out)?;
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "wire.jsonl")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn session_dir(wire: &Path) -> PathBuf {
    wire.parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map_or_else(|| wire.to_path_buf(), Path::to_path_buf)
}

fn session_dir_id(wire: &Path) -> Option<String> {
    session_dir(wire)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

fn wire_agent_name(wire: &Path) -> Option<&str> {
    wire.parent().and_then(|dir| dir.file_name())?.to_str()
}

fn subagent_parent(wire: &Path, session_id: &str) -> Option<String> {
    let agent = wire_agent_name(wire)?;
    if agent == "main" {
        return None;
    }
    Some(format!("{AGENT}:{session_id}"))
}

fn kimi_home() -> PathBuf {
    if let Ok(value) = std::env::var("KIMI_CODE_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return expand_home(trimmed);
        }
    }
    user_home().join(".kimi-code")
}

fn expand_home(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        user_home().join(rest)
    } else if value == "~" {
        user_home()
    } else {
        PathBuf::from(value)
    }
}

fn user_home() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct KimiMeta {
    title: String,
    #[serde(rename = "titleKind", default)]
    title_kind: String,
    #[serde(rename = "isCustomTitle", default)]
    is_custom_title: Option<bool>,
    #[serde(rename = "customTitle", default)]
    custom_title: String,
    cwd: String,
    #[serde(rename = "workDir", default)]
    work_dir: String,
    #[serde(rename = "createdAt", default)]
    created_at: Value,
    #[serde(rename = "forkedFrom", default)]
    forked_from: String,
}

impl KimiMeta {
    fn names(&self) -> (String, String) {
        if !self.title.is_empty() && self.is_custom_title == Some(true) {
            return (title_line(&self.title), String::new());
        }
        if !self.title.is_empty() && self.title_kind == "custom" && self.is_custom_title.is_none() {
            return (title_line(&self.title), String::new());
        }
        if !self.title.is_empty() {
            return (String::new(), title_line(&self.title));
        }
        (title_line(&self.custom_title), String::new())
    }

    fn cwd(&self) -> String {
        if !self.cwd.is_empty() {
            return self.cwd.clone();
        }
        self.work_dir.clone()
    }

    fn created_ms(&self) -> i64 {
        kimi_time_ms(&self.created_at)
    }
}

#[derive(Clone, Debug, Default)]
struct SessionSnapshot {
    meta: Option<KimiMeta>,
    indexed_cwd: String,
    main: bool,
    parent: Option<String>,
}

fn session_snapshot(home: &Path, file: &SessionFile) -> SessionSnapshot {
    let main = wire_agent_name(&file.path).is_some_and(|name| name == "main");
    let meta = if main {
        let path = session_dir(&file.path).join("state.json");
        if path.is_file() {
            let bytes = std::fs::read(path).ok();
            bytes.and_then(|bytes| json::from_slice(&bytes).ok())
        } else {
            None
        }
    } else {
        None
    };
    let indexed_cwd = if main {
        indexed_work_dir(home, &file.id)
    } else {
        String::new()
    };
    let parent = if main {
        None
    } else {
        subagent_parent(&file.path, &file.id)
    };
    SessionSnapshot {
        meta,
        indexed_cwd,
        main,
        parent,
    }
}

fn indexed_work_dir(home: &Path, session_id: &str) -> String {
    let path = home.join("session_index.jsonl");
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return String::new();
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = value.get("sessionId").and_then(Value::as_str) else {
            continue;
        };
        if id == session_id {
            return value
                .get("workDir")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
        }
    }
    String::new()
}

fn kimi_time_ms(raw: &Value) -> i64 {
    if let Some(ms) = raw.as_i64() {
        return ms;
    }
    if let Some(text) = raw.as_str()
        && let Ok(at) = DateTime::parse_from_rfc3339(text)
    {
        return at.timestamp_millis();
    }
    0
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct KimiCarry {
    created_ms: i64,
    forked: bool,
    since_ms: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct KimiState {
    record: SessionRecord,
    custom: String,
    named: String,
    prompt: String,
    carry: KimiCarry,
    home: PathBuf,
    session_id: String,
    indexed_cwd: String,
}

impl KimiState {
    fn new(file: &SessionFile, home: &Path, snapshot: &SessionSnapshot) -> Self {
        let id = format!("{AGENT}:{}", file.id);
        let mut record = SessionRecord::new(AGENT, id, &file.path);
        record.title.clone_from(&file.id);
        record.main = snapshot.main;
        if let Some(parent) = &snapshot.parent {
            record = record.with_parent(parent.clone());
        }
        let mut carry = KimiCarry::default();
        if let Some(meta) = &snapshot.meta {
            carry.created_ms = meta.created_ms();
            carry.forked = !meta.forked_from.is_empty();
            if carry.forked {
                carry.since_ms = carry.created_ms;
            }
            record.cwd = meta.cwd();
            if record.cwd.is_empty() {
                record.cwd.clone_from(&snapshot.indexed_cwd);
            }
            let (custom, named) = meta.names();
            if record.start_ms == 0 && carry.created_ms > 0 && snapshot.main {
                record.start_ms = carry.created_ms;
                record.last_ms = carry.created_ms;
            }
            return Self {
                record,
                custom,
                named,
                prompt: String::new(),
                carry,
                home: home.to_path_buf(),
                session_id: file.id.clone(),
                indexed_cwd: snapshot.indexed_cwd.clone(),
            };
        }
        Self {
            record,
            custom: String::new(),
            named: String::new(),
            prompt: String::new(),
            carry,
            home: home.to_path_buf(),
            session_id: file.id.clone(),
            indexed_cwd: snapshot.indexed_cwd.clone(),
        }
    }

    fn apply_snapshot(&mut self, snapshot: &SessionSnapshot) {
        if !snapshot.main {
            return;
        }
        if let Some(meta) = &snapshot.meta {
            let (custom, named) = meta.names();
            self.custom = custom;
            self.named = named;
            let cwd = meta.cwd();
            if !cwd.is_empty() {
                self.record.cwd = cwd;
            } else if !snapshot.indexed_cwd.is_empty() {
                self.record.cwd.clone_from(&snapshot.indexed_cwd);
            }
            self.indexed_cwd.clone_from(&snapshot.indexed_cwd);
            let created = meta.created_ms();
            let forked = !meta.forked_from.is_empty();
            if created > 0 && self.carry.created_ms == created && self.carry.forked == forked {
                self.carry.created_ms = created;
                self.carry.forked = forked;
                if forked {
                    self.carry.since_ms = created;
                }
            }
        }
        self.finish_title();
    }

    fn finish_title(&mut self) {
        let fallback = self
            .record
            .id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(&self.record.id)
            .to_owned();
        self.record.title = if !self.custom.is_empty() {
            self.custom.clone()
        } else if !self.named.is_empty() {
            self.named.clone()
        } else if !self.prompt.is_empty() {
            title_line(&self.prompt)
        } else {
            fallback
        };
    }

    fn reset_after_fork_marker(&mut self) {
        let path = self.record.path.clone();
        let id = self.record.id.clone();
        let agent = self.record.agent.clone();
        let parent = self.record.parent_id.clone();
        let main = self.record.main;
        self.record = SessionRecord::new(&agent, id, path);
        self.record.main = main;
        self.record.parent_id = parent;
        self.prompt.clear();
        self.carry.since_ms = 0;
        if self.carry.created_ms > 0 && main {
            self.saw_ms(self.carry.created_ms);
        }
    }

    fn saw_ms(&mut self, ms: i64) {
        if ms <= 0 {
            return;
        }
        if self.record.start_ms == 0 || ms < self.record.start_ms {
            self.record.start_ms = ms;
        }
        self.record.last_ms = self.record.last_ms.max(ms);
    }
}

struct KimiParser {
    state: KimiState,
}

impl KimiParser {
    fn line(&mut self, value: &Value) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.state.carry.forked && kind == "forked" {
            self.state.reset_after_fork_marker();
            return;
        }
        if should_count(kind) {
            self.count_typed(value, kind);
            return;
        }
        if let Some(ms) = value.get("time").and_then(Value::as_i64)
            && ms > 0
            && ms >= self.state.carry.since_ms
        {
            self.state.saw_ms(ms);
        }
    }

    fn count_typed(&mut self, value: &Value, kind: &str) {
        let ms = value.get("time").and_then(Value::as_i64).unwrap_or(0);
        if ms <= 0 || ms < self.state.carry.since_ms {
            return;
        }
        self.state.saw_ms(ms);
        match kind {
            "usage.record" => self.usage_record(value, ms),
            "turn.prompt" => self.turn_prompt(value),
            "context.append_loop_event" => self.loop_event(value),
            _ => {}
        }
    }

    fn usage_record(&mut self, value: &Value, ms: i64) {
        let Some(usage) = value.get("usage") else {
            return;
        };
        let input_other = usage
            .get("inputOther")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let output = usage.get("output").and_then(Value::as_i64).unwrap_or(-1);
        let cache_read = usage
            .get("inputCacheRead")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        let cache_write = usage
            .get("inputCacheCreation")
            .and_then(Value::as_i64)
            .unwrap_or(-1);
        if input_other < 0 || output < 0 || cache_read < 0 || cache_write < 0 {
            return;
        }
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .map_or_else(|| "kimi".to_owned(), normalize_model);
        let tokens = SessionTokens::new(
            u64::try_from(input_other).unwrap_or(0),
            u64::try_from(output).unwrap_or(0),
            u64::try_from(cache_read).unwrap_or(0),
            u64::try_from(cache_write).unwrap_or(0),
        );
        if tokens.is_zero() {
            return;
        }
        self.state.record.add_usage(&model, tokens);
        let date = ms_date(ms);
        if !date.is_empty() {
            self.state
                .record
                .add_day(SessionDay::new(date, model, tokens));
        }
    }

    fn turn_prompt(&mut self, value: &Value) {
        if !self.state.record.main {
            return;
        }
        let origin = value
            .get("origin")
            .and_then(|origin| origin.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if origin != "user" {
            return;
        }
        if self.state.custom.is_empty() && self.state.named.is_empty() {
            let mut parts = Vec::new();
            if let Some(items) = value.get("input").and_then(Value::as_array) {
                for item in items {
                    if item.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = item.get("text").and_then(Value::as_str)
                    {
                        parts.push(text);
                    }
                }
            }
            let text = parts.join(" ");
            let prompt = title_line(&text);
            if !prompt.is_empty() {
                self.state.prompt = prompt;
            }
        }
    }

    fn loop_event(&mut self, value: &Value) {
        let Some(event) = value.get("event") else {
            return;
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match event_type {
            "step.end" => {
                if self.state.record.main
                    && event.get("finishReason").and_then(Value::as_str) == Some("end_turn")
                {
                    // Reply counts live in daily stats when a later row adds them.
                }
            }
            "tool.call" => {
                let _name = event
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
            }
            _ => {}
        }
    }
}

fn should_count(kind: &str) -> bool {
    matches!(
        kind,
        "usage.record" | "turn.prompt" | "context.append_loop_event"
    )
}

fn normalize_model(model: &str) -> String {
    let rest = model.strip_prefix(GATEWAY_PREFIX).unwrap_or(model);
    if rest.is_empty() {
        "kimi".to_owned()
    } else {
        rest.to_owned()
    }
}

fn ms_date(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|at| at.with_timezone(&Local).format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn title_line(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut line = trimmed.replace('\n', " ");
    if line.len() > 120 {
        let mut end = 120;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    line
}

impl SessionParser for KimiParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SessionParse, PluginError> {
        let mut state = self.state;
        if state.record.cwd.is_empty() && !state.indexed_cwd.is_empty() {
            state.record.cwd.clone_from(&state.indexed_cwd);
        }
        state.finish_title();
        let record = state.record.clone();
        let saved =
            json::to_string(&state).map_err(|_| PluginError::decode("cannot save Kimi state"))?;
        Ok(SessionParse::new(record, saved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_api::SessionStore;
    use std::fs;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn kimi_lists_wire_sessions() {
        let store = KimiStore::with_root(fixture("root"));
        let files = store.files(&fixture("root")).expect("files");
        assert!(files.len() >= 6, "{}", files.len());
    }

    #[test]
    fn kimi_golden_main_session() {
        let store = KimiStore::with_root(fixture("root"));
        let files = store.files(&fixture("root")).expect("files");
        let id = "session_fc16731e-2bfe-4873-a6cb-4b9e73280360";
        let file = files
            .iter()
            .find(|file| file.id == id && wire_agent_name(&file.path) == Some("main"))
            .expect("main wire");
        let parser = store.parser(file, None).expect("parser");
        let bytes = fs::read(&file.path).expect("wire");
        let mut parser = parser;
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let value: Value = json::from_slice(line).expect("line");
            parser.push(&value).expect("push");
        }
        let parsed = parser.finish().expect("finish");
        let record = parsed.record.expect("record");
        assert_eq!(record.title, "Review the diff");
        assert_eq!(record.cwd, "/work/kimi");
        let tokens = record
            .models
            .get("kimi-code/k3")
            .copied()
            .expect("model tokens");
        assert_eq!(tokens, SessionTokens::new(20_888, 2_176, 317_440, 0));
    }

    #[test]
    fn kimi_title_truncates_on_a_utf8_boundary() {
        let title = title_line(&format!("{}é", "a".repeat(119)));
        assert_eq!(title, "a".repeat(119));
        assert!(title.len() <= 120);
    }

    #[test]
    fn kimi_resume_argv() {
        let store = KimiStore::new();
        assert_eq!(
            store.resume_argv(
                "kimi:session_fc16731e-2bfe-4873-a6cb-4b9e73280360",
                Path::new("/any")
            ),
            Some(vec![
                "kimi".to_owned(),
                "--session".to_owned(),
                "session_fc16731e-2bfe-4873-a6cb-4b9e73280360".to_owned(),
            ])
        );
    }
}
