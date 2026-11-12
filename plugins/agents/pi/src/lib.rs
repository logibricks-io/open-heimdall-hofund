//! Pi session discovery and JSONL parsing.
//!
//! The kernel supplies consented roots and depth-checked JSON values. This
//! crate owns Pi's paths, record shape, and token semantics.

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
pub const AGENT: &str = "pi";

/// Pi's JSONL session store.
#[derive(Clone, Debug)]
pub struct PiStore {
    locator: PiLocator,
}

impl PiStore {
    /// Uses Pi's environment overrides, agent home, and `settings.json`.
    #[must_use]
    pub fn new() -> Self {
        let root = std::env::var_os("PI_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map_or_else(|| home_dir().join(".pi").join("agent"), PathBuf::from);
        let session = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map(|path| expand_home(&path))
            .or_else(|| configured_session_dir(&root));
        Self::with_session_roots(root, session)
    }

    /// Uses an explicit agent root and its `settings.json` `sessionDir`.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let session = configured_session_dir(&root);
        Self::with_session_roots(root, session)
    }

    /// Uses explicit agent and optional flat session roots.
    #[must_use]
    pub fn with_session_roots(root: impl Into<PathBuf>, session: Option<PathBuf>) -> Self {
        Self {
            locator: PiLocator {
                root: root.into(),
                session,
            },
        }
    }
}

impl Default for PiStore {
    fn default() -> Self {
        Self::new()
    }
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

fn configured_session_dir(root: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(root.join("settings.json")).ok()?;
    let settings: Value = json::from_slice(&bytes).ok()?;
    settings
        .get("sessionDir")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map(|path| expand_home(&path))
}

/// Directory-only Pi locator.
#[derive(Clone, Debug)]
pub struct PiLocator {
    root: PathBuf,
    session: Option<PathBuf>,
}

impl SessionLocator for PiLocator {
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        roots.extend(self.session.clone());
        roots
    }

    fn present(&self) -> bool {
        self.root.join("sessions").is_dir()
            || self.session.as_ref().is_some_and(|path| path.is_dir())
    }
}

impl SessionStore for PiStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let base = if self.locator.root == root {
            root.join("sessions")
        } else {
            root.to_path_buf()
        };
        let mut files = Vec::new();
        if self.locator.root == root {
            for project in entries(&base)? {
                if project
                    .file_type()
                    .map_err(|_| PluginError::io("cannot list Pi sessions"))?
                    .is_dir()
                {
                    push_named(&project.path(), &mut files)?;
                }
            }
        } else {
            push_named(&base, &mut files)?;
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        files.dedup_by(|left, right| left.path == right.path);
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
            .map_err(|_| PluginError::decode("invalid Pi parser state"))?;
        Ok(Box::new(PiParser {
            state: restored.unwrap_or_else(|| PiState::new(file)),
        }))
    }

    fn resume_argv(&self, _session_id: &str, path: &Path) -> Option<Vec<String>> {
        Some(vec![
            "pi".to_owned(),
            "--session".to_owned(),
            path.to_string_lossy().into_owned(),
        ])
    }
}

fn entries(path: &Path) -> Result<Vec<std::fs::DirEntry>, PluginError> {
    let Ok(directory) = std::fs::read_dir(path) else {
        return Ok(Vec::new());
    };
    let mut entries = directory
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list Pi sessions"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn push_named(path: &Path, files: &mut Vec<SessionFile>) -> Result<(), PluginError> {
    for entry in entries(path)? {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some((_, id)) = stem.split_once('_') else {
            continue;
        };
        if !id.is_empty() {
            files.push(SessionFile::new(id.to_owned(), path));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct PiState {
    record: SessionRecord,
    named: String,
    prompt: String,
    model: String,
    since: i64,
}

impl PiState {
    fn new(file: &SessionFile) -> Self {
        let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{}", file.id), &file.path);
        record.title.clone_from(&file.id);
        Self {
            record,
            named: String::new(),
            prompt: String::new(),
            model: String::new(),
            since: 0,
        }
    }

    fn finish_title(&mut self) {
        let fallback = self
            .record
            .id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(&self.record.id)
            .to_owned();
        self.record.title = if !self.named.is_empty() {
            self.named.clone()
        } else if !self.prompt.is_empty() {
            self.prompt.clone()
        } else {
            fallback
        };
    }
}

struct PiParser {
    state: PiState,
}

impl PiParser {
    fn line(&mut self, value: &Value) {
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok());
        let ms = at.as_ref().map_or(0, DateTime::timestamp_millis);
        let copied = self.state.since != 0 && ms != 0 && ms < self.state.since;
        if !copied {
            self.saw(ms);
        }
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "session" => self.header(value, ms),
            "session_info" => {
                if let Some(name) = value.get("name").and_then(Value::as_str) {
                    self.state.named = title_line(name);
                }
            }
            "title_change" => {
                if let Some(title) = value.get("title").and_then(Value::as_str) {
                    self.state.named = title_line(title);
                }
            }
            "model_change" => self.model_change(value),
            "usage" | "model_usage" if !copied => {
                if let Some(model) = value.get("model").and_then(Value::as_str) {
                    self.count(model, value.get("usage"), at.as_ref());
                }
            }
            "compaction" | "branch_summary" if !copied && !self.state.model.is_empty() => {
                let model = self.state.model.clone();
                self.count(&model, value.get("usage"), at.as_ref());
            }
            "message" => self.message(value, copied, at.as_ref()),
            _ => {}
        }
        self.state.finish_title();
    }

    fn header(&mut self, value: &Value, ms: i64) {
        if self.state.record.start_ms != 0
            && self.state.record.id != format!("{AGENT}:{}", self.state.record.title)
        {
            return;
        }
        if let Some(id) = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.state.record.id = format!("{AGENT}:{id}");
        }
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
            cwd.clone_into(&mut self.state.record.cwd);
        }
        if let Some(parent) = value
            .get("parentSession")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.state.since = ms;
            self.state.record.forked_from = Some(format!("{AGENT}:{parent}"));
        }
        if let Some(title) = value.get("title").and_then(Value::as_str) {
            self.state.named = title_line(title);
        }
    }

    fn model_change(&mut self, value: &Value) {
        if let Some(model) = value.get("modelId").and_then(Value::as_str) {
            model.clone_into(&mut self.state.model);
        } else if let Some(model) = value
            .get("model")
            .and_then(Value::as_str)
            .and_then(|model| model.split_once('/').map(|(_, id)| id))
        {
            model.clone_into(&mut self.state.model);
        }
    }

    fn message(&mut self, value: &Value, copied: bool, at: Option<&DateTime<chrono::FixedOffset>>) {
        let Some(message) = value.get("message") else {
            return;
        };
        match message.get("role").and_then(Value::as_str).unwrap_or("") {
            "user" => {
                if copied || !self.state.prompt.is_empty() {
                    return;
                }
                let text = message.get("content").map(content_text).unwrap_or_default();
                let trimmed = text.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('<') {
                    self.state.prompt = title_line(trimmed);
                }
            }
            "assistant" => {
                if let Some(model) = message.get("model").and_then(Value::as_str) {
                    model.clone_into(&mut self.state.model);
                }
                if copied {
                    return;
                }
                if let Some(model) = message.get("model").and_then(Value::as_str) {
                    self.count(model, message.get("usage"), at);
                }
            }
            _ => {
                if !copied && !self.state.model.is_empty() {
                    let model = self.state.model.clone();
                    self.count(&model, message.get("usage"), at);
                }
            }
        }
    }

    fn count(
        &mut self,
        model: &str,
        usage: Option<&Value>,
        at: Option<&DateTime<chrono::FixedOffset>>,
    ) {
        let Some(usage) = usage else {
            return;
        };
        let tokens = SessionTokens::new(
            counter(usage, "input"),
            counter(usage, "output"),
            counter(usage, "cacheRead"),
            counter(usage, "cacheWrite"),
        );
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

    fn saw(&mut self, ms: i64) {
        if ms <= 0 {
            return;
        }
        if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
            self.state.record.start_ms = ms;
        }
        self.state.record.last_ms = self.state.record.last_ms.max(ms);
    }
}

impl SessionParser for PiParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save Pi parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

fn counter(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
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
