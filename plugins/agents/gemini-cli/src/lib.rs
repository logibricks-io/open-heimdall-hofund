//! Gemini CLI session discovery and JSON/JSONL parsing.
//!
//! The kernel supplies consented roots and depth-checked JSON values. This
//! crate owns Gemini CLI tmp chat paths and token semantics.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use hofund_plugin_api::{
    PluginError, SessionDay, SessionFile, SessionFormat, SessionLocator, SessionParse,
    SessionParser, SessionRecord, SessionStore, SessionTokens,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "gemini";

/// Gemini CLI's session store.
#[derive(Clone, Debug)]
pub struct GeminiStore {
    locator: GeminiLocator,
}

impl GeminiStore {
    /// Uses `$GEMINI_CLI_HOME` or the user home and `.gemini/tmp`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_home(gemini_home())
    }

    /// Uses an explicit home directory for tests and embedding.
    #[must_use]
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            locator: GeminiLocator { home: home.into() },
        }
    }
}

impl Default for GeminiStore {
    fn default() -> Self {
        Self::new()
    }
}

fn gemini_home() -> PathBuf {
    std::env::var_os("GEMINI_CLI_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(home_dir, PathBuf::from)
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Directory-only Gemini CLI locator.
#[derive(Clone, Debug)]
pub struct GeminiLocator {
    home: PathBuf,
}

impl SessionLocator for GeminiLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.home.clone()]
    }

    fn present(&self) -> bool {
        self.home.join(".gemini").join("tmp").is_dir()
    }
}

impl SessionStore for GeminiStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let tmp = root.join(".gemini").join("tmp");
        if !tmp.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        for profile in entries(&tmp)? {
            if !profile
                .file_type()
                .map_err(|_| PluginError::io("cannot list Gemini sessions"))?
                .is_dir()
            {
                continue;
            }
            let chats = profile.path().join("chats");
            if !chats.is_dir() {
                continue;
            }
            for entry in entries(&chats)? {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if let Some(id) = name
                    .strip_prefix("session-")
                    .and_then(|stem| stem.strip_suffix(".json"))
                    .filter(|id| !id.is_empty())
                {
                    files.push(SessionFile::new(id, path.clone()).with_format(SessionFormat::Json));
                } else if let Some(id) = name
                    .strip_prefix("session-")
                    .and_then(|stem| stem.strip_suffix(".jsonl"))
                    .filter(|id| !id.is_empty())
                {
                    files.push(
                        SessionFile::new(id, path.clone()).with_format(SessionFormat::JsonLines),
                    );
                }
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
            .map(json::from_str::<GeminiState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid Gemini parser state"))?;
        Ok(Box::new(GeminiParser {
            state: restored.unwrap_or_else(|| GeminiState::new(file)),
        }))
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        if !gemini_on_path() {
            return None;
        }
        Some(resume_command(session_id))
    }
}

fn gemini_on_path() -> bool {
    gemini_in(std::env::var_os("PATH"))
}

fn gemini_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["gemini.exe", "gemini.cmd", "gemini"]
    }
    #[cfg(not(windows))]
    {
        &["gemini"]
    }
}

fn gemini_in(path: Option<std::ffi::OsString>) -> bool {
    let names = gemini_names();
    path.is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|directory| names.iter().any(|name| directory.join(name).is_file()))
    })
}

fn resume_command(session_id: &str) -> Vec<String> {
    let id = session_id
        .strip_prefix(&format!("{AGENT}:"))
        .unwrap_or(session_id);
    vec!["gemini".to_owned(), "--resume".to_owned(), id.to_owned()]
}

#[cfg(test)]
fn resume_argv_in(path: Option<std::ffi::OsString>, session_id: &str) -> Option<Vec<String>> {
    if !gemini_in(path) {
        return None;
    }
    Some(resume_command(session_id))
}

fn entries(path: &Path) -> Result<Vec<std::fs::DirEntry>, PluginError> {
    let Ok(directory) = std::fs::read_dir(path) else {
        return Ok(Vec::new());
    };
    let mut entries = directory
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list Gemini sessions"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct GeminiTokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    thoughts: u64,
    #[serde(default)]
    cached: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct GeminiMessage {
    id: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    tokens: GeminiTokens,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct GeminiState {
    record: SessionRecord,
    session: String,
    order: Vec<String>,
    messages: BTreeMap<String, GeminiMessage>,
    prompt: String,
}

impl GeminiState {
    fn new(file: &SessionFile) -> Self {
        let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{}", file.id), &file.path);
        record.title.clone_from(&file.id);
        Self {
            record,
            session: file.id.clone(),
            order: Vec::new(),
            messages: BTreeMap::new(),
            prompt: String::new(),
        }
    }

    fn finish_title(&mut self) {
        let fallback = self
            .record
            .id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(&self.record.id)
            .to_owned();
        if self.prompt.is_empty() {
            self.record.title = fallback;
        } else {
            self.record.title.clone_from(&self.prompt);
        }
    }

    fn line(&mut self, value: &Value) {
        if let Some(session) = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|session| !session.is_empty())
        {
            session.clone_into(&mut self.session);
            self.record.id = format!("{AGENT}:{session}");
        }
        if let Some(messages) = value.get("messages").and_then(Value::as_array) {
            for message in messages {
                self.put(message);
            }
        }
        if let Some(set) = value.get("$set") {
            if let Some(session) = set
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|session| !session.is_empty())
            {
                session.clone_into(&mut self.session);
                self.record.id = format!("{AGENT}:{session}");
            }
            if let Some(messages) = set.get("messages").and_then(Value::as_array) {
                self.messages.clear();
                self.order.clear();
                for message in messages {
                    self.put(message);
                }
            }
        }
        if let Some(index) = value
            .get("$rewindTo")
            .and_then(Value::as_str)
            .and_then(|rewind| self.order.iter().position(|id| id == rewind))
        {
            for remove in self
                .order
                .iter()
                .skip(index + 1)
                .cloned()
                .collect::<Vec<_>>()
            {
                self.messages.remove(&remove);
            }
            self.order.truncate(index + 1);
        }
        if let Some(patch) = value.get("$patch") {
            self.patch(patch);
        }
        if value.get("id").is_some() && value.get("type").is_some() {
            self.put(value);
        }
        if !self.session.is_empty() {
            self.record.id = format!("{AGENT}:{}", self.session);
        }
        self.rebuild_usage();
        self.finish_title();
    }

    fn patch(&mut self, patch: &Value) {
        let _ = patch
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.messages.get_mut(id));
        if let Some(updates) = patch.get("Updates").and_then(Value::as_array) {
            for update in updates {
                let wrapped = json::json!({ "$patch": update });
                self.line(&wrapped);
            }
        }
        for id in patch_ids(patch, "RemoveIDs") {
            self.messages.remove(&id);
        }
        let order_ids = patch_ids(patch, "OrderIDs");
        if !order_ids.is_empty() {
            let mut keep = Vec::new();
            for id in &self.order {
                if self.messages.contains_key(id) && !order_ids.iter().any(|item| item == id) {
                    keep.push(id.clone());
                }
            }
            for id in order_ids {
                if self.messages.contains_key(&id) {
                    keep.push(id);
                }
            }
            self.order = keep;
        }
    }

    fn put(&mut self, value: &Value) {
        let Ok(message) = json::from_value::<GeminiMessage>(value.clone()) else {
            return;
        };
        if message.id.is_empty() || message.kind.is_empty() {
            return;
        }
        if message.kind == "user"
            && self.prompt.is_empty()
            && let Some(text) = user_prompt(value.get("content"))
        {
            self.prompt = title_line(&text);
        }
        if !self.messages.contains_key(&message.id) {
            self.order.push(message.id.clone());
        }
        let timestamp = message.timestamp.clone();
        let id = message.id.clone();
        self.messages.insert(id.clone(), message);
        if let Ok(at) = DateTime::parse_from_rfc3339(&timestamp) {
            let ms = at.timestamp_millis();
            if self.record.start_ms == 0 || ms < self.record.start_ms {
                self.record.start_ms = ms;
            }
            self.record.last_ms = self.record.last_ms.max(ms);
        }
    }

    fn rebuild_usage(&mut self) {
        self.record.models.clear();
        self.record.days.clear();
        for id in &self.order {
            let Some(message) = self.messages.get(id) else {
                continue;
            };
            if message.kind != "gemini" || message.model.is_empty() {
                continue;
            }
            let tokens = gemini_tokens(&message.tokens);
            if tokens.is_zero() {
                continue;
            }
            self.record.add_usage(message.model.clone(), tokens);
            if let Ok(at) = DateTime::parse_from_rfc3339(&message.timestamp) {
                let date = at.with_timezone(&Local).format("%Y-%m-%d").to_string();
                self.record
                    .add_day(SessionDay::new(date, message.model.clone(), tokens));
            }
        }
    }
}

fn patch_ids(patch: &Value, key: &str) -> Vec<String> {
    patch
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn gemini_tokens(tokens: &GeminiTokens) -> SessionTokens {
    let input = tokens.input.saturating_sub(tokens.cached);
    let output = tokens.output.saturating_add(tokens.thoughts);
    SessionTokens::new(input, output, tokens.cached, 0)
}

fn user_prompt(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed.to_owned());
    }
    let array = content.as_array()?;
    for part in array {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
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

struct GeminiParser {
    state: GeminiState,
}

impl SessionParser for GeminiParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.state.line(value);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SessionParse, PluginError> {
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save Gemini parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
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

    fn parse_file(path: &Path, format: SessionFormat) -> SessionRecord {
        let id = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("session-"))
            .and_then(|stem| {
                stem.strip_suffix(".json")
                    .or_else(|| stem.strip_suffix(".jsonl"))
            })
            .expect("session id");
        let file = SessionFile::new(id, path).with_format(format);
        let store = GeminiStore::with_home(fixture("home"));
        let mut parser = store.parser(&file, None).expect("parser");
        if format == SessionFormat::Json {
            let value: Value =
                json::from_slice(&std::fs::read(path).expect("fixture")).expect("JSON");
            parser.push(&value).expect("push");
        } else {
            let input = std::fs::File::open(path).expect("fixture");
            for line in BufReader::new(input).lines() {
                let line = line.expect("UTF-8 line");
                if line.trim().is_empty() {
                    continue;
                }
                let value = json::from_str(&line).expect("JSON fixture");
                parser.push(&value).expect("push");
            }
        }
        parser
            .finish()
            .expect("finish")
            .record
            .expect("session record")
    }

    #[test]
    #[ignore = "run once to refresh golden fixtures"]
    fn dump_golden_fixtures() {
        let json_path = fixture("home/.gemini/tmp/p/chats/session-json.json");
        let jsonl_path = fixture("home/.gemini/tmp/p/chats/session-jsonl.jsonl");
        let mut json = parse_file(&json_path, SessionFormat::Json);
        let mut jsonl = parse_file(&jsonl_path, SessionFormat::JsonLines);
        json.path = PathBuf::new();
        jsonl.path = PathBuf::new();
        eprintln!("{}", json::to_string_pretty(&json).expect("json"));
        eprintln!("{}", json::to_string_pretty(&jsonl).expect("jsonl"));
    }

    #[test]
    fn gemini_json_snapshot_matches_golden() {
        let path = fixture("home/.gemini/tmp/p/chats/session-json.json");
        let mut record = parse_file(&path, SessionFormat::Json);
        record.path = PathBuf::new();
        let actual = json::to_value(record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("gemini-json.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn gemini_jsonl_patches_and_rewind_match_golden() {
        let path = fixture("home/.gemini/tmp/p/chats/session-jsonl.jsonl");
        let mut record = parse_file(&path, SessionFormat::JsonLines);
        record.path = PathBuf::new();
        let actual = json::to_value(record).expect("JSON");
        let expected: Value =
            json::from_str(&std::fs::read_to_string(fixture("gemini-jsonl.json")).expect("golden"))
                .expect("golden JSON");
        assert_eq!(actual, expected);
    }

    #[test]
    fn lists_fixture_sessions_under_home() {
        let home = fixture("home");
        let store = GeminiStore::with_home(&home);
        assert_eq!(store.files(&home).expect("files").len(), 2);
    }

    #[test]
    fn gemini_resume_argv_depends_on_the_binary() {
        let store = GeminiStore::with_home(".");
        let path = Path::new("session.jsonl");
        if gemini_on_path() {
            assert_eq!(
                store.resume_argv("gemini:s", path),
                Some(vec![
                    "gemini".to_owned(),
                    "--resume".to_owned(),
                    "s".to_owned(),
                ])
            );
        } else {
            assert_eq!(store.resume_argv("gemini:s", path), None);
        }
    }

    #[test]
    fn present_is_false_without_the_tmp_directory() {
        let present = GeminiLocator {
            home: fixture("home"),
        };
        assert!(present.present());
        let missing = GeminiLocator {
            home: fixture("missing-home"),
        };
        assert!(!missing.present());
    }

    #[test]
    fn home_paths_are_not_empty() {
        assert!(!gemini_home().as_os_str().is_empty());
        assert!(!home_dir().as_os_str().is_empty());
    }

    #[test]
    fn gemini_on_path_matches_the_injected_lookup() {
        assert_eq!(gemini_on_path(), gemini_in(std::env::var_os("PATH")));
    }

    fn write_gemini_stub(dir: &Path) {
        #[cfg(windows)]
        let name = "gemini.exe";
        #[cfg(not(windows))]
        let name = "gemini";
        std::fs::write(dir.join(name), b"").expect("stub");
    }

    #[test]
    fn gemini_in_finds_a_named_binary_on_the_injected_path() {
        let present = tempfile::tempdir().expect("temp dir");
        write_gemini_stub(present.path());
        assert!(gemini_in(Some(present.path().into())));
        let missing = tempfile::tempdir().expect("temp dir");
        assert!(!gemini_in(Some(missing.path().into())));
        assert!(!gemini_in(None));
        assert!(!gemini_in(Some(std::ffi::OsString::new())));
    }

    #[test]
    fn resume_argv_in_depends_on_the_injected_path() {
        let present = tempfile::tempdir().expect("temp dir");
        write_gemini_stub(present.path());
        assert_eq!(
            resume_argv_in(Some(present.path().into()), "gemini:s"),
            Some(vec![
                "gemini".to_owned(),
                "--resume".to_owned(),
                "s".to_owned(),
            ])
        );
        let missing = tempfile::tempdir().expect("temp dir");
        assert_eq!(
            resume_argv_in(Some(missing.path().into()), "gemini:s"),
            None
        );
        assert_eq!(resume_argv_in(None, "gemini:s"), None);
    }

    fn fresh_state() -> GeminiState {
        GeminiState::new(&SessionFile::new("s0", "session-s0.json"))
    }

    #[test]
    fn line_ignores_empty_session_id_and_tracks_time_bounds() {
        let mut state = fresh_state();
        state.line(&json::json!({"sessionId": ""}));
        assert_eq!(state.record.id, "gemini:s0");
        state.line(&json::json!({"sessionId": "real"}));
        assert_eq!(state.record.id, "gemini:real");
        state.put(&json::json!({
            "id": "a",
            "type": "user",
            "timestamp": "2020-01-02T00:00:02Z",
            "content": "hi"
        }));
        state.put(&json::json!({
            "id": "b",
            "type": "user",
            "timestamp": "2020-01-02T00:00:01Z"
        }));
        state.put(&json::json!({
            "id": "",
            "type": "user",
            "timestamp": "2020-01-02T00:00:00Z"
        }));
        assert_eq!(state.order, ["a", "b"]);
        let early = chrono::DateTime::parse_from_rfc3339("2020-01-02T00:00:01Z")
            .expect("rfc3339")
            .timestamp_millis();
        let late = chrono::DateTime::parse_from_rfc3339("2020-01-02T00:00:02Z")
            .expect("rfc3339")
            .timestamp_millis();
        assert_eq!(state.record.start_ms, early);
        assert_eq!(state.record.last_ms, late);
        assert_eq!(state.prompt, "hi");
    }

    #[test]
    fn rebuild_usage_skips_non_gemini_or_empty_model() {
        let mut state = fresh_state();
        state.put(&json::json!({
            "id": "g",
            "type": "gemini",
            "model": "m",
            "timestamp": "2020-01-02T00:00:00Z",
            "tokens": {"input": 4, "output": 1, "cached": 1}
        }));
        state.put(&json::json!({
            "id": "u",
            "type": "user",
            "model": "m",
            "timestamp": "2020-01-02T00:00:01Z",
            "tokens": {"input": 9, "output": 9}
        }));
        state.put(&json::json!({
            "id": "e",
            "type": "gemini",
            "model": "",
            "timestamp": "2020-01-02T00:00:02Z",
            "tokens": {"input": 9, "output": 9}
        }));
        state.rebuild_usage();
        let tokens = state.record.tokens();
        assert_eq!(tokens.input, 3);
        assert_eq!(tokens.output, 1);
        assert_eq!(state.record.models.get("m").map(|item| item.input), Some(3));
    }

    #[test]
    fn patch_removes_ids_and_patch_ids_reads_arrays() {
        assert_eq!(
            patch_ids(&json::json!({"RemoveIDs": ["a", "b"]}), "RemoveIDs"),
            ["a", "b"]
        );
        assert_eq!(
            patch_ids(&json::json!({}), "RemoveIDs"),
            Vec::<String>::new()
        );
        let mut state = fresh_state();
        state.put(&json::json!({"id": "a", "type": "user", "timestamp": "2020-01-02T00:00:00Z"}));
        state.put(&json::json!({"id": "b", "type": "user", "timestamp": "2020-01-02T00:00:01Z"}));
        state.patch(&json::json!({"RemoveIDs": ["a"]}));
        assert!(!state.messages.contains_key("a"));
        assert!(state.messages.contains_key("b"));
    }

    #[test]
    fn locator_roots_return_the_configured_home() {
        let store = GeminiStore::with_home("/tmp/gemini-home");
        assert_eq!(store.locator().roots(), [PathBuf::from("/tmp/gemini-home")]);
    }

    #[test]
    fn set_applies_before_patch_on_the_same_value() {
        let mut state = fresh_state();
        state.line(&json::json!({
            "$set": {
                "sessionId": "s1",
                "messages": [
                    {
                        "id": "a",
                        "type": "user",
                        "content": "drop",
                        "timestamp": "2020-01-02T00:00:00Z"
                    },
                    {
                        "id": "b",
                        "type": "user",
                        "content": "keep",
                        "timestamp": "2020-01-02T00:00:01Z"
                    }
                ]
            },
            "$patch": { "RemoveIDs": ["a"] }
        }));
        assert_eq!(state.session, "s1");
        assert_eq!(state.record.id, "gemini:s1");
        // $set must run first so RemoveIDs can see message "a". Patch does not
        // rewrite `order`, so "a" may remain there after the map removal.
        assert!(!state.messages.contains_key("a"));
        assert!(state.messages.contains_key("b"));
    }

    #[test]
    fn empty_set_session_id_is_ignored() {
        let mut state = fresh_state();
        state.line(&json::json!({ "$set": { "sessionId": "" } }));
        assert_eq!(state.session, "s0");
        assert_eq!(state.record.id, "gemini:s0");
        state.line(&json::json!({ "$set": { "sessionId": "real" } }));
        assert_eq!(state.session, "real");
        assert_eq!(state.record.id, "gemini:real");
    }

    #[test]
    fn line_puts_only_when_id_and_type_are_both_present() {
        let mut state = fresh_state();
        state.line(&json::json!({"id": "only-id"}));
        state.line(&json::json!({"type": "user"}));
        assert_eq!(state.order, Vec::<String>::new());
        state.line(&json::json!({
            "id": "ok",
            "type": "user",
            "content": "hi",
            "timestamp": "2020-01-02T00:00:00Z"
        }));
        assert_eq!(state.order, ["ok"]);
    }

    #[test]
    fn patch_order_ids_keep_leftovers_then_the_named_ids() {
        let mut state = fresh_state();
        for (id, content) in [("a", "one"), ("b", "two"), ("c", "three")] {
            state.put(&json::json!({
                "id": id,
                "type": "user",
                "content": content,
                "timestamp": "2020-01-02T00:00:00Z"
            }));
        }
        state.patch(&json::json!({ "OrderIDs": ["c", "a"] }));
        assert_eq!(state.order, ["b", "c", "a"]);
    }
}
