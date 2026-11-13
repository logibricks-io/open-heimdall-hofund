//! Raft (slock) session discovery and Pi-shaped JSONL parsing.
//!
//! The kernel supplies a consented root and depth-checked JSON values. This
//! crate owns only Raft's paths and Pi-compatible record format.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionLocator, SessionParse, SessionParser,
    SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "raft";

/// Raft's session store under the slock home.
#[derive(Clone, Debug)]
pub struct RaftStore {
    locator: RaftLocator,
}

impl RaftStore {
    /// Uses `$SLOCK_HOME` or `~/.slock`.
    #[must_use]
    pub fn new() -> Self {
        let root = std::env::var_os("SLOCK_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(home_root, PathBuf::from);
        Self::with_root(root)
    }

    /// Uses an explicit root, for embedding and tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            locator: RaftLocator { root: root.into() },
        }
    }
}

impl Default for RaftStore {
    fn default() -> Self {
        Self::new()
    }
}

fn home_root() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(
        || PathBuf::from(".slock"),
        |home| PathBuf::from(home).join(".slock"),
    )
}

/// Directory-only Raft locator.
#[derive(Clone, Debug)]
pub struct RaftLocator {
    root: PathBuf,
}

impl SessionLocator for RaftLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("agents").is_dir()
    }
}

impl SessionStore for RaftStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let agents = root.join("agents");
        if !agents.is_dir() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        let Ok(entries) = std::fs::read_dir(&agents) else {
            return Ok(Vec::new());
        };
        for agent in entries.flatten().filter(|entry| entry.path().is_dir()) {
            for folder in [".pi-sessions", ".builtin-sessions"] {
                let base = agent.path().join(folder);
                if !base.is_dir() {
                    continue;
                }
                walk_linked(&base, &base, &mut paths)
                    .map_err(|_| PluginError::io("cannot list Raft sessions"))?;
            }
        }
        let mut files = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            {
                continue;
            }
            let Some(id) = pi_session_id(name) else {
                continue;
            };
            if seen.insert(path.clone()) {
                files.push(SessionFile::new(id, path));
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
        let restored = state
            .map(json::from_str::<PiState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid Raft parser state"))?;
        Ok(Box::new(PiParser {
            state: restored.unwrap_or_else(|| PiState::new(file)),
        }))
    }
}

fn pi_session_id(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    let (_, id) = stem.split_once('_')?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_owned())
}

fn walk_linked(root: &Path, lexical: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(root)?;
    let walk = if metadata.file_type().is_symlink() {
        root.canonicalize()?
    } else {
        root.to_path_buf()
    };
    walk_real(&walk, lexical, out)
}

fn walk_real(real: &Path, lexical: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(real)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let real_path = entry.path();
        let lexical_path = lexical.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            walk_real(&real_path, &lexical_path, out)?;
        } else if kind.is_file() || kind.is_symlink() {
            out.push(lexical_path);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct PiUsage {
    input: u64,
    output: u64,
    #[serde(rename = "cacheRead", default)]
    cache_read: u64,
    #[serde(rename = "cacheWrite", default)]
    cache_write: u64,
}

impl PiUsage {
    fn tokens(self) -> SessionTokens {
        SessionTokens::new(self.input, self.output, self.cache_read, self.cache_write)
    }

    fn of(value: &Value) -> Option<Self> {
        serde::Deserialize::deserialize(value).ok()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct PiState {
    record: SessionRecord,
    named: String,
    first: String,
    model: String,
    #[serde(default)]
    fork_since_ms: i64,
    #[serde(default)]
    session_id_set: bool,
}

impl PiState {
    fn new(file: &SessionFile) -> Self {
        let id = format!("{AGENT}:{}", file.id);
        Self {
            record: SessionRecord::new(AGENT, &id, &file.path),
            named: String::new(),
            first: String::new(),
            model: String::new(),
            fork_since_ms: 0,
            session_id_set: false,
        }
    }

    fn finish_title(&mut self) {
        self.record.title = if !self.named.is_empty() {
            self.named.clone()
        } else if !self.first.is_empty() {
            self.first.clone()
        } else {
            self.record
                .id
                .strip_prefix(&format!("{AGENT}:"))
                .unwrap_or(&self.record.id)
                .to_owned()
        };
    }
}

struct PiParser {
    state: PiState,
}

impl PiParser {
    fn push_line(&mut self, value: &Value) {
        let at = line_time(value);
        let copied = self.state.fork_since_ms > 0
            && at.is_some_and(|stamp| stamp.timestamp_millis() < self.state.fork_since_ms);
        if !copied {
            self.saw(at);
        }
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "session" if !self.state.session_id_set => self.session_line(value, at),
            "session_info" => self.title_name(value.get("name")),
            "title_change" => self.title_name(value.get("title")),
            "model_change" => self.model_change(value),
            "usage" | "model_usage" if !copied => self.usage_line(value, at),
            "compaction" | "branch_summary" if !copied => self.compaction_line(value, at),
            "message" => self.message_line(value, copied, at),
            _ => {}
        }
        self.state.finish_title();
    }

    fn session_line(&mut self, value: &Value, at: Option<DateTime<chrono::FixedOffset>>) {
        if let Some(id) = value.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            self.state.record.id = format!("{AGENT}:{id}");
            self.state.session_id_set = true;
        }
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
            cwd.clone_into(&mut self.state.record.cwd);
        }
        if value
            .get("parentSession")
            .and_then(Value::as_str)
            .is_some_and(|parent| !parent.is_empty())
            && let Some(at) = at
        {
            self.state.fork_since_ms = at.timestamp_millis();
        }
        self.title_name(value.get("title"));
    }

    fn title_name(&mut self, value: Option<&Value>) {
        if let Some(title) = value.and_then(Value::as_str)
            && !title.is_empty()
        {
            self.state.named = title_line(title);
        }
    }

    fn model_change(&mut self, value: &Value) {
        if let Some(model) = value.get("modelId").and_then(Value::as_str)
            && !model.is_empty()
        {
            model.clone_into(&mut self.state.model);
        } else if let Some(model) = value.get("model").and_then(Value::as_str)
            && let Some((_provider, id)) = model.split_once('/')
            && !id.is_empty()
        {
            id.clone_into(&mut self.state.model);
        }
    }

    fn usage_line(&mut self, value: &Value, at: Option<DateTime<chrono::FixedOffset>>) {
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .map_or_else(|| self.state.model.clone(), str::to_owned);
        if !model.is_empty()
            && let Some(usage) = value.get("usage").and_then(PiUsage::of)
        {
            self.count(&model, usage.tokens(), at);
        }
    }

    fn compaction_line(&mut self, value: &Value, at: Option<DateTime<chrono::FixedOffset>>) {
        let model = self.state.model.clone();
        if !model.is_empty()
            && let Some(usage) = value.get("usage").and_then(PiUsage::of)
        {
            self.count(&model, usage.tokens(), at);
        }
    }

    fn message_line(
        &mut self,
        value: &Value,
        copied: bool,
        at: Option<DateTime<chrono::FixedOffset>>,
    ) {
        let Some(message) = value.get("message") else {
            return;
        };
        match message.get("role").and_then(Value::as_str).unwrap_or("") {
            "user" if !copied => {
                let text = message.get("content").map(content_text).unwrap_or_default();
                if self.state.first.is_empty() {
                    if let Some(prompt) = pi_prompt(&text) {
                        self.state.first = prompt;
                    } else if self.state.named.is_empty() {
                        self.state.first = untagged(&text);
                    }
                }
            }
            "assistant" if !copied => {
                let model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| !model.is_empty())
                    .map_or_else(|| self.state.model.clone(), str::to_owned);
                if !model.is_empty() {
                    self.state.model.clone_from(&model);
                }
                if let Some(usage) = message.get("usage").and_then(PiUsage::of) {
                    self.count(&model, usage.tokens(), at);
                }
            }
            _ => {}
        }
    }

    fn saw(&mut self, at: Option<DateTime<chrono::FixedOffset>>) {
        let Some(at) = at else {
            return;
        };
        let ms = at.timestamp_millis();
        if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
            self.state.record.start_ms = ms;
        }
        self.state.record.last_ms = self.state.record.last_ms.max(ms);
    }

    fn count(
        &mut self,
        model: &str,
        tokens: SessionTokens,
        at: Option<DateTime<chrono::FixedOffset>>,
    ) {
        if tokens.is_zero() {
            return;
        }
        self.state.record.add_usage(model, tokens);
        if let Some(at) = at {
            let date = at.with_timezone(&Local).format("%Y-%m-%d").to_string();
            self.state
                .record
                .add_day(SessionDay::new(date, model, tokens));
        }
    }
}

impl SessionParser for PiParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.push_line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save Raft parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn line_time(value: &Value) -> Option<DateTime<chrono::FixedOffset>> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.trim().to_owned();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text);
        }
    }
    parts.join(" ").trim().to_owned()
}

fn pi_prompt(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None;
    }
    Some(title_line(trimmed))
}

fn untagged(text: &str) -> String {
    let stripped = text
        .replace(['\t', '\n', '\r'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if stripped.is_empty() {
        return String::new();
    }
    title_line(&stripped)
}

fn title_line(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let kept: String = chars.by_ref().take(160).collect();
    if chars.next().is_some() {
        format!("{}…", kept.trim_end())
    } else {
        kept
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn parse(path: &Path) -> SessionRecord {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("name");
        let id = pi_session_id(name).expect("pi id");
        let file = SessionFile::new(id, path);
        let store = RaftStore::with_root(fixture("root"));
        let mut parser = store.parser(&file, None).expect("parser");
        let input = std::fs::File::open(path).expect("fixture");
        for line in BufReader::new(input).lines() {
            let line = line.expect("UTF-8 line");
            let value = json::from_str(&line).expect("JSON fixture");
            parser.push(&value).expect("push");
        }
        parser
            .finish()
            .expect("finish")
            .record
            .expect("session record")
    }

    #[test]
    fn raft_golden_fixture() {
        let mut record = parse(&fixture(
            "root/agents/demo/.pi-sessions/2026-10-02T13-50-18_sess-raft-001.jsonl",
        ));
        record.path = PathBuf::new();
        let actual = json::to_value(record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("raft-main.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn raft_lists_builtin_and_pi_sessions() {
        let store = RaftStore::with_root(fixture("root"));
        let files = store.files(&fixture("root")).expect("files");
        assert_eq!(files.len(), 2);
        let ids: Vec<_> = files.iter().map(|file| file.id.as_str()).collect();
        assert!(ids.contains(&"sess-raft-001"));
        assert!(ids.contains(&"sess-builtin-002"));
    }

    #[test]
    fn raft_skips_non_pi_names() {
        let store = RaftStore::with_root(fixture("root"));
        let files = store.files(&fixture("root")).expect("files");
        assert!(
            files
                .iter()
                .all(|file| !file.path.to_string_lossy().contains("notes.jsonl"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_link_walk_returns_lexical_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        symlink(fixture("root"), temp.path().join("linked")).expect("root link");
        let store = RaftStore::with_root(temp.path());
        let files = store
            .files(&temp.path().join("linked"))
            .expect("walk linked root");
        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .all(|file| file.path.starts_with(temp.path().join("linked")))
        );
    }
}
