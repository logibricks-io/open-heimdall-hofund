//! omp session discovery and Pi-shaped JSONL parsing.
//!
//! The kernel supplies consented roots and depth-checked JSON values. This
//! crate owns omp's roots, artifact grouping, and token semantics.

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
    SessionRecord, SessionStore, SessionTokens, SessionUsageEvent,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "omp";

/// omp's JSONL session store over default, profile, and XDG roots.
#[derive(Clone, Debug)]
pub struct OmpStore {
    locator: OmpLocator,
}

impl OmpStore {
    /// Uses the default home, profiles, and non-Windows XDG data root.
    #[must_use]
    pub fn new() -> Self {
        let root = home_dir().join(".omp").join("agent");
        let roots = session_roots(&root);
        #[cfg(not(windows))]
        let roots = {
            let mut roots = roots;
            if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty())
            {
                roots.push(PathBuf::from(data).join("omp").join("sessions"));
            }
            roots
        };
        Self::with_session_roots(roots)
    }

    /// Uses an explicit agent home and its sibling profiles.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::with_session_roots(session_roots(&root.into()))
    }

    /// Uses explicit session roots.
    #[must_use]
    pub fn with_session_roots(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            locator: OmpLocator {
                roots: roots.into_iter().collect(),
            },
        }
    }
}

impl Default for OmpStore {
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

fn session_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![root.join("sessions")];
    let profiles = root
        .parent()
        .map_or_else(|| PathBuf::from("profiles"), |home| home.join("profiles"));
    for profile in entries(&profiles).into_iter().flatten() {
        let sessions = profile.path().join("agent").join("sessions");
        if sessions.is_dir() {
            roots.push(sessions);
        }
    }
    roots
}

/// Directory-only omp locator.
#[derive(Clone, Debug)]
pub struct OmpLocator {
    roots: Vec<PathBuf>,
}

impl SessionLocator for OmpLocator {
    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn present(&self) -> bool {
        self.roots.iter().any(|root| root.is_dir())
    }
}

impl SessionStore for OmpStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let mut files = Vec::new();
        for project in entries(root)? {
            if !project
                .file_type()
                .map_err(|_| PluginError::io("cannot list omp sessions"))?
                .is_dir()
            {
                continue;
            }
            for entry in entries(&project.path())? {
                let path = entry.path();
                let Some(id) = named_id(&path) else {
                    continue;
                };
                let mut related = Vec::new();
                artifacts(&path.with_extension(""), &mut related)?;
                related.sort();
                related.dedup();
                files.push(SessionFile::new(id, path).with_related(related));
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
            .map(json::from_str::<OmpState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid omp parser state"))?;
        Ok(Box::new(OmpParser {
            state: restored.unwrap_or_else(|| OmpState::new(file)),
        }))
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(session_id);
        Some(vec!["omp".to_owned(), "--resume".to_owned(), id.to_owned()])
    }
}

fn entries(path: &Path) -> Result<Vec<std::fs::DirEntry>, PluginError> {
    let Ok(directory) = std::fs::read_dir(path) else {
        return Ok(Vec::new());
    };
    let mut entries = directory
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list omp sessions"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn named_id(path: &Path) -> Option<String> {
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    let (_, id) = stem.split_once('_')?;
    (!id.is_empty()).then(|| id.to_owned())
}

fn artifacts(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), PluginError> {
    for entry in entries(path)? {
        let kind = entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list omp artifacts"))?;
        if kind.is_dir() {
            if entry.file_name() != "btw-history" {
                artifacts(&entry.path(), out)?;
            }
        } else if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            out.push(entry.path());
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct OmpState {
    record: SessionRecord,
    named: String,
    prompt: String,
    model: String,
    since: i64,
    #[serde(default)]
    header_seen: bool,
    #[serde(default)]
    checkpoint: SessionTokens,
}

impl OmpState {
    fn new(file: &SessionFile) -> Self {
        let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{}", file.id), &file.path);
        record.title.clone_from(&file.id);
        record.subagents = file
            .related
            .iter()
            .filter_map(|path| path.file_stem().and_then(|stem| stem.to_str()))
            .map(|id| format!("{AGENT}:{id}"))
            .collect();
        Self {
            record,
            named: String::new(),
            prompt: String::new(),
            model: String::new(),
            since: 0,
            header_seen: false,
            checkpoint: SessionTokens::default(),
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

struct OmpParser {
    state: OmpState,
}

impl OmpParser {
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
            "usage" | "model_usage" => {
                if let Some(model) = value.get("model").and_then(Value::as_str) {
                    self.count(model, value.get("usage"), at.as_ref(), copied);
                } else if copied {
                    self.count("", value.get("usage"), at.as_ref(), copied);
                }
            }
            "compaction" | "branch_summary" => {
                if copied || !self.state.model.is_empty() {
                    let model = self.state.model.clone();
                    self.count(&model, value.get("usage"), at.as_ref(), copied);
                }
            }
            "message" => self.message(value, copied, at.as_ref()),
            _ => {}
        }
        self.state.finish_title();
    }

    fn header(&mut self, value: &Value, ms: i64) {
        if self.state.header_seen {
            return;
        }
        self.state.header_seen = true;
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
            self.state.record.fork_usage_filtered = true;
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
                    self.count(model, message.get("usage"), at, copied);
                } else if copied {
                    self.count("", message.get("usage"), at, copied);
                }
            }
            _ => {
                if copied || !self.state.model.is_empty() {
                    let model = self.state.model.clone();
                    self.count(&model, message.get("usage"), at, copied);
                }
            }
        }
    }

    fn count(
        &mut self,
        model: &str,
        usage: Option<&Value>,
        at: Option<&DateTime<chrono::FixedOffset>>,
        copied: bool,
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
        self.state.checkpoint.add(tokens);
        if copied {
            return;
        }
        let date = at.map_or_else(String::new, |at| {
            at.with_timezone(&Local).format("%Y-%m-%d").to_string()
        });
        self.state.record.usage.push(SessionUsageEvent::new(
            self.state.checkpoint,
            tokens,
            model,
            &date,
        ));
        self.state.record.add_usage(model, tokens);
        if !date.is_empty() {
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

impl SessionParser for OmpParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save omp parser state"))?;
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
