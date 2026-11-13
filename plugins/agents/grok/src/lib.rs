//! Grok Build session discovery and `updates.jsonl` parsing.
//!
//! The kernel supplies a consented root and depth-checked JSON values. This
//! crate owns only Grok Build's paths and record format.

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
pub const AGENT: &str = "grok";

/// Grok Build's session store under `GROK_HOME` or `~/.grok`.
#[derive(Clone, Debug)]
pub struct GrokStore {
    locator: GrokLocator,
}

impl GrokStore {
    /// Uses `GROK_HOME` or `~/.grok`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_root(grok_home())
    }

    /// Uses an explicit root for embedding and tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            locator: GrokLocator { root: root.into() },
        }
    }
}

impl Default for GrokStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only Grok locator.
#[derive(Clone, Debug)]
pub struct GrokLocator {
    root: PathBuf,
}

impl SessionLocator for GrokLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("sessions").is_dir()
    }
}

impl SessionStore for GrokStore {
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
        collect_updates(&sessions, &mut paths)?;
        let mut files = Vec::new();
        for path in paths {
            let Some(id) = path
                .parent()
                .and_then(|dir| dir.file_name())
                .and_then(|n| n.to_str())
            else {
                continue;
            };
            if !safe_id(id) {
                continue;
            }
            let summary = path
                .parent()
                .map(|dir| dir.join("summary.json"))
                .filter(|p| p.is_file());
            let related = if let Some(summary) = summary {
                vec![summary]
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
        let summary_path = file
            .related
            .iter()
            .find(|path| path.file_name().and_then(|n| n.to_str()) == Some("summary.json"))
            .cloned()
            .or_else(|| {
                file.path
                    .parent()
                    .map(|dir| dir.join("summary.json"))
                    .filter(|p| p.is_file())
            });
        let summary = summary_path.as_deref().and_then(read_grok_summary);
        let parent_map = file
            .path
            .parent()
            .and_then(|dir| dir.parent())
            .and_then(|cwd| cwd.parent())
            .map(|sessions| subagent_parents(sessions).unwrap_or_default())
            .unwrap_or_default();
        let restored = state
            .map(json::from_str::<GrokState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid Grok parser state"))?;
        Ok(Box::new(GrokParser {
            state: restored.unwrap_or_else(|| GrokState::new(file, summary, &parent_map)),
        }))
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id.strip_prefix(&format!("{AGENT}:"))?;
        if !safe_id(id) {
            return None;
        }
        Some(vec![
            "grok".to_owned(),
            "--resume".to_owned(),
            id.to_owned(),
        ])
    }
}

fn grok_home() -> PathBuf {
    if let Ok(value) = std::env::var("GROK_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return expand_home(trimmed);
        }
    }
    user_home().join(".grok")
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

fn collect_updates(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), PluginError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|_| PluginError::io("cannot list Grok sessions"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list Grok sessions"))?;
    for entry in entries {
        let path = entry.path();
        if entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list Grok sessions"))?
            .is_dir()
        {
            collect_updates(&path, out)?;
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "updates.jsonl")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn subagent_parents(
    sessions: &Path,
) -> Result<std::collections::BTreeMap<String, String>, PluginError> {
    let mut parent_of = std::collections::BTreeMap::new();
    walk_subagent_links(sessions, sessions, &mut parent_of)?;
    Ok(parent_of)
}

fn walk_subagent_links(
    real: &Path,
    lexical: &Path,
    parent_of: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), PluginError> {
    let entries = std::fs::read_dir(real)
        .map_err(|_| PluginError::io("cannot list Grok subagents"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list Grok subagents"))?;
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let lexical_path = lexical.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list Grok subagents"))?;
        if file_type.is_dir() {
            if name == "subagents" {
                let parent = lexical
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned();
                for child in std::fs::read_dir(&path)
                    .map_err(|_| PluginError::io("cannot list Grok subagents"))?
                    .flatten()
                {
                    let child_name = child.file_name();
                    let Some(child_name) = child_name.to_str() else {
                        continue;
                    };
                    let Some(child_id) = child_name.strip_suffix(".json") else {
                        continue;
                    };
                    parent_of.insert(child_id.to_owned(), parent.clone());
                }
            } else {
                walk_subagent_links(&path, &lexical_path, parent_of)?;
            }
        }
    }
    Ok(())
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Clone, Debug, Default)]
struct GrokSummary {
    id: String,
    cwd: String,
    created: Option<DateTime<Utc>>,
    generated_title: String,
    session_summary: String,
    current_model: String,
    parent: String,
    kind: String,
}

fn read_grok_summary(path: &Path) -> Option<GrokSummary> {
    let bytes = std::fs::read(path).ok()?;
    let value = json::from_slice::<Value>(&bytes).ok()?;
    let info = value.get("info")?;
    let created = value
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.with_timezone(&Utc));
    Some(GrokSummary {
        id: info
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        cwd: info
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        created,
        generated_title: value
            .get("generated_title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        session_summary: value
            .get("session_summary")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        current_model: value
            .get("current_model_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        parent: value
            .get("parent_session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        kind: value
            .get("session_kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct GrokUsage {
    #[serde(rename = "inputTokens", default)]
    input: u64,
    #[serde(rename = "outputTokens", default)]
    output: u64,
    #[serde(rename = "cachedReadTokens", default)]
    cache_read: u64,
    #[serde(rename = "cacheCreationTokens", default)]
    cache_write: u64,
}

impl GrokUsage {
    fn tokens(self) -> SessionTokens {
        let input = self
            .input
            .saturating_sub(self.cache_read)
            .saturating_sub(self.cache_write);
        SessionTokens::new(input, self.output, self.cache_read, self.cache_write)
    }

    fn of(value: &Value) -> Option<Self> {
        serde::Deserialize::deserialize(value).ok()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct GrokState {
    record: SessionRecord,
    model: String,
    created_ms: i64,
    since_ms: i64,
    prompt: String,
    in_prompt: bool,
    title_from_prompt: String,
    named: String,
}

impl GrokState {
    fn new(
        file: &SessionFile,
        summary: Option<GrokSummary>,
        parent_of: &std::collections::BTreeMap<String, String>,
    ) -> Self {
        let id = format!("{AGENT}:{}", file.id);
        let mut record = SessionRecord::new(AGENT, &id, &file.path);
        let mut model = String::new();
        let mut created_ms = 0_i64;
        let mut since_ms = 0_i64;
        let mut named = String::new();
        if let Some(summary) = summary {
            if !summary.id.is_empty() {
                record.id = format!("{AGENT}:{}", summary.id);
            }
            summary.cwd.clone_into(&mut record.cwd);
            named = title_line(&summary.generated_title);
            if named.is_empty() {
                named = title_line(&summary.session_summary);
            }
            summary.current_model.clone_into(&mut model);
            if let Some(created) = summary.created {
                created_ms = created.timestamp_millis();
                record.start_ms = created_ms;
                record.last_ms = created_ms;
                if !summary.parent.is_empty() {
                    since_ms = created.timestamp();
                }
            }
            if summary.kind.starts_with("subagent") {
                let parent = if summary.parent.is_empty() {
                    parent_of.get(&file.id).cloned().unwrap_or_default()
                } else {
                    summary.parent.clone()
                };
                if !parent.is_empty() && parent != file.id && safe_id(&parent) {
                    record = record.with_parent(format!("{AGENT}:{parent}"));
                }
            }
        }
        Self {
            record,
            model,
            created_ms,
            since_ms,
            prompt: String::new(),
            in_prompt: false,
            title_from_prompt: String::new(),
            named,
        }
    }

    fn finish_title(&mut self) {
        let fallback = self
            .record
            .id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(&self.record.id)
            .to_owned();
        let mut title = if !self.named.is_empty() {
            self.named.clone()
        } else if self.in_prompt {
            title_line(&self.prompt)
        } else if !self.title_from_prompt.is_empty() {
            self.title_from_prompt.clone()
        } else {
            fallback
        };
        if title.starts_with('<') {
            title = untagged_line(&title);
        }
        self.record.title = title;
    }
}

struct GrokParser {
    state: GrokState,
}

impl GrokParser {
    fn line(&mut self, value: &Value) {
        let Some(update) = value.get("params").and_then(|params| params.get("update")) else {
            return;
        };
        let session_update = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if session_update == "hook_execution" {
            return;
        }
        let at = num_timestamp(value);
        if self.state.since_ms > 0 {
            if let Some(at) = at {
                if at.timestamp() < self.state.since_ms {
                    return;
                }
            } else {
                return;
            }
        }
        if let Some(at) = at {
            self.saw(at);
        }
        if session_update == "tool_call" || session_update == "tool_call_update" {
            return;
        }
        if session_update == "user_message_chunk" {
            let content = update.get("content");
            if content.and_then(|c| c.get("type")).and_then(Value::as_str) != Some("text") {
                return;
            }
            let text = content
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let user_prompt = self.state.record.main && self.state.named.is_empty();
            if self.state.in_prompt && !user_prompt {
                self.state.title_from_prompt = title_line(&self.state.prompt);
                self.state.prompt.clear();
                self.state.in_prompt = false;
            }
            if user_prompt {
                self.state.prompt.push_str(text);
                self.state.in_prompt = true;
            }
            return;
        }
        if session_update != "turn_completed" {
            return;
        }
        let Some(usage) = update.get("usage") else {
            return;
        };
        let date = at
            .map(|at| at.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        if let Some(map) = usage.get("modelUsage").and_then(Value::as_object) {
            for (model, raw) in map {
                if let Some(parsed) = GrokUsage::of(raw) {
                    let tokens = parsed.tokens();
                    if !tokens.is_zero() {
                        self.state.record.add_usage(model, tokens);
                        if !date.is_empty() {
                            self.state.record.add_day(SessionDay::new(
                                date.clone(),
                                model.clone(),
                                tokens,
                            ));
                        }
                    }
                }
            }
        } else if let Some(parsed) = GrokUsage::of(usage) {
            let tokens = parsed.tokens();
            let model = if self.state.model.is_empty() {
                "grok".to_owned()
            } else {
                self.state.model.clone()
            };
            if !tokens.is_zero() {
                self.state.record.add_usage(&model, tokens);
                if !date.is_empty() {
                    self.state
                        .record
                        .add_day(SessionDay::new(date, model, tokens));
                }
            }
        }
    }

    fn saw(&mut self, at: DateTime<Utc>) {
        let ms = at.timestamp_millis();
        if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
            self.state.record.start_ms = ms;
        }
        self.state.record.last_ms = self.state.record.last_ms.max(ms);
    }
}

impl SessionParser for GrokParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        if self.state.in_prompt {
            self.state.title_from_prompt = title_line(&self.state.prompt);
        }
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save Grok parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn num_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let stamp = value.get("timestamp")?;
    if let Some(secs) = stamp.as_i64().filter(|n| *n > 0) {
        if secs > 100_000_000_000 {
            return Utc.timestamp_millis_opt(secs).single();
        }
        return Utc.timestamp_opt(secs, 0).single();
    }
    if let Some(secs) = stamp.as_u64().filter(|n| *n > 0) {
        let secs = i64::try_from(secs).ok()?;
        if secs > 100_000_000_000 {
            return Utc.timestamp_millis_opt(secs).single();
        }
        return Utc.timestamp_opt(secs, 0).single();
    }
    None
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
            .join("%2Fwork%2Fgrok")
            .join(id)
            .join("updates.jsonl");
        let summary = path.parent().map(|p| p.join("summary.json"));
        let related = summary
            .filter(|p| p.is_file())
            .map(|p| vec![p])
            .unwrap_or_default();
        let file = SessionFile::new(id, &path).with_related(related);
        let store = GrokStore::with_root(root);
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
    fn grok_golden_main_session() {
        let root = fixture("root");
        let mut record = parse_fixture("0190aaaa-0000-7000-8000-00000000000a", &root);
        record.path = PathBuf::new();
        let actual = json::to_value(&record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("grok-main.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn grok_resume_argv() {
        let store = GrokStore::new();
        assert_eq!(
            store.resume_argv("grok:abc-def_01", Path::new("/any")),
            Some(vec![
                "grok".to_owned(),
                "--resume".to_owned(),
                "abc-def_01".to_owned()
            ])
        );
    }
}
