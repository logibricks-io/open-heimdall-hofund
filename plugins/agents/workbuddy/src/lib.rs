//! WorkBuddy-family session discovery and `OpenAI` Agents SDK JSONL parsing.
//!
//! The kernel supplies consented roots and bounded JSON values. This crate owns
//! `WorkBuddy`, `WorkBuddy` AI, and `CodeBuddy` Code layouts (including `sameFolder`
//! dedup).

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};
use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionLocator, SessionParse, SessionParser,
    SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use regex::Regex;
use serde::{Deserialize, Serialize};

static BUDDY_SCENE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

/// The `WorkBuddy` (China build) session agent id.
pub const AGENT: &str = "workbuddy";

/// `WorkBuddy` session store.
#[derive(Clone, Debug)]
pub struct WorkBuddyStore {
    locator: BuddyLocator,
}

impl WorkBuddyStore {
    /// Uses `$WORKBUDDY_CONFIG_DIR` or `~/.workbuddy`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_root(workbuddy_home())
    }

    /// Uses an explicit config root for tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            locator: BuddyLocator { root: root.into() },
        }
    }
}

impl Default for WorkBuddyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for WorkBuddyStore {
    fn agent(&self) -> &str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        list_buddy_files(AGENT, root)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        buddy_parser(AGENT, file, state)
    }
}

/// `WorkBuddy` AI (international build) session store.
#[derive(Clone, Debug)]
pub struct WorkBuddyAiStore {
    locator: BuddyLocator,
    workbuddy_root: PathBuf,
}

impl WorkBuddyAiStore {
    /// Uses `~/.workbuddy-ai` unless that folder is the same inode as `WorkBuddy`'s.
    #[must_use]
    pub fn new() -> Self {
        Self::with_roots(workbuddy_ai_home(), workbuddy_home())
    }

    /// Uses an explicit config root for tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::with_roots(root, workbuddy_home())
    }

    /// Pairs an AI tree with the `WorkBuddy` tree used for same-folder dedup.
    #[must_use]
    pub fn with_roots(ai_root: impl Into<PathBuf>, workbuddy_root: impl Into<PathBuf>) -> Self {
        Self {
            locator: BuddyLocator {
                root: ai_root.into(),
            },
            workbuddy_root: workbuddy_root.into(),
        }
    }
}

impl Default for WorkBuddyAiStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for WorkBuddyAiStore {
    fn agent(&self) -> &'static str {
        "workbuddy-ai"
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        if same_folder(&self.locator.root, &self.workbuddy_root) {
            return Ok(Vec::new());
        }
        list_buddy_files("workbuddy-ai", root)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        buddy_parser("workbuddy-ai", file, state)
    }
}

/// `CodeBuddy` Code session store.
#[derive(Clone, Debug)]
pub struct CodeBuddyStore {
    locator: BuddyLocator,
    workbuddy_root: PathBuf,
}

impl CodeBuddyStore {
    /// Uses `$CODEBUDDY_CONFIG_DIR` or `~/.codebuddy`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_roots(codebuddy_home(), workbuddy_home())
    }

    /// Uses an explicit config root for tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::with_roots(root, workbuddy_home())
    }

    /// Pairs a `CodeBuddy` tree with the `WorkBuddy` tree used for dedup.
    #[must_use]
    pub fn with_roots(code_root: impl Into<PathBuf>, workbuddy_root: impl Into<PathBuf>) -> Self {
        Self {
            locator: BuddyLocator {
                root: code_root.into(),
            },
            workbuddy_root: workbuddy_root.into(),
        }
    }
}

impl Default for CodeBuddyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for CodeBuddyStore {
    fn agent(&self) -> &'static str {
        "codebuddy"
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        if same_folder(&self.locator.root, &self.workbuddy_root) {
            return Ok(Vec::new());
        }
        list_buddy_files("codebuddy", root)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        buddy_parser("codebuddy", file, state)
    }
}

/// Directory-only locator for a buddy config tree.
#[derive(Clone, Debug)]
pub struct BuddyLocator {
    root: PathBuf,
}

impl SessionLocator for BuddyLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("projects").is_dir()
    }
}

fn workbuddy_home() -> PathBuf {
    std::env::var_os("WORKBUDDY_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| user_home().join(".workbuddy"), PathBuf::from)
}

fn workbuddy_ai_home() -> PathBuf {
    user_home().join(".workbuddy-ai")
}

fn codebuddy_home() -> PathBuf {
    std::env::var_os("CODEBUDDY_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| user_home().join(".codebuddy"), PathBuf::from)
}

fn user_home() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from(".workbuddy"), PathBuf::from)
}

fn same_folder(a: &Path, b: &Path) -> bool {
    let (Ok(x), Ok(y)) = (a.metadata(), b.metadata()) else {
        return false;
    };
    if !x.is_dir() || !y.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        x.dev() == y.dev() && x.ino() == y.ino()
    }
    // `MetadataExt::file_index` / `volume_serial_number` are still unstable
    // (`windows_by_handle`). Canonical paths are the stable equivalent for
    // same-folder dedup (kernel account stamps have the same constraint).
    #[cfg(not(unix))]
    {
        let (Ok(left), Ok(right)) = (a.canonicalize(), b.canonicalize()) else {
            return false;
        };
        left == right
    }
}

fn list_buddy_files(_agent: &str, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
    let projects = root.join("projects");
    let mut paths = Vec::new();
    if !projects.is_dir() {
        return Ok(Vec::new());
    }
    walk_linked(&projects, &projects, &mut paths)
        .map_err(|_| PluginError::io("cannot list WorkBuddy sessions"))?;
    let mut files = Vec::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".jsonl").map(str::to_owned) else {
            continue;
        };
        files.push(SessionFile::new(id, path));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
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

fn walk_real(walk: &Path, lexical: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(walk)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(walk).expect("walk prefix");
        let reported = lexical.join(rel);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk_real(&path, &reported, out)?;
        } else if meta.is_file() && reported.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(reported);
        }
    }
    Ok(())
}

fn buddy_parser(
    agent: &str,
    file: &SessionFile,
    state: Option<&str>,
) -> Result<Box<dyn SessionParser>, PluginError> {
    let parent = subagent_parent(agent, file);
    let restored = state
        .map(json::from_str::<BuddyState>)
        .transpose()
        .map_err(|_| PluginError::decode("invalid WorkBuddy parser state"))?;
    Ok(Box::new(BuddyParser {
        state: restored.unwrap_or_else(|| BuddyState::new(agent, file, parent)),
    }))
}

fn subagent_parent(agent: &str, file: &SessionFile) -> Option<String> {
    let is_sub = file
        .path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir == "subagents");
    if !is_sub {
        return None;
    }
    let parent = file
        .path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())?;
    Some(format!("{agent}:{parent}"))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct BuddyState {
    record: SessionRecord,
    custom: String,
    named: String,
    first: String,
    model: String,
    msg_id: String,
    msg_model: String,
    msg_use: SessionTokens,
    msg_day: String,
}

impl BuddyState {
    fn new(agent: &str, file: &SessionFile, parent: Option<String>) -> Self {
        let id = file.id.trim_start_matches(&format!("{agent}:"));
        let mut record = SessionRecord::new(agent, format!("{agent}:{id}"), &file.path);
        id.clone_into(&mut record.title);
        if let Some(parent) = parent {
            record = record.with_parent(parent);
        }
        Self {
            record,
            custom: String::new(),
            named: String::new(),
            first: String::new(),
            model: String::new(),
            msg_id: String::new(),
            msg_model: String::new(),
            msg_use: SessionTokens::default(),
            msg_day: String::new(),
        }
    }

    fn finish_title(&mut self) {
        let fallback = self
            .record
            .id
            .split_once(':')
            .map_or_else(|| self.record.id.clone(), |(_, rest)| rest.to_owned());
        self.record.title = if !self.custom.is_empty() {
            self.custom.clone()
        } else if !self.named.is_empty() {
            self.named.clone()
        } else if !self.first.is_empty() {
            self.first.clone()
        } else {
            fallback
        };
    }
}

struct BuddyParser {
    state: BuddyState,
}

impl BuddyParser {
    #[allow(clippy::too_many_lines)]
    fn line(&mut self, value: &Value) {
        self.state.finish_title();
        let at = json_ms(value);
        self.saw(at, self.state.record.main);
        let want = value.pointer("/providerData/usage").is_some()
            || (self.state.record.main
                && (value
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t == "custom-title" || t == "ai-title")
                    || (self.state.first.is_empty() && is_user_line(value))))
            || (self.state.record.cwd.is_empty() && value.get("cwd").is_some());
        if !want {
            return;
        }
        if self.state.record.cwd.is_empty()
            && let Some(cwd) = value.get("cwd").and_then(Value::as_str)
        {
            cwd.clone_into(&mut self.state.record.cwd);
        }
        if let Some(provider) = value.get("providerData") {
            let model = provider
                .get("model")
                .or_else(|| provider.get("requestModelId"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !model.is_empty() {
                model.clone_into(&mut self.state.model);
            }
        }
        match value.get("type").and_then(Value::as_str) {
            Some("custom-title") if self.state.record.main => {
                if let Some(title) = value.get("customTitle").and_then(Value::as_str) {
                    self.state.custom = title_line(title);
                }
            }
            Some("ai-title") if self.state.record.main => {
                if let Some(title) = value.get("aiTitle").and_then(Value::as_str) {
                    self.state.named = title_line(title);
                }
            }
            Some("message")
                if value.get("role").and_then(Value::as_str) == Some("user")
                    && self.state.record.main
                    && self.state.first.is_empty() =>
            {
                self.state.first = buddy_prompt(value.get("content"));
            }
            _ => {}
        }
        let Some(provider) = value.get("providerData") else {
            return;
        };
        let Some(usage) = provider.get("usage") else {
            return;
        };
        let in_tok = usage
            .get("inputTokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let out_tok = usage
            .get("outputTokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let read = details_sum(usage, "inputTokensDetails", "cached_tokens")
            + details_sum(usage, "input_tokens_details", "cached_tokens");
        let write = usage
            .get("cacheCreationTokens")
            .or_else(|| usage.get("cache_creation_input_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let tokens = SessionTokens::new(
            in_tok.saturating_sub(read).saturating_sub(write),
            out_tok,
            read,
            write,
        );
        let fallback_model = self.state.model.clone();
        let model = [
            provider.get("model").and_then(Value::as_str),
            provider.get("requestModelId").and_then(Value::as_str),
            Some(fallback_model.as_str()),
            Some("auto"),
        ]
        .into_iter()
        .flatten()
        .find(|model| !model.is_empty())
        .unwrap_or("auto")
        .to_owned();
        let msg_id = provider
            .get("messageId")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !msg_id.is_empty() && msg_id == self.state.msg_id {
            let day = self.state.msg_day.clone();
            let prior_model = self.state.msg_model.clone();
            let prior = self.state.msg_use;
            self.unuse(&day, &prior_model, prior);
        }
        let date = local_date(at).unwrap_or_default();
        msg_id.clone_into(&mut self.state.msg_id);
        self.state.msg_model.clone_from(&model);
        self.state.msg_use = tokens;
        self.state.msg_day.clone_from(&date);
        self.state.record.add_usage(&model, tokens);
        if let Some(day) = local_date(at) {
            self.state
                .record
                .add_day(SessionDay::new(day, &model, tokens));
        }
    }

    fn saw(&mut self, ms: i64, main: bool) {
        if ms <= 0 || !main {
            return;
        }
        if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
            self.state.record.start_ms = ms;
        }
        self.state.record.last_ms = self.state.record.last_ms.max(ms);
    }

    fn unuse(&mut self, date: &str, model: &str, tokens: SessionTokens) {
        if let Some(day) = self
            .state
            .record
            .days
            .iter_mut()
            .find(|day| day.date == date && day.model == model)
        {
            subtract_tokens(&mut day.tokens, tokens);
        }
        if let Some(model_tokens) = self.state.record.models.get_mut(model) {
            subtract_tokens(model_tokens, tokens);
        }
    }
}

fn subtract_tokens(target: &mut SessionTokens, amount: SessionTokens) {
    target.input = target.input.saturating_sub(amount.input);
    target.output = target.output.saturating_sub(amount.output);
    target.cache_read = target.cache_read.saturating_sub(amount.cache_read);
    target.cache_write = target.cache_write.saturating_sub(amount.cache_write);
    target.cache_write_1h = target.cache_write_1h.saturating_sub(amount.cache_write_1h);
}

impl SessionParser for BuddyParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        if self.state.record.cwd.is_empty() {
            let meta = self.state.record.path.with_extension("meta.json");
            if let Ok(body) = std::fs::read_to_string(&meta)
                && let Ok(value) = json::from_str::<Value>(&body)
                && let Some(cwd) = value.get("cwd").and_then(Value::as_str)
            {
                cwd.clone_into(&mut self.state.record.cwd);
            }
        }
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save WorkBuddy parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn is_user_line(value: &Value) -> bool {
    value.get("role").and_then(Value::as_str) == Some("user")
}

fn json_ms(value: &Value) -> i64 {
    value.get("timestamp").and_then(Value::as_i64).unwrap_or(0)
}

fn details_sum(usage: &Value, field: &str, key: &str) -> u64 {
    let Some(Value::Array(list)) = usage.get(field) else {
        return usage
            .get(field)
            .and_then(Value::as_object)
            .and_then(|map| map.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0);
    };
    list.iter()
        .filter_map(Value::as_object)
        .filter_map(|map| map.get(key))
        .filter_map(Value::as_u64)
        .sum()
}

fn buddy_prompt(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        let trimmed = text.trim();
        if trimmed.starts_with('<') {
            return String::new();
        }
        return title_line(trimmed);
    }
    let Some(blocks) = content.as_array() else {
        return String::new();
    };
    let scene = BUDDY_SCENE.get_or_init(|| Regex::new(r"^@[\w-]+:\S*$").expect("scene regex"));
    let mut parts = Vec::new();
    for block in blocks {
        let Some(obj) = block.as_object() else {
            continue;
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let text = obj.get("text").and_then(Value::as_str).unwrap_or("").trim();
        if (kind == "input_text" || kind == "text")
            && !text.is_empty()
            && !text.starts_with('<')
            && !scene.is_match(text)
        {
            parts.push(text);
        }
    }
    title_line(&parts.join(" "))
}

fn local_date(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|at| at.format("%Y-%m-%d").to_string())
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
    use super::*;
    use hofund_plugin_api::SessionStore;

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/root")
            .canonicalize()
            .expect("fixtures")
    }

    #[test]
    fn lists_three_workbuddy_sessions_from_golden_fixtures() {
        let store = WorkBuddyStore::with_root(fixture_root());
        let files = store.files(&fixture_root()).expect("files");
        assert_eq!(files.len(), 3);
        let launch = files
            .iter()
            .find(|file| file.id == "22222222-aaaa-4bbb-8ccc-000000000001")
            .expect("launch session");
        let mut parser = store.parser(launch, None).expect("parser");
        for line in std::fs::read_to_string(&launch.path)
            .expect("read")
            .lines()
            .filter(|line| !line.trim().is_empty())
        {
            let value: Value = json::from_str(line).expect("json");
            parser.push(&value).expect("push");
        }
        let parsed = parser.finish().expect("finish");
        let record = parsed.record.expect("record");
        assert_eq!(record.title, "Launch notes draft");
        assert_eq!(record.cwd, "/work/wb");
        let hy3 = record.models.get("hy3").copied().expect("hy3");
        assert_eq!(
            hy3,
            SessionTokens::new(2422, 968, 8777, 0),
            "{:?}",
            record.models
        );
    }

    #[test]
    fn old_session_reads_cwd_from_meta_json() {
        let store = WorkBuddyStore::with_root(fixture_root());
        let files = store.files(&fixture_root()).expect("files");
        let old = files
            .iter()
            .find(|file| file.id == "0123456789abcdef0123456789abcdef")
            .expect("old session");
        let mut parser = store.parser(old, None).expect("parser");
        for line in std::fs::read_to_string(&old.path).expect("read").lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = json::from_str(line).expect("json");
            parser.push(&value).expect("push");
        }
        let parsed = parser.finish().expect("finish");
        let record = parsed.record.expect("record");
        assert_eq!(record.cwd, "/work/wb-old");
        assert_eq!(record.title, "Quarterly report");
    }

    #[test]
    fn same_folder_accepts_identical_and_dot_paths() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("wb");
        std::fs::create_dir_all(&root).expect("mkdir");
        assert!(same_folder(&root, &root));
        assert!(same_folder(&root, &root.join(".")));
        assert!(!same_folder(&root, &temp.path().join("other")));
    }

    #[test]
    #[cfg(unix)]
    fn workbuddy_ai_skips_when_same_folder_as_workbuddy() {
        let temp = tempfile::tempdir().expect("temp");
        let wb_root = temp.path().join("wb");
        let ai_link = temp.path().join("wb-ai");
        std::fs::create_dir_all(wb_root.join("projects/demo")).expect("mkdir");
        std::fs::write(wb_root.join("projects/demo/s1.jsonl"), "{}\n").expect("write");
        std::os::unix::fs::symlink(&wb_root, &ai_link).expect("symlink");
        let ai = WorkBuddyAiStore::with_roots(&ai_link, &wb_root);
        let wb = WorkBuddyStore::with_root(&wb_root);
        assert_eq!(
            ai.files(ai.locator().roots()[0].as_path()).expect("ai"),
            vec![]
        );
        assert_eq!(
            wb.files(wb.locator().roots()[0].as_path())
                .expect("wb")
                .len(),
            1
        );
    }
}
