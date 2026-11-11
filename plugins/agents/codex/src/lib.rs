//! Codex rollout discovery and JSONL parsing.
//!
//! The kernel supplies consented roots and depth-checked values. This crate
//! owns Codex home discovery, rollout names, and token counter semantics.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use hofund_plugin_api::{
    PluginError, SessionCompression, SessionDay, SessionFile, SessionLocator, SessionParse,
    SessionParser, SessionRecord, SessionStore, SessionTokens, SessionUsageEvent,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "codex";

/// Codex's session store over every discovered home.
#[derive(Clone, Debug)]
pub struct CodexStore {
    locator: CodexLocator,
}

impl CodexStore {
    /// Uses `$CODEX_HOME` or `~/.codex`, then discovers sibling Codex homes.
    #[must_use]
    pub fn new() -> Self {
        Self::with_homes(discover_homes())
    }

    /// Uses explicit homes, for embedding and tests.
    #[must_use]
    pub fn with_homes(homes: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            locator: CodexLocator {
                homes: homes.into_iter().collect(),
            },
        }
    }

    /// Uses one explicit home.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::with_homes([root.into()])
    }
}

impl Default for CodexStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only Codex home locator.
#[derive(Clone, Debug)]
pub struct CodexLocator {
    homes: Vec<PathBuf>,
}

impl SessionLocator for CodexLocator {
    fn roots(&self) -> Vec<PathBuf> {
        self.homes.clone()
    }

    fn present(&self) -> bool {
        self.homes
            .iter()
            .any(|home| home.join("sessions").is_dir() || home.join("archived_sessions").is_dir())
    }
}

impl SessionStore for CodexStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let mut paths = Vec::new();
        for folder in ["sessions", "archived_sessions"] {
            let base = root.join(folder);
            if !base.exists() {
                continue;
            }
            walk_linked(&base, &base, &mut paths)
                .map_err(|_| PluginError::io("cannot list Codex sessions"))?;
        }
        let plain: BTreeSet<PathBuf> = paths
            .iter()
            .filter(|path| path.extension().is_some_and(|extension| extension != "zst"))
            .cloned()
            .collect();
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for path in paths {
            let compressed = path.extension().is_some_and(|extension| extension == "zst");
            if compressed && plain.contains(&path.with_extension("")) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(id) = rollout_id(name) else {
                continue;
            };
            if seen.insert(path.clone()) {
                let file = SessionFile::new(id, path);
                files.push(if compressed {
                    file.with_compression(SessionCompression::Zstd)
                } else {
                    file
                });
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
            .map(json::from_str::<CodexState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid Codex parser state"))?;
        Ok(Box::new(CodexParser {
            state: restored.unwrap_or_else(|| CodexState::new(file)),
        }))
    }
}

fn discover_homes() -> Vec<PathBuf> {
    let home = home_dir();
    let main = std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".codex"));
    let mut homes = vec![main.clone()];
    let mut seen = BTreeSet::from([same_dir(&main)]);
    let mut roots = vec![home.clone()];
    if let Some(parent) = main.parent()
        && parent != home
    {
        roots.push(parent.to_path_buf());
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if !name.trim_start_matches('.').starts_with("codex") {
                continue;
            }
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if codex_home_at(&dir) {
                add_home(&dir, &mut homes, &mut seen);
                continue;
            }
            if let Ok(children) = std::fs::read_dir(dir) {
                for child in children.flatten() {
                    let sub = child.path();
                    if codex_home_at(&sub) {
                        add_home(&sub, &mut homes, &mut seen);
                    }
                }
            }
        }
    }
    homes
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn codex_home_at(path: &Path) -> bool {
    path.join("auth.json").is_file() || path.join("sessions").is_dir()
}

fn same_dir(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn add_home(path: &Path, homes: &mut Vec<PathBuf>, seen: &mut BTreeSet<PathBuf>) {
    if seen.insert(same_dir(path)) {
        homes.push(path.to_path_buf());
    }
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

fn rollout_id(name: &str) -> Option<String> {
    let plain = name.strip_suffix(".zst").unwrap_or(name);
    let base = plain.strip_suffix(".jsonl")?.strip_prefix("rollout-")?;
    if base.len() < 21
        || !matches!(base.as_bytes().get(4), Some(b'-'))
        || !matches!(base.as_bytes().get(7), Some(b'-'))
        || !matches!(base.as_bytes().get(10), Some(b'T'))
        || !matches!(base.as_bytes().get(13), Some(b'-'))
        || !matches!(base.as_bytes().get(16), Some(b'-'))
        || base.as_bytes().get(19) != Some(&b'-')
    {
        return None;
    }
    for (index, byte) in base.bytes().take(19).enumerate() {
        if matches!(index, 4 | 7 | 10 | 13 | 16) {
            continue;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
    }
    let rest = &base[20..];
    let id = rest.split('_').next().unwrap_or("");
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    Some(id.to_ascii_lowercase())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct RawUsage {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

impl RawUsage {
    fn of(value: &Value) -> Option<Self> {
        Some(Self {
            input: value.get("input_tokens")?.as_u64().unwrap_or(0),
            cached: value
                .get("cached_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_write: value
                .get("cache_write_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output: value.get("output_tokens")?.as_u64().unwrap_or(0),
        })
    }

    fn spent(self) -> SessionTokens {
        SessionTokens::new(
            self.input
                .saturating_sub(self.cached.saturating_add(self.cache_write)),
            self.output,
            self.cached,
            self.cache_write,
        )
    }

    fn less(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_sub(other.input),
            cached: self.cached.saturating_sub(other.cached),
            cache_write: self.cache_write.saturating_sub(other.cache_write),
            output: self.output.saturating_sub(other.output),
        }
    }

    fn zero(self) -> bool {
        self == Self::default()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct CodexState {
    record: SessionRecord,
    model: String,
    named: String,
    prompt: String,
    total: Option<RawUsage>,
}

impl CodexState {
    fn new(file: &SessionFile) -> Self {
        let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{}", file.id), &file.path);
        record.title.clone_from(&file.id);
        Self {
            record,
            model: String::new(),
            named: String::new(),
            prompt: String::new(),
            total: None,
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

struct CodexParser {
    state: CodexState,
}

impl CodexParser {
    fn line(&mut self, value: &Value) {
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
        let Some(payload) = value.get("payload") else {
            return;
        };
        let top = value.get("type").and_then(Value::as_str).unwrap_or("");
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match top {
            "session_meta" => self.meta(payload),
            "turn_context" => self.turn(payload),
            "response_item" if kind == "message" => self.message(payload),
            "event_msg" => match kind {
                "user_message" => self.user_message(payload),
                "thread_settings_applied" => {
                    if let Some(model) = payload
                        .get("thread_settings")
                        .and_then(|settings| settings.get("model"))
                        .and_then(Value::as_str)
                    {
                        model.clone_into(&mut self.state.model);
                    }
                }
                "token_count" => self.token_count(payload, at),
                _ => {}
            },
            _ => {}
        }
        self.state.finish_title();
    }

    fn meta(&mut self, payload: &Value) {
        if let Some(id) = payload.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            self.state.record.id = format!("{AGENT}:{id}");
        }
        self.state.record.forked_from = payload
            .get("forked_from_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|id| format!("{AGENT}:{id}"));
        if self.state.record.cwd.is_empty()
            && let Some(cwd) = payload.get("cwd").and_then(Value::as_str)
        {
            cwd.clone_into(&mut self.state.record.cwd);
        }
        if let Some(name) = payload.get("agent_nickname").and_then(Value::as_str) {
            self.state.named = title_line(name);
        }
        let source_parent = payload
            .get("source")
            .and_then(|source| source.get("subagent"))
            .and_then(|subagent| subagent.get("thread_spawn"))
            .and_then(|spawn| spawn.get("parent_thread_id"))
            .and_then(Value::as_str);
        let parent = payload
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .or(source_parent);
        if let Some(parent) = parent.filter(|parent| !parent.is_empty()) {
            self.state.record = self
                .state
                .record
                .clone()
                .with_parent(format!("{AGENT}:{parent}"));
        }
    }

    fn turn(&mut self, payload: &Value) {
        if let Some(model) = payload.get("model").and_then(Value::as_str)
            && !model.is_empty()
        {
            model.clone_into(&mut self.state.model);
        }
        if self.state.record.cwd.is_empty()
            && let Some(cwd) = payload.get("cwd").and_then(Value::as_str)
        {
            cwd.clone_into(&mut self.state.record.cwd);
        }
    }

    fn message(&mut self, payload: &Value) {
        if payload.get("role").and_then(Value::as_str) != Some("user") {
            return;
        }
        if let Some(content) = payload.get("content").and_then(Value::as_array) {
            for part in content {
                if matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "text")
                ) && let Some(text) = part.get("text").and_then(Value::as_str)
                {
                    self.prompt(text);
                }
            }
        }
    }

    fn user_message(&mut self, payload: &Value) {
        if let Some(message) = payload.get("message").and_then(Value::as_str) {
            self.prompt(message);
        }
    }

    fn prompt(&mut self, text: &str) {
        if self.state.prompt.is_empty() {
            let trimmed = text.trim();
            if !trimmed.is_empty()
                && !trimmed.starts_with('<')
                && !trimmed.starts_with("# AGENTS.md")
            {
                self.state.prompt = title_line(trimmed);
            }
        }
    }

    fn token_count(&mut self, payload: &Value, at: Option<DateTime<chrono::FixedOffset>>) {
        let Some(info) = payload.get("info") else {
            return;
        };
        let Some(total) = info.get("total_token_usage").and_then(RawUsage::of) else {
            return;
        };
        if self.state.total == Some(total) {
            return;
        }
        let last = info.get("last_token_usage").and_then(RawUsage::of);
        let used = match (last, self.state.total) {
            (Some(last), _) => last,
            (None, Some(previous)) => total.less(previous),
            (None, None) => total,
        };
        self.state.total = Some(total);
        if used.zero() || self.state.model.is_empty() {
            return;
        }
        let model = self.state.model.clone();
        let tokens = used.spent();
        let date = at.map_or_else(String::new, |at| {
            at.with_timezone(&Local).format("%Y-%m-%d").to_string()
        });
        self.state.record.usage.push(SessionUsageEvent::new(
            SessionTokens::new(total.input, total.output, total.cached, total.cache_write),
            tokens,
            &model,
            &date,
        ));
        self.state.record.add_usage(&model, tokens);
        if !date.is_empty() {
            self.state
                .record
                .add_day(SessionDay::new(date, model, tokens));
        }
    }
}

impl SessionParser for CodexParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save Codex parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
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
            rollout_id(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .expect("name"),
            )
            .expect("rollout id"),
            path,
        );
        let store = CodexStore::with_root(fixture("home"));
        let mut parser = store.parser(&file, None).expect("parser");
        let input = std::fs::File::open(path).expect("fixture");
        for line in BufReader::new(input).lines() {
            let line = line.expect("UTF-8 line");
            let value = json::from_str(&line).expect("JSON fixture");
            parser.push(&value).expect("push");
        }
        parser.finish().expect("finish").record.expect("record")
    }

    #[test]
    fn codex_golden_fixture() {
        let mut record = parse(&fixture(
            "home/sessions/2026/09/20/rollout-2026-09-20T15-48-28-01a0bdc9-b5fd-7e63-9658-68bc8dce5ecd.jsonl",
        ));
        record.path = PathBuf::new();
        let actual = json::to_value(record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("codex-main.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn codex_plain_wins_over_zst() {
        let store = CodexStore::with_root(fixture("home"));
        let files = store.files(&fixture("home")).expect("files");
        let matching: Vec<_> = files
            .iter()
            .filter(|file| file.id == "01a0bdc9-b5fd-7e63-9658-68bc8dce5ecd")
            .collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].compression, SessionCompression::None);
        assert!(
            files
                .iter()
                .any(|file| file.compression == SessionCompression::Zstd)
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_root_link_walk_uses_lexical_paths() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        symlink(fixture("home"), temp.path().join("linked")).expect("root link");
        let store = CodexStore::with_root(temp.path());
        let files = store.files(&temp.path().join("linked")).expect("files");
        assert_ne!(files, Vec::<SessionFile>::new());
        assert!(
            files
                .iter()
                .all(|file| file.path.starts_with(temp.path().join("linked")))
        );
    }
}
