//! Antigravity CLI (`agy`) transcript JSONL and `conversation_summaries.db` parsing.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};
use hofund_plugin_api::{
    PluginError, SessionDatabase, SessionDay, SessionFile, SessionFormat, SessionLocator,
    SessionParse, SessionParser, SessionRecord, SessionStore, SessionTokens, SessionValue,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "agy";

const AGY_MODEL: &str = "agy";

/// Antigravity CLI session store.
#[derive(Clone, Debug)]
pub struct AgyStore {
    locator: AgyLocator,
}

impl AgyStore {
    /// Uses `~/.gemini/antigravity-cli`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_root(agy_dir())
    }

    /// Uses an explicit data folder for tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            locator: AgyLocator { root: root.into() },
        }
    }
}

impl Default for AgyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
struct AgyLocator {
    root: PathBuf,
}

impl SessionLocator for AgyLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("brain").is_dir() || self.root.join("conversation_summaries.db").is_file()
    }
}

impl SessionStore for AgyStore {
    fn agent(&self) -> &str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let summaries = root.join("conversation_summaries.db");
        let brain = root.join("brain");
        let Ok(entries) = std::fs::read_dir(&brain) else {
            return Ok(Vec::new());
        };
        let mut files = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !safe_id(&id) {
                continue;
            }
            let path = entry
                .path()
                .join(".system_generated/logs/transcript_full.jsonl");
            if !path.is_file() {
                continue;
            }
            let mut file = SessionFile::new(format!("{AGENT}:{id}"), path);
            if summaries.is_file() {
                file = file.with_related_databases(vec![summaries.clone()]);
            }
            files.push(file);
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
            .map(json::from_str::<AgyState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid agy parser state"))?;
        Ok(Box::new(AgyParser {
            state: restored.unwrap_or_else(|| AgyState::new(file)),
        }))
    }

    fn read_database(
        &self,
        file: &SessionFile,
        database: &dyn SessionDatabase,
    ) -> Result<Vec<SessionRecord>, PluginError> {
        if file.format != SessionFormat::Sqlite {
            return Err(PluginError::decode("unexpected agy file format"));
        }
        Ok(read_summary(file, database)?.into_iter().collect())
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id.strip_prefix(&format!("{AGENT}:"))?;
        if !safe_id(id) {
            return None;
        }
        Some(vec![
            "agy".to_owned(),
            "--conversation".to_owned(),
            id.to_owned(),
        ])
    }
}

fn agy_dir() -> PathBuf {
    std::env::var_os("AGY_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || user_home().join(".gemini/antigravity-cli"),
            PathBuf::from,
        )
}

fn user_home() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn conversation_id(file: &SessionFile) -> String {
    file.id
        .strip_prefix(&format!("{AGENT}:"))
        .unwrap_or(&file.id)
        .to_owned()
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn read_summary(
    file: &SessionFile,
    database: &dyn SessionDatabase,
) -> Result<Option<SessionRecord>, PluginError> {
    let conversation = conversation_id(file);
    let rows = database
        .query(
            "SELECT title, workspace_uris, last_modified_time FROM conversation_summaries WHERE conversation_id = ?",
            &[conversation.as_str()],
        )
        .map_err(|_| PluginError::decode("cannot read agy summary"))?;
    let Some(row) = rows.values.first() else {
        return Ok(None);
    };
    let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{conversation}"), &file.path);
    record.title = session_text(row.first());
    let workspace = session_text(row.get(1));
    if let Ok(Value::Array(uris)) = json::from_str::<Value>(&workspace) {
        for uri in uris {
            if let Some(uri) = uri.as_str()
                && let Some(folder) = agy_folder(uri)
            {
                record.cwd = folder;
                break;
            }
        }
    }
    record.start_ms = agy_time_ms(row.get(2));
    record.last_ms = record.start_ms;
    Ok(Some(record))
}

fn session_text(value: Option<&SessionValue>) -> String {
    match value {
        Some(SessionValue::Text(text)) => text.clone(),
        Some(SessionValue::Integer(value)) => value.to_string(),
        Some(SessionValue::Real(value)) => value.to_string(),
        Some(SessionValue::Blob(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

fn agy_time_ms(value: Option<&SessionValue>) -> i64 {
    match value {
        Some(SessionValue::Integer(ms)) => *ms,
        Some(SessionValue::Text(text)) => {
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(text.trim()) {
                parsed.timestamp_millis()
            } else if let Ok(parsed) =
                chrono::NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f%z")
            {
                parsed.and_utc().timestamp_millis()
            } else {
                0
            }
        }
        Some(SessionValue::Blob(bytes)) => agy_time_ms(Some(&SessionValue::Text(
            String::from_utf8_lossy(bytes).into_owned(),
        ))),
        _ => 0,
    }
}

fn agy_folder(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.len() >= 3 && rest.as_bytes().get(1) == Some(&b':') {
        rest.to_owned()
    } else if let Some(host) = rest.split('/').next()
        && !host.is_empty()
        && host != "localhost"
    {
        #[cfg(windows)]
        {
            return Some(format!(r"\\{host}{}", rest.strip_prefix(host)?).replace('/', "\\"));
        }
        #[cfg(not(windows))]
        {
            let _ = host;
            return None;
        }
    } else {
        format!("/{}", rest.trim_start_matches('/'))
    };
    #[cfg(windows)]
    if path.len() >= 3 && path.as_bytes()[0] == b'/' && path.as_bytes()[2] == b':' {
        let drive = path.as_bytes()[1].to_ascii_uppercase() as char;
        return Some(format!("{drive}:{}", &path[2..]).replace('/', "\\"));
    }
    Some(path)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct AgyState {
    record: SessionRecord,
    #[serde(default)]
    last_step: i32,
    #[serde(default)]
    steps: HashMap<i32, StepTokens>,
    #[serde(default)]
    first: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct StepTokens {
    date: String,
    tokens: SessionTokens,
}

impl AgyState {
    fn new(file: &SessionFile) -> Self {
        let id = conversation_id(file);
        let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{id}"), &file.path);
        record.title.clear();
        Self {
            record,
            last_step: -1,
            steps: HashMap::new(),
            first: String::new(),
        }
    }
}

struct AgyParser {
    state: AgyState,
}

impl AgyParser {
    fn finish_title(&mut self) {
        let fallback = self
            .state
            .record
            .id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(&self.state.record.id)
            .to_owned();
        let title = if self.state.first.is_empty() {
            fallback
        } else {
            self.state.first.clone()
        };
        self.state.record.title = title;
    }
}

impl SessionParser for AgyParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        let Some(step) = value.get("step_index").and_then(Value::as_i64) else {
            return Ok(());
        };
        let step = i32::try_from(step).unwrap_or(-1);
        let Some(created) = value.get("created_at").and_then(Value::as_str) else {
            return Ok(());
        };
        let Ok(at) = chrono::DateTime::parse_from_rfc3339(created) else {
            return Ok(());
        };
        let ms = at.timestamp_millis();
        let source = value.get("source").and_then(Value::as_str).unwrap_or("");
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");

        if source == "MODEL" && kind == "PLANNER_RESPONSE" {
            let input = value
                .get("input_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let output = value
                .get("output_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let cache_read = value
                .get("cache_read_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if input > 0 || output > 0 || cache_read > 0 {
                let tokens = SessionTokens::new(
                    u64::try_from(input.max(0)).unwrap_or(0),
                    u64::try_from(output.max(0)).unwrap_or(0),
                    u64::try_from(cache_read.max(0)).unwrap_or(0),
                    0,
                );
                let date = local_date(ms);
                if let Some(previous) = self.state.steps.get(&step).cloned() {
                    self.drop_day(&previous.date, &previous.tokens);
                }
                if tokens.is_zero() {
                    self.state.steps.remove(&step);
                } else {
                    self.state.steps.insert(
                        step,
                        StepTokens {
                            date: date.clone(),
                            tokens,
                        },
                    );
                    self.state.record.add_usage(AGY_MODEL, tokens);
                    self.state
                        .record
                        .add_day(SessionDay::new(date, AGY_MODEL, tokens));
                }
            }
        }

        if step <= self.state.last_step {
            return Ok(());
        }
        self.state.last_step = step;
        self.saw(ms);

        if source == "USER_EXPLICIT"
            && kind == "USER_INPUT"
            && (self.state.record.title.is_empty() || self.state.first.is_empty())
            && let Some(content) = value.get("content").and_then(Value::as_str)
        {
            let request = agy_request(content);
            if !request.is_empty() && self.state.record.title.is_empty() {
                self.state.first = title_line(&request);
            }
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::decode("cannot serialize agy state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

impl AgyParser {
    fn saw(&mut self, ms: i64) {
        if ms <= 0 {
            return;
        }
        if self.state.record.start_ms == 0 || ms < self.state.record.start_ms {
            self.state.record.start_ms = ms;
        }
        self.state.record.last_ms = self.state.record.last_ms.max(ms);
    }

    fn drop_day(&mut self, date: &str, tokens: &SessionTokens) {
        if date.is_empty() {
            return;
        }
        if let Some(day) = self
            .state
            .record
            .days
            .iter_mut()
            .find(|day| day.date == date && day.model == AGY_MODEL)
        {
            day.tokens.input = day.tokens.input.saturating_sub(tokens.input);
            day.tokens.output = day.tokens.output.saturating_sub(tokens.output);
            day.tokens.cache_read = day.tokens.cache_read.saturating_sub(tokens.cache_read);
        }
    }
}

fn agy_request(content: &str) -> String {
    let Some((_, rest)) = content.split_once("<USER_REQUEST>") else {
        return content.trim().to_owned();
    };
    let Some((request, _)) = rest.split_once("</USER_REQUEST>") else {
        return rest.trim().to_owned();
    };
    request.trim().to_owned()
}

fn local_date(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|at| at.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
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

    fn parse_transcript(store: &AgyStore, file: &SessionFile) -> SessionRecord {
        let mut parser = store.parser(file, None).expect("parser");
        let body = std::fs::read_to_string(&file.path).expect("read");
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let value: Value = json::from_str(line).expect("json");
            parser.push(&value).expect("push");
        }
        Box::new(parser)
            .finish()
            .expect("finish")
            .record
            .expect("record")
    }

    #[test]
    fn lists_agy_sessions_from_golden_fixtures() {
        let root = fixture_root();
        let store = AgyStore::with_root(root.clone());
        let files = store.files(&root).expect("files");
        assert_eq!(files.len(), 3);
        assert!(
            files
                .iter()
                .all(|file| file.related_databases == [root.join("conversation_summaries.db")])
        );
        let two = files
            .iter()
            .find(|f| f.id.ends_with("9c462bce-9709-47a1-9655-4ddd39f54270"))
            .expect("two-turn conversation");
        let record = parse_transcript(&store, two);
        assert_eq!(record.title, "again");
        let tokens = record.models.get(AGY_MODEL).copied().unwrap_or_default();
        assert_eq!(tokens.input, 1404);
        assert_eq!(tokens.output, 18);
        assert_eq!(tokens.cache_read, 600);
        let daily = record
            .days
            .iter()
            .find(|day| day.model == AGY_MODEL)
            .expect("daily usage");
        assert_eq!(daily.tokens, tokens);
        let nr = files
            .iter()
            .find(|f| f.id.ends_with("1fcab41f-d57c-4ef4-b4d3-6bcdb84853e5"))
            .expect("no row");
        let record = parse_transcript(&store, nr);
        assert_eq!(record.title, "what is in this folder");
    }

    #[test]
    fn summary_uses_kernel_database_rows() {
        struct SummaryDb;

        impl SessionDatabase for SummaryDb {
            fn query(
                &self,
                sql: &str,
                params: &[&str],
            ) -> Result<hofund_plugin_api::SessionRows, PluginError> {
                assert!(sql.contains("WHERE conversation_id = ?"));
                assert_eq!(params, ["id-1"]);
                Ok(hofund_plugin_api::SessionRows::new(
                    vec![vec![
                        SessionValue::Text("Saved title".into()),
                        SessionValue::Text(r#"["file:///work/proj"]"#.into()),
                        SessionValue::Text("2026-10-25T09:49:15Z".into()),
                    ]],
                    false,
                ))
            }

            fn revision(&self) -> Result<i64, PluginError> {
                Ok(1)
            }
        }

        let file = SessionFile::new("agy:id-1", "conversation_summaries.db")
            .with_format(SessionFormat::Sqlite);
        let summary = read_summary(&file, &SummaryDb)
            .expect("summary")
            .expect("row");
        assert_eq!(summary.id, "agy:id-1");
        assert_eq!(summary.title, "Saved title");
        assert_eq!(summary.cwd, "/work/proj");
        assert!(summary.last_ms > 0);
    }
}
