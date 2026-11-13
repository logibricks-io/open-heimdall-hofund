//! `DeepSeek` Harness session discovery and JSONL (optional zstd) parsing.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};
use hofund_plugin_api::{
    PluginError, SessionCompression, SessionDay, SessionFile, SessionLocator, SessionParse,
    SessionParser, SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use regex::Regex;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "dsh";

static DSH_NAME: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

/// `DeepSeek` Harness session store.
#[derive(Clone, Debug)]
pub struct DshStore {
    locator: DshLocator,
}

impl DshStore {
    /// Uses `$DSH_HOME` or `~/.dsh`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_root(dsh_home())
    }

    /// Uses an explicit home for tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            locator: DshLocator { root: root.into() },
        }
    }
}

impl Default for DshStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only dsh locator.
#[derive(Clone, Debug)]
pub struct DshLocator {
    root: PathBuf,
}

impl SessionLocator for DshLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("sessions").is_dir()
    }
}

impl SessionStore for DshStore {
    fn agent(&self) -> &str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        list_dsh_files(root)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        let restored = state
            .map(json::from_str::<DshState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid dsh parser state"))?;
        let head = read_head(&file.path)?.unwrap_or_default();
        Ok(Box::new(DshParser {
            state: restored.unwrap_or_else(|| DshState::new(file, &head)),
        }))
    }
}

fn dsh_home() -> PathBuf {
    std::env::var_os("DSH_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || user_home().join(".dsh"),
            |value| expand_home(&value.to_string_lossy()),
        )
}

fn user_home() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from(".dsh"), PathBuf::from)
}

fn expand_home(path: &str) -> PathBuf {
    let trimmed = path.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return user_home().join(rest);
    }
    if trimmed == "~" {
        return user_home();
    }
    PathBuf::from(trimmed)
}

fn dsh_name_pattern() -> &'static Regex {
    DSH_NAME.get_or_init(|| {
        Regex::new(r"^session(?:\.v(\d+))?\.jsonl(\.zstd)?$").expect("dsh name regex")
    })
}

fn list_dsh_files(root: &Path) -> Result<Vec<SessionFile>, PluginError> {
    let sessions = root.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for project in entries.flatten() {
        if !project.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(session_dirs) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for session_dir in session_dirs.flatten() {
            if !session_dir.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Some(file) = pick_session_file(&session_dir.path())? {
                files.push(file);
            }
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn pick_session_file(dir: &Path) -> Result<Option<SessionFile>, PluginError> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(None);
    };
    let mut best: Option<(i32, std::time::SystemTime, SessionFile)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(caps) = dsh_name_pattern().captures(name) else {
            continue;
        };
        let version = caps
            .get(1)
            .and_then(|m| m.as_str().parse::<i32>().ok())
            .unwrap_or(0);
        let zstd = caps.get(2).is_some();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let path = entry.path();
        let head = read_head(&path)?;
        let Some(head) = head else {
            continue;
        };
        let mut file = SessionFile::new(format!("{AGENT}:{}", head.id), path);
        if zstd {
            file = file.with_compression(SessionCompression::Zstd);
        }
        if let Some((best_gen, best_mod, _)) = &best
            && (version < *best_gen || (version == *best_gen && modified <= *best_mod))
        {
            continue;
        }
        best = Some((version, modified, file));
    }
    Ok(best.map(|(_, _, file)| file))
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct DshHead {
    #[serde(default)]
    version: i32,
    #[serde(default)]
    id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    parent_session: String,
    #[serde(default)]
    delegation_depth: i32,
    #[serde(default)]
    seed_length: i64,
    #[serde(default)]
    is_seeded: bool,
}

fn read_head(path: &Path) -> Result<Option<DshHead>, PluginError> {
    const MAX_HEADER_BYTES: usize = 1024 * 1024;

    let file = std::fs::File::open(path).map_err(|_| PluginError::io("cannot read dsh session"))?;
    let reader: Box<dyn Read> = if path
        .extension()
        .is_some_and(|ext| ext == "zstd" || ext == "zst")
    {
        let decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|_| PluginError::decode("cannot decompress dsh session header"))?;
        Box::new(decoder)
    } else {
        Box::new(file)
    };
    let mut bounded = BufReader::new(reader.take((MAX_HEADER_BYTES + 1) as u64));
    let mut bytes = Vec::new();
    bounded
        .read_until(b'\n', &mut bytes)
        .map_err(|_| PluginError::io("cannot read dsh session header"))?;
    if bytes.len() > MAX_HEADER_BYTES {
        return Ok(None);
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    let Ok(first) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let Ok(value) = json::from_str::<Value>(first) else {
        return Ok(None);
    };
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return Ok(None);
    }
    let head = DshHead {
        version: i32::try_from(value.get("version").and_then(Value::as_i64).unwrap_or(0))
            .unwrap_or(0),
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        created_at: value.get("createdAt").and_then(Value::as_i64).unwrap_or(0),
        parent_session: value
            .get("parentSession")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        delegation_depth: i32::try_from(
            value
                .get("delegationDepth")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        )
        .unwrap_or(0),
        seed_length: value.get("seedLength").and_then(Value::as_i64).unwrap_or(0),
        is_seeded: value
            .get("isSeeded")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    if head.id.is_empty() {
        return Ok(None);
    }
    Ok(Some(head))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct DshState {
    record: SessionRecord,
    head: DshHead,
    named: String,
    first: String,
    headed: bool,
}

impl DshState {
    fn new(file: &SessionFile, head: &DshHead) -> Self {
        let id = file.id.trim_start_matches(&format!("{AGENT}:"));
        let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{id}"), &file.path);
        id.clone_into(&mut record.title);
        if !head.cwd.is_empty() {
            head.cwd.clone_into(&mut record.cwd);
        }
        if head.delegation_depth > 0 && !head.parent_session.is_empty() {
            record = record.with_parent(format!("{AGENT}:{}", head.parent_session));
        }
        Self {
            record,
            head: head.clone(),
            named: String::new(),
            first: String::new(),
            headed: false,
        }
    }

    fn finish_title(&mut self) {
        let fallback = self
            .record
            .id
            .split_once(':')
            .map_or_else(|| self.record.id.clone(), |(_, rest)| rest.to_owned());
        if !self.named.is_empty() {
            self.named.clone_into(&mut self.record.title);
        } else if !self.first.is_empty() {
            self.first.clone_into(&mut self.record.title);
        } else {
            fallback.clone_into(&mut self.record.title);
        }
    }
}

struct DshParser {
    state: DshState,
}

impl DshParser {
    #[allow(clippy::too_many_lines)]
    fn line(&mut self, value: &Value) {
        if !self.state.headed {
            self.state.headed = true;
            if self.state.record.start_ms == 0 && self.state.head.created_at > 0 {
                self.state.record.start_ms = self.state.head.created_at;
            }
            return;
        }
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let seq = value.get("seq").and_then(Value::as_i64);
        let time_ms = value.get("time").and_then(Value::as_i64).unwrap_or(0);
        if self.state.head.version < 2
            && let Some(seq) = seq
            && seq < self.state.head.seed_length
        {
            return;
        }
        if event_type == "session/end-seed"
            && self.state.head.version >= 2
            && self.state.head.is_seeded
            && value
                .pointer("/data/inherited")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            let record = self.state.record.clone();
            let named = self.state.named.clone();
            self.state.first.clear();
            self.state.named.clear();
            self.state.record.models.clear();
            self.state.record.days.clear();
            self.state.record.title.clone_from(&named);
            self.state.record.start_ms = record.start_ms;
            self.state.record.last_ms = 0;
            self.state.record.cwd.clone_from(&record.cwd);
            if self.state.head.created_at > 0 {
                self.state.record.start_ms = self.state.head.created_at;
            }
            return;
        }
        if time_ms > 0 {
            self.saw(time_ms);
        }
        let data = value.get("data");
        match event_type {
            "user/message" if self.state.record.main => {
                let kind = data
                    .and_then(|d| d.pointer("/source/kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if (self.state.record.title.is_empty() || self.state.first.is_empty())
                    && kind == "user"
                {
                    let text = dsh_user_text(data.and_then(|d| d.get("content")));
                    if !text.starts_with('<') && self.state.record.title.is_empty() {
                        self.state.first = title_line(&text);
                    }
                }
            }
            "session/title" => {
                if let Some(title) = data.and_then(|d| d.get("title")).and_then(Value::as_str) {
                    self.state.named = title_line(title);
                }
            }
            "assistant/message" => {
                let Some(data) = data else {
                    return;
                };
                let model = data
                    .pointer("/message/source/model")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(usage) = data.get("usage") else {
                    return;
                };
                if model.is_empty() {
                    return;
                }
                let tokens = SessionTokens::new(
                    usage
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    usage
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    usage
                        .get("cacheReadTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    usage
                        .get("cacheWriteTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                if tokens.is_zero() {
                    return;
                }
                self.state.record.add_usage(model, tokens);
                if let Some(day) = local_date(time_ms) {
                    self.state
                        .record
                        .add_day(SessionDay::new(day, model, tokens));
                }
            }
            _ => {}
        }
    }

    fn saw(&mut self, ms: i64) {
        if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
            self.state.record.start_ms = ms;
        }
        self.state.record.last_ms = self.state.record.last_ms.max(ms);
    }
}

impl SessionParser for DshParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save dsh parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

fn dsh_user_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
    if let Some(parts) = content.as_array() {
        let mut out = Vec::new();
        for part in parts {
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = part.get("text").and_then(Value::as_str)
            {
                out.push(text);
            }
        }
        return out.join("\n");
    }
    String::new()
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

    fn fixture_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/root")
            .canonicalize()
            .expect("fixtures")
    }

    #[test]
    fn lists_dsh_sessions_from_golden_fixtures() {
        let store = DshStore::with_root(fixture_root());
        let files = store.files(&fixture_root()).expect("files");
        assert_eq!(files.len(), 5);
        let mut main = 0;
        for file in &files {
            let mut parser = store.parser(file, None).expect("parser");
            let body = std::fs::read_to_string(&file.path).expect("read");
            for line in body.lines().filter(|l| !l.trim().is_empty()) {
                let value: Value = json::from_str(line).expect("json");
                parser.push(&value).expect("push");
            }
            let parsed = parser.finish().expect("finish");
            if parsed.record.expect("record").main {
                main += 1;
            }
        }
        assert_eq!(main, 4);
        let a = files.iter().find(|f| f.id == "dsh:dsh-a").expect("dsh-a");
        let mut parser = store.parser(a, None).expect("parser");
        let body = std::fs::read_to_string(&a.path).expect("read");
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let value: Value = json::from_str(line).expect("json");
            parser.push(&value).expect("push");
        }
        let parsed = parser.finish().expect("finish");
        let record = parsed.record.expect("record");
        assert_eq!(record.title, "Build script tidy");
        assert_eq!(record.cwd, "/work/dsh");
    }

    #[test]
    fn lists_compressed_session_larger_than_one_mib() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path();
        let session = root.join("sessions/work/large");
        std::fs::create_dir_all(&session).expect("session dir");
        let mut body = b"{\"type\":\"session\",\"id\":\"large\",\"cwd\":\"/work\"}\n".to_vec();
        body.extend_from_slice(b"{\"type\":\"padding\",\"text\":\"");
        body.extend(std::iter::repeat_n(b'x', 1024 * 1024));
        body.extend_from_slice(b"\"}\n");
        let compressed = zstd::bulk::compress(&body, 3).expect("compress");
        std::fs::write(session.join("session.jsonl.zstd"), compressed).expect("compressed");

        let store = DshStore::with_root(root);
        let files = store.files(root).expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, "dsh:large");
        assert_eq!(files[0].compression, SessionCompression::Zstd);
    }
}
