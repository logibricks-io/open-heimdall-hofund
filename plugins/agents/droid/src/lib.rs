//! Droid (Factory) session discovery and JSONL plus settings parsing.
//!
//! The kernel supplies a consented root and depth-checked JSON values. This
//! crate owns only Droid's paths and record format.

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
pub const AGENT: &str = "droid";

/// Droid's session store under `~/.factory` or `$FACTORY_HOME_OVERRIDE/.factory`.
#[derive(Clone, Debug)]
pub struct DroidStore {
    locator: DroidLocator,
    factory_root: PathBuf,
}

impl DroidStore {
    /// Uses `$FACTORY_HOME_OVERRIDE` or the user home, then `.factory`.
    #[must_use]
    pub fn new() -> Self {
        let (factory_root, _) = factory_paths();
        Self::with_root(factory_root)
    }

    /// Uses an explicit `.factory` root for embedding and tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let factory_root = root.into();
        Self {
            locator: DroidLocator {
                root: factory_root.clone(),
            },
            factory_root,
        }
    }
}

impl Default for DroidStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only Droid locator.
#[derive(Clone, Debug)]
pub struct DroidLocator {
    root: PathBuf,
}

impl SessionLocator for DroidLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("sessions").is_dir()
    }
}

impl SessionStore for DroidStore {
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
        collect_jsonl(&sessions, &mut paths)?;
        let mut files = Vec::new();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(id) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if !safe_id(id) {
                continue;
            }
            let settings = settings_path(&path);
            let related = if settings.is_file() {
                vec![settings]
            } else {
                Vec::new()
            };
            files.push(SessionFile::new(id, path.clone()).with_related(related));
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
            .map(json::from_str::<DroidState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid Droid parser state"))?;
        Ok(Box::new(DroidParser {
            factory_root: self.factory_root.clone(),
            state: restored.unwrap_or_else(|| DroidState::new(file)),
        }))
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id.strip_prefix(&format!("{AGENT}:"))?;
        if !safe_id(id) {
            return None;
        }
        Some(vec![
            "droid".to_owned(),
            "--resume".to_owned(),
            id.to_owned(),
        ])
    }
}

fn factory_paths() -> (PathBuf, PathBuf) {
    let home = user_home();
    let factory = if let Ok(value) = std::env::var("FACTORY_HOME_OVERRIDE") {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            home.join(".factory")
        } else {
            expand_home(trimmed).join(".factory")
        }
    } else {
        home.join(".factory")
    };
    (factory, home)
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

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), PluginError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|_| PluginError::io("cannot list Droid sessions"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list Droid sessions"))?;
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list Droid sessions"))?;
        if file_type.is_dir() {
            collect_jsonl(&path, out)?;
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn settings_path(transcript: &Path) -> PathBuf {
    let Some(name) = transcript.file_name().and_then(|name| name.to_str()) else {
        return transcript.with_extension("settings.json");
    };
    let Some(id) = name.strip_suffix(".jsonl") else {
        return transcript.with_extension("settings.json");
    };
    transcript.parent().map_or_else(
        || PathBuf::from(format!("{id}.settings.json")),
        |parent| parent.join(format!("{id}.settings.json")),
    )
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct DroidUsage {
    #[serde(rename = "inputTokens", default)]
    input: u64,
    #[serde(rename = "outputTokens", default)]
    output: u64,
    #[serde(rename = "cacheReadTokens", default)]
    cache_read: u64,
    #[serde(rename = "cacheCreationTokens", default)]
    cache_write: u64,
}

impl DroidUsage {
    fn tokens(self) -> SessionTokens {
        SessionTokens::new(self.input, self.output, self.cache_read, self.cache_write)
    }

    fn of(value: &Value) -> Option<Self> {
        serde::Deserialize::deserialize(value).ok()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct SettingsCount {
    date: String,
    model: String,
    tokens: SessionTokens,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct DroidState {
    record: SessionRecord,
    named: String,
    prompt: String,
    started: bool,
    fork_upto: String,
    skip_fork: bool,
    settings: Option<SettingsCount>,
}

impl DroidState {
    fn new(file: &SessionFile) -> Self {
        let id = format!("{AGENT}:{}", file.id);
        Self {
            record: SessionRecord::new(AGENT, &id, &file.path),
            named: String::new(),
            prompt: String::new(),
            started: false,
            fork_upto: String::new(),
            skip_fork: false,
            settings: None,
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

struct DroidParser {
    factory_root: PathBuf,
    state: DroidState,
}

impl DroidParser {
    fn line(&mut self, value: &Value) {
        if !self.state.started {
            self.state.started = true;
            if value.get("type").and_then(Value::as_str) != Some("session_start") {
                return;
            }
            let id = value.get("id").and_then(Value::as_str).unwrap_or("");
            if !id.is_empty() {
                self.state.record.id = format!("{AGENT}:{id}");
            }
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                cwd.clone_into(&mut self.state.record.cwd);
            }
            if let Some(last) = value.get("lastCwd").and_then(Value::as_str)
                && !last.is_empty()
            {
                last.clone_into(&mut self.state.record.cwd);
            }
            if let Some(title) = value.get("title").and_then(Value::as_str) {
                let titled = title_line(title);
                if titled != "New Session" {
                    self.state.named = titled;
                }
            }
            if let Some(forked) = value.get("forkedAtMessageId").and_then(Value::as_str) {
                forked.clone_into(&mut self.state.fork_upto);
                self.state.skip_fork = !forked.is_empty();
            }
            if let Some(calling) = value.get("callingSessionId").and_then(Value::as_str)
                && !calling.is_empty()
                && calling != id
                && safe_id(calling)
            {
                self.state.record = self
                    .state
                    .record
                    .clone()
                    .with_parent(format!("{AGENT}:{calling}"));
            }
            return;
        }
        if value.get("type").and_then(Value::as_str) != Some("message") {
            return;
        }
        let msg_id = value.get("id").and_then(Value::as_str).unwrap_or("");
        if self.state.skip_fork {
            if msg_id == self.state.fork_upto {
                self.state.skip_fork = false;
            }
            return;
        }
        let at = line_time(value);
        self.saw(at);
        let Some(message) = value.get("message") else {
            return;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "user" => self.user(message, at),
            "assistant" if self.state.record.main => {}
            _ => {}
        }
    }

    fn saw(&mut self, at: Option<DateTime<Utc>>) {
        let Some(at) = at else {
            return;
        };
        let ms = at.timestamp_millis();
        if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
            self.state.record.start_ms = ms;
        }
        self.state.record.last_ms = self.state.record.last_ms.max(ms);
    }

    fn user(&mut self, message: &Value, at: Option<DateTime<Utc>>) {
        let text = message_text(message.get("content").unwrap_or(&Value::Null));
        if text.trim().is_empty() || !self.state.record.main {
            return;
        }
        if text.trim().starts_with('<') {
            if self.state.prompt.is_empty() {
                self.state.prompt = untagged_line(&text);
            }
            return;
        }
        if let Some(at) = at {
            let _ = at;
        }
        if self.state.prompt.is_empty() {
            self.state.prompt = title_line(&text);
        }
    }

    fn apply_settings(&mut self, path: &Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(value) = json::from_slice::<Value>(&bytes) else {
            return;
        };
        let Some(usage) = value.get("tokenUsage") else {
            return;
        };
        let Some(parsed) = DroidUsage::of(usage) else {
            return;
        };
        let tokens = parsed.tokens();
        if tokens.is_zero() {
            return;
        }
        let model = value.get("model").and_then(Value::as_str).map_or_else(
            || "droid".to_owned(),
            |model| droid_model_name(model, &self.factory_root),
        );
        let when = if self.state.record.last_ms > 0 {
            Utc.timestamp_millis_opt(self.state.record.last_ms)
                .single()
                .unwrap_or_else(Utc::now)
        } else {
            Utc::now()
        };
        let date = when.with_timezone(&Local).format("%Y-%m-%d").to_string();
        if let Some(previous) = self.state.settings.take() {
            subtract_usage(&mut self.state.record, &previous);
        }
        self.state.record.add_usage(&model, tokens);
        self.state
            .record
            .add_day(SessionDay::new(date.clone(), model.clone(), tokens));
        self.state.settings = Some(SettingsCount {
            date,
            model,
            tokens,
        });
    }
}

impl SessionParser for DroidParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        if value.get("tokenUsage").is_some() && value.get("model").is_some() {
            return Ok(());
        }
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let settings = settings_path(&self.state.record.path);
        self.apply_settings(&settings);
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save Droid parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn subtract_usage(record: &mut SessionRecord, counted: &SettingsCount) {
    if let Some(tokens) = record.models.get_mut(&counted.model) {
        tokens.input = tokens.input.saturating_sub(counted.tokens.input);
        tokens.output = tokens.output.saturating_sub(counted.tokens.output);
        tokens.cache_read = tokens.cache_read.saturating_sub(counted.tokens.cache_read);
        tokens.cache_write = tokens
            .cache_write
            .saturating_sub(counted.tokens.cache_write);
        if tokens.is_zero() {
            record.models.remove(&counted.model);
        }
    }
    record.days.retain(|day| {
        !(day.date == counted.date
            && day.model == counted.model
            && day.tokens.input <= counted.tokens.input
            && day.tokens.output <= counted.tokens.output)
    });
}

fn line_time(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("timestamp")
        .and_then(|stamp| {
            stamp
                .as_str()
                .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
                .map(|at| at.with_timezone(&Utc))
                .or_else(|| {
                    stamp
                        .as_i64()
                        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
                })
        })
        .or_else(|| {
            value.get("message").and_then(|message| {
                message
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
            })
        })
}

fn message_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            return String::new();
        }
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            parts.push(text);
        }
    }
    parts.join("\n")
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

fn untagged_line(text: &str) -> String {
    let mut out = String::new();
    let mut depth: i32 = 0;
    let mut chunk = String::new();
    for ch in text.chars() {
        match ch {
            '<' => {
                if depth == 0 && !chunk.trim().is_empty() {
                    out.push_str(chunk.trim());
                    chunk.clear();
                }
                depth += 1;
            }
            '>' => {
                depth = depth.saturating_sub(1);
            }
            _ if depth == 0 => chunk.push(ch),
            _ => {}
        }
    }
    if !chunk.trim().is_empty() {
        out.push_str(chunk.trim());
    }
    title_line(&out)
}

fn droid_model_name(model: &str, factory_root: &Path) -> String {
    if model.is_empty() {
        return "droid".to_owned();
    }
    if !model.starts_with("custom:") {
        return model.to_owned();
    }
    let settings = factory_root.join("settings.json");
    if let Ok(bytes) = std::fs::read(settings)
        && let Ok(value) = json::from_slice::<Value>(&bytes)
        && let Some(models) = value.get("customModels").and_then(Value::as_array)
    {
        for entry in models {
            if entry.get("id").and_then(Value::as_str) == Some(model)
                && let Some(resolved) = entry.get("model").and_then(Value::as_str)
                && !resolved.is_empty()
            {
                return resolved.to_owned();
            }
        }
    }
    let tail = model.strip_prefix("custom:").unwrap_or(model);
    strip_droid_slot_suffix(tail)
}

fn strip_droid_slot_suffix(name: &str) -> String {
    if let Some(open) = name.rfind("-[") {
        let prefix = &name[..open];
        if name[open..].contains(']') {
            return prefix.to_owned();
        }
    }
    if let Some((base, suffix)) = name.rsplit_once('-')
        && suffix.chars().all(|ch| ch.is_ascii_digit())
    {
        return base.to_owned();
    }
    name.to_owned()
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

    fn parse_fixture(id: &str, root: &Path) -> SessionRecord {
        let path = root
            .join("sessions")
            .join("-work-droid")
            .join(format!("{id}.jsonl"));
        let file = SessionFile::new(id, &path);
        let store = DroidStore::with_root(root);
        let mut parser = store.parser(&file, None).expect("parser");
        let input = std::fs::File::open(&path).expect("fixture");
        for line in BufReader::new(input).lines() {
            let line = line.expect("UTF-8 line");
            let value = json::from_str(&line).expect("JSON fixture");
            parser.push(&value).expect("push");
        }
        parser.finish().expect("finish").record.expect("record")
    }

    #[test]
    fn droid_golden_main_session() {
        let root = fixture("root");
        let mut record = parse_fixture("d0000000-0000-4000-8000-00000000000a", &root);
        record.path = PathBuf::new();
        let actual = json::to_value(&record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("droid-main.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn later_child_messages_advance_last_ms() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("child.jsonl");
        let file = SessionFile::new("child", &path);
        let store = DroidStore::with_root(temp.path());
        let mut parser = store.parser(&file, None).expect("parser");
        for value in [
            json::json!({
                "type": "session_start",
                "id": "child",
                "callingSessionId": "parent"
            }),
            json::json!({
                "type": "message",
                "timestamp": 1_000,
                "message": {"role": "assistant", "content": "first"}
            }),
            json::json!({
                "type": "message",
                "timestamp": 2_000,
                "message": {"role": "assistant", "content": "later"}
            }),
        ] {
            parser.push(&value).expect("push");
        }
        let record = parser.finish().expect("finish").record.expect("record");
        assert!(!record.main);
        assert_eq!(record.parent_id.as_deref(), Some("droid:parent"));
        assert_eq!(record.start_ms, 1_000);
        assert_eq!(record.last_ms, 2_000);
    }

    #[test]
    fn droid_resume_argv() {
        let store = DroidStore::new();
        assert_eq!(
            store.resume_argv("droid:abc-def_01", Path::new("/any")),
            Some(vec![
                "droid".to_owned(),
                "--resume".to_owned(),
                "abc-def_01".to_owned()
            ])
        );
        assert!(
            store
                .resume_argv("droid:../bad", Path::new("/any"))
                .is_none()
        );
    }

    #[test]
    fn droid_model_name_resolves_custom() {
        let root = fixture("root");
        assert_eq!(
            droid_model_name("custom:hofund/anthropic/claude-sonnet-4-5", &root),
            "anthropic/claude-sonnet-4-5"
        );
    }
}
