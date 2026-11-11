//! `minimax-code` session discovery and `messages.jsonl` parsing.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use chrono::{DateTime, Local, TimeZone};
use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionLocator, SessionParse, SessionParser,
    SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "minimax-code";

/// `minimax-code` data under `MINIMAX_DATA_DIR` or `~/.minimax`.
#[derive(Clone, Debug)]
pub struct MiniMaxStore {
    locator: MiniMaxLocator,
}

impl MiniMaxStore {
    /// Uses `$MINIMAX_DATA_DIR` or `~/.minimax`.
    #[must_use]
    pub fn new() -> Self {
        let root = std::env::var_os("MINIMAX_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map_or_else(home_root, PathBuf::from);
        Self::with_root(root)
    }

    /// Uses an explicit root, for embedding and tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            locator: MiniMaxLocator { root: root.into() },
        }
    }
}

impl Default for MiniMaxStore {
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
        || PathBuf::from(".minimax"),
        |home| PathBuf::from(home).join(".minimax"),
    )
}

/// Directory-only `minimax-code` locator.
#[derive(Clone, Debug)]
pub struct MiniMaxLocator {
    root: PathBuf,
}

impl SessionLocator for MiniMaxLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("v2").join("sessions").is_dir()
    }
}

impl SessionStore for MiniMaxStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let sessions = root.join("v2").join("sessions");
        if !sessions.is_dir() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        walk_linked(&sessions, &sessions, &mut paths)
            .map_err(|_| PluginError::io("cannot list MiniMax Code sessions"))?;
        let mut files = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for path in paths {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with("messages")
                || !Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            {
                continue;
            }
            let Some(parent) = path.parent().and_then(|parent| parent.file_name()) else {
                continue;
            };
            let parent = parent.to_string_lossy();
            let id = session_id_from_dir(&parent);
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
            .map(json::from_str::<MiniMaxState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid MiniMax Code parser state"))?;
        Ok(Box::new(MiniMaxParser {
            state: restored.unwrap_or_else(|| MiniMaxState::new(file)),
        }))
    }
}

fn session_id_from_dir(dir: &str) -> String {
    let Some((_, encoded)) = dir.split_once("-session_") else {
        return dir.to_owned();
    };
    for engine in [URL_SAFE_NO_PAD, URL_SAFE, STANDARD_NO_PAD, STANDARD] {
        if let Ok(bytes) = engine.decode(encoded)
            && !bytes.is_empty()
        {
            return String::from_utf8_lossy(&bytes).into_owned();
        }
    }
    encoded.to_owned()
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
struct MiniMaxUsage {
    input: u64,
    output: u64,
    #[serde(rename = "cacheRead", default)]
    cache_read: u64,
    #[serde(rename = "cacheWrite", default)]
    cache_write: u64,
}

impl MiniMaxUsage {
    fn tokens(self) -> SessionTokens {
        SessionTokens::new(self.input, self.output, self.cache_read, self.cache_write)
    }

    fn of(value: &Value) -> Option<Self> {
        serde::Deserialize::deserialize(value).ok()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct MiniMaxState {
    record: SessionRecord,
    first: String,
}

impl MiniMaxState {
    fn new(file: &SessionFile) -> Self {
        let id = format!("{AGENT}:{}", file.id);
        Self {
            record: SessionRecord::new(AGENT, &id, &file.path),
            first: String::new(),
        }
    }

    fn finish_title(&mut self) {
        self.record.title = if self.first.is_empty() {
            self.record
                .id
                .strip_prefix(&format!("{AGENT}:"))
                .unwrap_or(&self.record.id)
                .to_owned()
        } else {
            self.first.clone()
        };
    }
}

struct MiniMaxParser {
    state: MiniMaxState,
}

impl MiniMaxParser {
    fn push_line(&mut self, value: &Value) {
        let Some(message) = value.get("message") else {
            return;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let at = message
            .get("timestamp")
            .and_then(Value::as_i64)
            .and_then(ms_to_datetime);
        self.saw(at);
        match role {
            "user" => {
                let text = message.get("content").map(content_text).unwrap_or_default();
                if self.state.first.is_empty()
                    && let Some(prompt) = user_prompt(&text)
                {
                    self.state.first = prompt;
                }
            }
            "assistant" => {
                let model = message
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| !model.is_empty())
                    .unwrap_or("unknown");
                if let Some(usage) = message.get("usage").and_then(MiniMaxUsage::of)
                    && !usage.tokens().is_zero()
                {
                    let tokens = usage.tokens();
                    self.state.record.add_usage(model.to_owned(), tokens);
                    if let Some(at) = at {
                        let date = at.with_timezone(&Local).format("%Y-%m-%d").to_string();
                        self.state
                            .record
                            .add_day(SessionDay::new(date, model, tokens));
                    }
                }
            }
            _ => {}
        }
        self.state.finish_title();
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
}

impl SessionParser for MiniMaxParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.push_line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save MiniMax Code parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn ms_to_datetime(ms: i64) -> Option<DateTime<chrono::FixedOffset>> {
    let secs = ms.div_euclid(1000);
    let sub = ms.rem_euclid(1000);
    Local
        .timestamp_millis_opt(secs * 1000 + sub)
        .single()
        .map(|local| local.fixed_offset())
}

fn content_text(content: &Value) -> String {
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
            && !text.is_empty()
        {
            parts.push(text);
        }
    }
    parts.join("\n")
}

fn user_prompt(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.starts_with('<') {
        return None;
    }
    Some(title_line(trimmed))
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
        let parent = path.parent().and_then(Path::file_name).expect("parent");
        let id = session_id_from_dir(&parent.to_string_lossy());
        let file = SessionFile::new(id, path);
        let store = MiniMaxStore::with_root(fixture("root"));
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
    fn minimax_golden_fixture() {
        let mut record = parse(&fixture(
            "root/v2/sessions/2026/10/02/12-00-00-000-session_bXZzX2RlbW8tMDAx/messages.jsonl",
        ));
        record.path = PathBuf::new();
        let actual = json::to_value(record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("minimax-main.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn minimax_lists_messages_files() {
        let store = MiniMaxStore::with_root(fixture("root"));
        let files = store.files(&fixture("root")).expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, "mvs_demo-001");
    }

    #[test]
    fn session_id_decodes_base64_suffix() {
        assert_eq!(
            session_id_from_dir("12-00-00-000-session_bXZzX2RlbW8tMDAx"),
            "mvs_demo-001"
        );
        assert_eq!(session_id_from_dir("plain-folder"), "plain-folder");
    }
}
