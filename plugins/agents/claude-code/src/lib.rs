//! Claude Code session discovery and JSONL parsing.
//!
//! The kernel supplies a consented root and depth-checked JSON values. This
//! crate owns only Claude Code's paths and record format.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionLocator, SessionParse, SessionParser,
    SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

mod desktop;

pub use desktop::{ClaudeDesktopLocator, ClaudeDesktopStore, DESKTOP_AGENT};

/// The session agent id.
pub const AGENT: &str = "claude";

/// Claude Code's session store.
#[derive(Clone, Debug)]
pub struct ClaudeStore {
    locator: ClaudeLocator,
}

impl ClaudeStore {
    /// Uses `$CLAUDE_CONFIG_DIR` or `~/.claude`.
    #[must_use]
    pub fn new() -> Self {
        let root = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map_or_else(home_root, PathBuf::from);
        Self::with_root(root)
    }

    /// Uses an explicit root, for embedding and tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            locator: ClaudeLocator { root: root.into() },
        }
    }
}

impl Default for ClaudeStore {
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
        || PathBuf::from(".claude"),
        |home| PathBuf::from(home).join(".claude"),
    )
}

/// Directory-only Claude Code locator.
#[derive(Clone, Debug)]
pub struct ClaudeLocator {
    root: PathBuf,
}

impl SessionLocator for ClaudeLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("projects").is_dir()
    }
}

impl SessionStore for ClaudeStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        collect_cc_files(AGENT, &root.join("projects"))
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        parser_for(AGENT, file, state)
    }
}

fn parser_for(
    agent: &str,
    file: &SessionFile,
    state: Option<&str>,
) -> Result<Box<dyn SessionParser>, PluginError> {
    let parent = subagent_parent(agent, file);
    let restored = state
        .map(json::from_str::<ClaudeState>)
        .transpose()
        .map_err(|_| PluginError::decode("invalid Claude Code parser state"))?;
    Ok(Box::new(ClaudeParser {
        state: restored.unwrap_or_else(|| ClaudeState::new(agent, file, parent)),
    }))
}

pub(crate) fn collect_cc_files(
    _agent: &str,
    projects: &Path,
) -> Result<Vec<SessionFile>, PluginError> {
    let mut paths = Vec::new();
    if !projects.exists() {
        return Ok(Vec::new());
    }
    walk_linked(projects, projects, &mut paths)
        .map_err(|_| PluginError::io("cannot list Claude Code sessions"))?;
    let mut files = Vec::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let id = name.strip_suffix(".jsonl").map(str::to_owned);
        if path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|dir| dir == "subagents")
        {
            if name.starts_with("agent-")
                && let Some(id) = id
            {
                files.push(SessionFile::new(id, path));
            }
        } else if let Some(id) = id {
            files.push(SessionFile::new(id, path));
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

pub(crate) fn walk_desktop_homes(root: &Path) -> Result<Vec<PathBuf>, PluginError> {
    let mode = root.join("local-agent-mode-sessions");
    let mut homes = Vec::new();
    if mode.is_dir() {
        for account in entries(&mode)? {
            if !account
                .file_type()
                .map_err(|_| PluginError::io("cannot list Claude Desktop sessions"))?
                .is_dir()
            {
                continue;
            }
            for org in entries(&account.path())? {
                if !org
                    .file_type()
                    .map_err(|_| PluginError::io("cannot list Claude Desktop sessions"))?
                    .is_dir()
                {
                    continue;
                }
                for local in entries(&org.path())? {
                    let name = local.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if !name.starts_with("local_") {
                        continue;
                    }
                    let claude = local.path().join(".claude");
                    if claude.is_dir() {
                        homes.push(claude);
                    }
                }
            }
        }
    }
    homes.sort();
    homes.dedup();
    Ok(homes)
}

fn entries(path: &Path) -> Result<Vec<std::fs::DirEntry>, PluginError> {
    let Ok(directory) = std::fs::read_dir(path) else {
        return Ok(Vec::new());
    };
    let mut entries = directory
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot read session directory"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
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

fn desktop_entrypoint(value: &Value) -> bool {
    value
        .get("entrypoint")
        .and_then(Value::as_str)
        .is_some_and(|entry| entry == "claude-desktop" || entry == "claude-desktop-3p")
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct SeenUsage {
    model: String,
    tokens: SessionTokens,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct ClaudeState {
    agent: String,
    record: SessionRecord,
    custom: String,
    named: String,
    prompt: String,
    seen: BTreeMap<String, SeenUsage>,
    root: String,
    leaf: String,
    compacts: Vec<String>,
    #[serde(default)]
    uuids: BTreeSet<String>,
}

impl ClaudeState {
    fn new(agent: &str, file: &SessionFile, parent: Option<String>) -> Self {
        let id = format!("{agent}:{}", file.id);
        let mut record = SessionRecord::new(agent, &id, &file.path);
        record.title.clone_from(&file.id);
        if let Some(parent) = parent {
            record = record.with_parent(parent);
        }
        Self {
            agent: agent.to_owned(),
            record,
            custom: String::new(),
            named: String::new(),
            prompt: String::new(),
            seen: BTreeMap::new(),
            root: String::new(),
            leaf: String::new(),
            compacts: Vec::new(),
            uuids: BTreeSet::new(),
        }
    }

    fn finish_title(&mut self) {
        let fallback = self
            .record
            .id
            .strip_prefix(&format!("{}:", self.agent))
            .unwrap_or(&self.record.id)
            .to_owned();
        self.record.title = if !self.custom.is_empty() {
            self.custom.clone()
        } else if !self.named.is_empty() {
            self.named.clone()
        } else if !self.prompt.is_empty() {
            self.prompt.clone()
        } else {
            fallback
        };
        self.record.family_root.clone_from(&self.root);
        self.record.family_leaf.clone_from(&self.leaf);
        self.record.family_ids = self.uuids.iter().cloned().collect();
    }
}

struct ClaudeParser {
    state: ClaudeState,
}

impl ClaudeParser {
    fn line(&mut self, value: &Value) {
        if self.state.agent == AGENT
            && self.state.record.app.is_empty()
            && desktop_entrypoint(value)
        {
            DESKTOP_AGENT.clone_into(&mut self.state.record.app);
        }
        let at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok());
        if let Some(at) = at {
            let ms = at.timestamp_millis();
            if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
                self.state.record.start_ms = ms;
            }
            self.state.record.last_ms = self.state.record.last_ms.max(ms);
        }
        if self.state.record.cwd.is_empty()
            && let Some(cwd) = value.get("cwd").and_then(Value::as_str)
        {
            cwd.clone_into(&mut self.state.record.cwd);
        }
        if self.state.record.main
            && let Some(session) = value.get("sessionId").and_then(Value::as_str)
            && !session.is_empty()
            && self.state.record.id
                == format!("{}:{}", self.state.agent, file_stem_id(&self.state.record))
        {
            self.state.record.id = format!("{}:{session}", self.state.agent);
        }

        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "custom-title" if self.state.record.main => {
                if let Some(title) = value.get("customTitle").and_then(Value::as_str) {
                    self.state.custom = title_line(title);
                }
            }
            "ai-title" => {
                if let Some(title) = value.get("aiTitle").and_then(Value::as_str) {
                    self.state.named = title_line(title);
                }
            }
            "summary" if self.state.named.is_empty() => {
                if let Some(title) = value.get("summary").and_then(Value::as_str) {
                    self.state.named = title_line(title);
                }
            }
            "user" => self.user(value),
            "assistant" => self.assistant(value, at),
            _ => {}
        }
        if self.state.record.main
            && matches!(kind, "user" | "assistant")
            && !value
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && let Some(uuid) = value.get("uuid").and_then(Value::as_str)
            && random_uuid(uuid)
        {
            if self.state.root.is_empty() {
                uuid.clone_into(&mut self.state.root);
            }
            uuid.clone_into(&mut self.state.leaf);
            self.state.uuids.insert(uuid.to_owned());
            if kind == "user"
                && value
                    .get("isCompactSummary")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                && !self.state.compacts.iter().any(|item| item == uuid)
            {
                self.state.compacts.push(uuid.to_owned());
            }
        }
        self.state.finish_title();
    }

    fn user(&mut self, value: &Value) {
        if (self.state.record.main
            && (value
                .get("isMeta")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || value
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)))
            || !self.state.prompt.is_empty()
        {
            return;
        }
        let content = value
            .get("message")
            .and_then(|message| message.get("content"));
        let text = content.map(content_text).unwrap_or_default();
        let prompt = prompt_text(&text);
        if !prompt.is_empty() {
            self.state.prompt = prompt;
        }
    }

    fn assistant(&mut self, value: &Value, at: Option<DateTime<chrono::FixedOffset>>) {
        let Some(message) = value.get("message") else {
            return;
        };
        let model = message.get("model").and_then(Value::as_str).unwrap_or("");
        if model == "<synthetic>" {
            return;
        }
        let Some(usage) = message.get("usage") else {
            return;
        };
        let creation = usage.get("cache_creation");
        let ephemeral = creation.map_or(0, |creation| {
            counter(creation, "ephemeral_5m_input_tokens")
                .saturating_add(counter(creation, "ephemeral_1h_input_tokens"))
        });
        let tokens = SessionTokens::new(
            counter(usage, "input_tokens"),
            counter(usage, "output_tokens"),
            counter(usage, "cache_read_input_tokens"),
            counter(usage, "cache_creation_input_tokens").max(ephemeral),
        )
        .with_cache_write_1h(
            creation
                .and_then(|creation| creation.get("ephemeral_1h_input_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        if model.is_empty() || tokens.is_zero() {
            return;
        }
        let id = message.get("id").and_then(Value::as_str).unwrap_or("");
        let request = value.get("requestId").and_then(Value::as_str).unwrap_or("");
        if id.is_empty() && request.is_empty() {
            self.count(model, tokens, at);
            return;
        }
        let key = format!("{id}\0{request}");
        if self.state.seen.contains_key(&key) {
            return;
        }
        self.state.seen.insert(
            key,
            SeenUsage {
                model: model.to_owned(),
                tokens,
            },
        );
        self.count(model, tokens, at);
    }

    fn count(
        &mut self,
        model: &str,
        tokens: SessionTokens,
        at: Option<DateTime<chrono::FixedOffset>>,
    ) {
        self.state.record.add_usage(model, tokens);
        if let Some(at) = at {
            let date = at.with_timezone(&Local).format("%Y-%m-%d").to_string();
            self.state
                .record
                .add_day(SessionDay::new(date, model, tokens));
        }
    }
}

impl SessionParser for ClaudeParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save Claude Code parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn file_stem_id(record: &SessionRecord) -> String {
    record
        .id
        .split_once(':')
        .map_or_else(|| record.id.clone(), |(_, id)| id.to_owned())
}

fn counter(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
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
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            return String::new();
        }
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
            && !text.is_empty()
        {
            parts.push(text);
        }
    }
    parts.join(" ").trim().to_owned()
}

fn prompt_text(text: &str) -> String {
    if let Some(start) = text.find("<command-name>") {
        let rest = &text[start + "<command-name>".len()..];
        if let Some(end) = rest.find("</command-name>") {
            let mut command = title_line(rest[..end].trim());
            if let Some(args_start) = text.find("<command-args>") {
                let args = &text[args_start + "<command-args>".len()..];
                if let Some(args_end) = args.find("</command-args>") {
                    let args = args[..args_end].trim();
                    if !args.is_empty() {
                        command.push(' ');
                        command.push_str(args);
                    }
                }
            }
            return command;
        }
    }
    if text.is_empty()
        || text.starts_with('<')
        || text.starts_with("[Request interrupted")
        || text.starts_with("Caveat:")
    {
        String::new()
    } else {
        title_line(text)
    }
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

fn random_uuid(uuid: &str) -> bool {
    let bytes = uuid.as_bytes();
    bytes.len() == 36
        && bytes[14] == b'4'
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
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
        let file = SessionFile::new(
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .expect("stem"),
            path,
        );
        let store = ClaudeStore::with_root(fixture("root"));
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
    fn claude_golden_fixture() {
        let mut record = parse(&fixture(
            "root/projects/-work-app/11111111-2222-3333-4444-555555555555.jsonl",
        ));
        record.path = PathBuf::new();
        let actual = json::to_value(record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("claude-main.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn claude_subagent_groups_and_counts() {
        let record = parse(&fixture(
            "root/projects/-work-app/11111111-2222-3333-4444-555555555555/subagents/agent-a1.jsonl",
        ));
        assert!(!record.main);
        assert_eq!(
            record.parent_id.as_deref(),
            Some("claude:11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(record.title, "Search the répo");
        assert_eq!(record.tokens().total(), 1100);
    }

    #[test]
    fn claude_entrypoint_tags_desktop_app_usage() {
        let path = fixture("desktop/entrypoint.jsonl");
        let file = SessionFile::new("desktop-tab", &path);
        let store = ClaudeStore::with_root(fixture("root"));
        let mut parser = store.parser(&file, None).expect("parser");
        let input = std::fs::File::open(&path).expect("fixture");
        for line in BufReader::new(input).lines() {
            let line = line.expect("UTF-8");
            let value = json::from_str(&line).expect("JSON");
            parser.push(&value).expect("push");
        }
        let record = parser.finish().expect("finish").record.expect("record");
        assert_eq!(record.agent, AGENT);
        assert_eq!(record.app, DESKTOP_AGENT);
        assert_eq!(record.tokens().input, 4);
        assert_eq!(record.tokens().output, 1);
    }

    #[cfg(unix)]
    #[test]
    fn root_link_walk_returns_lexical_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        symlink(fixture("root"), temp.path().join("linked")).expect("root link");
        let store = ClaudeStore::with_root(temp.path());
        let files = store
            .files(&temp.path().join("linked"))
            .expect("walk linked root");
        assert_ne!(files, Vec::<SessionFile>::new());
        assert!(
            files
                .iter()
                .all(|file| file.path.starts_with(temp.path().join("linked")))
        );
    }

    #[test]
    fn subagent_parent_reads_the_parent_session_dir() {
        let child = SessionFile::new("child", "sessions/parent/subagents/child.jsonl");
        assert_eq!(
            subagent_parent("claude", &child).as_deref(),
            Some("claude:parent")
        );
        let main = SessionFile::new("main", "sessions/parent/main.jsonl");
        assert_eq!(subagent_parent("claude", &main), None);
    }

    #[test]
    fn desktop_entrypoint_matches_desktop_ids_only() {
        assert!(desktop_entrypoint(
            &json::json!({"entrypoint": "claude-desktop"})
        ));
        assert!(desktop_entrypoint(
            &json::json!({"entrypoint": "claude-desktop-3p"})
        ));
        assert!(!desktop_entrypoint(
            &json::json!({"entrypoint": "claude-code"})
        ));
        assert!(!desktop_entrypoint(&json::json!({})));
    }

    #[test]
    fn file_stem_id_strips_the_agent_prefix() {
        let record = SessionRecord::new("claude", "claude:abc", "p");
        assert_eq!(file_stem_id(&record), "abc");
        let record = SessionRecord::new("claude", "nocolon", "p");
        assert_eq!(file_stem_id(&record), "nocolon");
    }
}
