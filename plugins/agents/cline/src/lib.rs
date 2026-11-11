//! Cline CLI session discovery and whole-file messages parsing.
//!
//! The kernel supplies a consented root and depth-checked JSON values. This
//! crate owns only Cline's paths and record format.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use chrono::{DateTime, Local, TimeZone, Utc};
use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionFormat, SessionLocator, SessionParse,
    SessionParser, SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "cline";

/// Cline session store under the configured sessions directory.
#[derive(Clone, Debug)]
pub struct ClineStore {
    locator: ClineLocator,
}

impl ClineStore {
    /// Uses Cline's environment variables or `~/.cline/data/sessions`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_sessions_dir(cline_sessions_dir())
    }

    /// Uses an explicit sessions directory for embedding and tests.
    #[must_use]
    pub fn with_sessions_dir(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            locator: ClineLocator { root },
        }
    }
}

impl Default for ClineStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only Cline locator.
#[derive(Clone, Debug)]
pub struct ClineLocator {
    root: PathBuf,
}

impl SessionLocator for ClineLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.is_dir()
    }
}

impl SessionStore for ClineStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        let entries = std::fs::read_dir(root)
            .map_err(|_| PluginError::io("cannot list Cline sessions"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| PluginError::io("cannot list Cline sessions"))?;
        for entry in entries {
            let path = entry.path();
            if !entry
                .file_type()
                .map_err(|_| PluginError::io("cannot list Cline sessions"))?
                .is_dir()
            {
                continue;
            }
            let Some(session_id) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !safe_id(session_id) {
                continue;
            }
            let inner = std::fs::read_dir(&path)
                .map_err(|_| PluginError::io("cannot list Cline sessions"))?;
            for child in inner.flatten() {
                let child_path = child.path();
                let Some(name) = child_path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.ends_with(".messages.json") {
                    continue;
                }
                let main = name == format!("{session_id}.messages.json");
                let mut file =
                    SessionFile::new(session_id, child_path).with_format(SessionFormat::Json);
                if main {
                    let manifest = path.join(format!("{session_id}.json"));
                    if manifest.is_file() {
                        file = file.with_related(vec![manifest]);
                    }
                }
                files.push(file);
            }
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        let snapshot = manifest_snapshot(&file.path, file);
        let restored = state
            .map(json::from_str::<ClineState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid Cline parser state"))?;
        let fresh = ClineState::new(file, &snapshot)?;
        let state = if let Some(old) = restored {
            if old.stamp == fresh.stamp {
                let mut merged = old;
                merged.apply_snapshot(&snapshot);
                merged
            } else {
                fresh
            }
        } else {
            fresh
        };
        Ok(Box::new(ClineParser { state }))
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id.strip_prefix(&format!("{AGENT}:"))?;
        if !safe_id(id) {
            return None;
        }
        Some(vec!["cline".to_owned(), "--id".to_owned(), id.to_owned()])
    }
}

fn cline_sessions_dir() -> PathBuf {
    if let Ok(value) = std::env::var("CLINE_SESSION_DATA_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return expand_home(trimmed);
        }
    }
    if let Ok(value) = std::env::var("CLINE_DATA_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return expand_home(trimmed).join("sessions");
        }
    }
    let cline_dir = std::env::var("CLINE_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map_or_else(|| user_home().join(".cline"), |value| expand_home(value.trim()));
    cline_dir.join("data").join("sessions")
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
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.'
        })
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct FileStamp {
    size: u64,
    modified_ns: i64,
}

#[derive(Clone, Debug, Default)]
struct ManifestSnapshot {
    session_id: String,
    cwd: String,
    named: String,
    manifest_prompt: String,
    start: i64,
    since_ms: i64,
}

fn manifest_snapshot(messages: &Path, file: &SessionFile) -> ManifestSnapshot {
    let session_id = file.id.clone();
    let dir = messages.parent().unwrap_or(messages);
    let main = messages
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == format!("{session_id}.messages.json"));
    let manifest_path = dir.join(format!("{session_id}.json"));
    if !main || !manifest_path.is_file() {
        return ManifestSnapshot::default();
    }
    let Ok(bytes) = std::fs::read(manifest_path) else {
        return ManifestSnapshot::default();
    };
    let Ok(value) = json::from_slice::<Value>(&bytes) else {
        return ManifestSnapshot::default();
    };
    let start = value
        .get("started_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms);
    let since_ms = value
        .pointer("/metadata/fork/forkedAt")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
        .unwrap_or(0);
    ManifestSnapshot {
        session_id: value
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or(&session_id)
            .to_owned(),
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        named: value
            .pointer("/metadata/title")
            .and_then(Value::as_str)
            .map(title_line)
            .unwrap_or_default(),
        manifest_prompt: value
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        start: start.unwrap_or(0),
        since_ms,
    }
}

fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.timestamp_millis())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct ClineState {
    record: SessionRecord,
    named: String,
    prompt: String,
    since_ms: i64,
    start_ms: i64,
    stamp: FileStamp,
}

impl ClineState {
    fn new(file: &SessionFile, snapshot: &ManifestSnapshot) -> Result<Self, PluginError> {
        let session_id = &file.id;
        let main = file
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == format!("{session_id}.messages.json"));
        let id = if snapshot.session_id.is_empty() {
            format!("{AGENT}:{session_id}")
        } else {
            format!("{AGENT}:{}", snapshot.session_id)
        };
        let mut record = SessionRecord::new(AGENT, id, &file.path);
        record.main = main;
        if !main {
            record = record.with_parent(format!("{AGENT}:{session_id}"));
        }
        record.cwd.clone_from(&snapshot.cwd);
        record.title.clone_from(session_id);
        let stamp = file_stamp(&file.path)?;
        let start_ms = snapshot.start;
        let since_ms = snapshot.since_ms;
        if start_ms > 0 {
            record.start_ms = start_ms;
            record.last_ms = start_ms;
        }
        let named = snapshot.named.clone();
        let prompt = String::new();
        Ok(Self {
            record,
            named,
            prompt,
            since_ms,
            start_ms,
            stamp,
        })
    }

    fn apply_snapshot(&mut self, snapshot: &ManifestSnapshot) {
        if !snapshot.cwd.is_empty() {
            self.record.cwd.clone_from(&snapshot.cwd);
        }
        if !snapshot.session_id.is_empty() {
            self.record.id = format!("{AGENT}:{}", snapshot.session_id);
        }
        self.named.clone_from(&snapshot.named);
        if self.start_ms == 0 {
            self.start_ms = snapshot.start;
        }
        self.since_ms = snapshot.since_ms;
        self.finish_title(&snapshot.manifest_prompt);
    }

    fn finish_title(&mut self, manifest_prompt: &str) {
        let fallback = self
            .record
            .id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(&self.record.id)
            .to_owned();
        if !self.named.is_empty() {
            self.record.title = self.named.clone();
            return;
        }
        if !self.prompt.is_empty() {
            self.record.title = title_line(&self.prompt);
            return;
        }
        if !manifest_prompt.is_empty() && self.record.main {
            let (title, first) = cline_title_from_prompt(manifest_prompt);
            if !title.is_empty() {
                self.record.title = title;
                return;
            }
            if !first.is_empty() {
                self.record.title = first;
                return;
            }
        }
        self.record.title = fallback;
    }

    fn saw(&mut self, ms: i64) {
        if ms <= 0 {
            return;
        }
        if self.record.start_ms == 0 || ms < self.record.start_ms {
            self.record.start_ms = ms;
        }
        self.record.last_ms = self.record.last_ms.max(ms);
    }
}

fn file_stamp(path: &Path) -> Result<FileStamp, PluginError> {
    let metadata =
        std::fs::metadata(path).map_err(|_| PluginError::io("cannot stat Cline file"))?;
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0);
    Ok(FileStamp {
        size: metadata.len(),
        modified_ns,
    })
}

struct ClineParser {
    state: ClineState,
}

impl ClineParser {
    fn ingest(&mut self, value: &Value) {
        let Some(messages) = value.get("messages").and_then(Value::as_array) else {
            return;
        };
        let mut named_from_prompt = false;
        for message in messages {
            if !named_from_prompt && message.get("role").and_then(Value::as_str) == Some("user") {
                let text = message_text(message.get("content"));
                let (title, first) = cline_title_from_prompt(&text);
                if !title.is_empty() || !first.is_empty() {
                    self.state.prompt = text;
                    named_from_prompt = true;
                    let _ = (title, first);
                }
            }
            let ms = message.get("ts").and_then(Value::as_i64).unwrap_or(0);
            if ms > 0 && ms < self.state.since_ms {
                continue;
            }
            if ms > 0 {
                self.state.saw(ms);
            }
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(model) = message
                .get("modelInfo")
                .and_then(|info| info.get("id"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let Some(metrics) = message.get("metrics") else {
                continue;
            };
            let input = metrics
                .get("inputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = metrics
                .get("outputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_read = metrics
                .get("cacheReadTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let cache_write = metrics
                .get("cacheWriteTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let tokens = SessionTokens::new(
                input.saturating_sub(cache_read.saturating_add(cache_write)),
                output,
                cache_read,
                cache_write,
            );
            if tokens.is_zero() {
                continue;
            }
            self.state.record.add_usage(model, tokens);
            let date = ms_date(ms);
            if !date.is_empty() {
                self.state
                    .record
                    .add_day(SessionDay::new(date, model, tokens));
            }
        }
    }
}

fn message_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            parts.push(text);
        }
    }
    parts.join("\n")
}

static CLINE_NOTICE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<mode_notice\b[^>]*>.*?</mode_notice>").expect("cline notice regex")
});
static CLINE_INPUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"</?(?:user_input|user_command)\b[^>]*>").expect("cline input regex")
});

fn cline_title_from_prompt(prompt: &str) -> (String, String) {
    let without_notice = CLINE_NOTICE.replace_all(prompt, " ");
    let stripped = CLINE_INPUT.replace_all(without_notice.as_ref(), " ");
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return (String::new(), String::new());
    }
    if trimmed.starts_with('<') {
        return (String::new(), title_line(trimmed));
    }
    (title_line(trimmed), String::new())
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
        line.truncate(120);
    }
    line
}

impl SessionParser for ClineParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.ingest(value);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SessionParse, PluginError> {
        let mut state = self.state;
        let snapshot = manifest_snapshot(
            &state.record.path,
            &SessionFile::new(
                state
                    .record
                    .id
                    .strip_prefix(&format!("{AGENT}:"))
                    .unwrap_or(&state.record.id),
                state.record.path.clone(),
            ),
        );
        state.apply_snapshot(&snapshot);
        let record = state.record.clone();
        let saved =
            json::to_string(&state).map_err(|_| PluginError::decode("cannot save Cline state"))?;
        Ok(SessionParse::new(record, saved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_api::SessionStore;

    fn fixture_sessions() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root/sessions")
    }

    #[test]
    fn cline_lists_sessions() {
        let store = ClineStore::with_sessions_dir(fixture_sessions());
        let files = store.files(&fixture_sessions()).expect("files");
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn cline_golden_main_session() {
        let store = ClineStore::with_sessions_dir(fixture_sessions());
        let root = fixture_sessions();
        let files = store.files(&root).expect("files");
        let file = files
            .iter()
            .find(|file| file.id == "1790503200000_ab12c")
            .expect("main session");
        let index = SessionIndexHarness::new(store, file);
        let record = index.parse().expect("record");
        assert_eq!(record.title, "Importer speed-up");
        assert_eq!(record.cwd, "/work/cline");
        let opus = record.models.get("claude-opus-5-5").copied().expect("opus");
        assert_eq!(opus, SessionTokens::new(100, 50, 1000, 500));
    }

    #[test]
    fn cline_resume_argv() {
        let store = ClineStore::new();
        assert_eq!(
            store.resume_argv("cline:1790503200000_ab12c", Path::new("/any")),
            Some(vec![
                "cline".to_owned(),
                "--id".to_owned(),
                "1790503200000_ab12c".to_owned(),
            ])
        );
    }

    struct SessionIndexHarness {
        store: ClineStore,
        file: SessionFile,
    }

    impl SessionIndexHarness {
        fn new(store: ClineStore, file: &SessionFile) -> Self {
            Self {
                store,
                file: file.clone(),
            }
        }

        fn parse(&self) -> Result<SessionRecord, PluginError> {
            let parser = self.store.parser(&self.file, None)?;
            let bytes = std::fs::read(&self.file.path).expect("read");
            let value: Value = json::from_slice(&bytes).expect("json");
            let mut parser = parser;
            parser.push(&value)?;
            let parsed = parser.finish()?;
            parsed
                .record
                .ok_or_else(|| PluginError::decode("empty cline record"))
        }
    }
}
