//! `OpenCode` session discovery and SQLite/JSON parsing.
//!
//! The kernel supplies a consented root, bounded files, and a read-only
//! database connection. This crate owns `OpenCode`'s schema and token semantics.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};
use hofund_plugin_api::{
    PluginError, SessionDatabase, SessionDay, SessionFile, SessionFormat, SessionLocator,
    SessionParse, SessionParser, SessionRecord, SessionStore, SessionTokens, SessionValue,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::{Value, json};
use serde::{Deserialize, Serialize};

/// The session agent id.
pub const AGENT: &str = "opencode";

/// `OpenCode`'s SQLite-or-JSON session store.
#[derive(Clone, Debug)]
pub struct OpenCodeStore {
    locator: OpenCodeLocator,
    database: PathBuf,
}

impl OpenCodeStore {
    /// Uses the XDG data home and `$OPENCODE_DB` when configured.
    #[must_use]
    pub fn new() -> Self {
        let root = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(
                || home_dir().join(".local").join("share").join("opencode"),
                |value| PathBuf::from(value).join("opencode"),
            );
        let database = std::env::var_os("OPENCODE_DB")
            .filter(|value| !value.is_empty() && *value != ":memory:")
            .map(PathBuf::from)
            .map_or_else(
                || root.join("opencode.db"),
                |path| {
                    if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    }
                },
            );
        Self::from_parts(root, database)
    }

    /// Uses an explicit data root and its `opencode.db`.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let database = root.join("opencode.db");
        Self::from_parts(root, database)
    }

    fn from_parts(root: PathBuf, database: PathBuf) -> Self {
        Self {
            locator: OpenCodeLocator {
                root,
                database: database.clone(),
            },
            database,
        }
    }
}

impl Default for OpenCodeStore {
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

/// Directory-only `OpenCode` locator.
#[derive(Clone, Debug)]
pub struct OpenCodeLocator {
    root: PathBuf,
    database: PathBuf,
}

impl SessionLocator for OpenCodeLocator {
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        if let Some(parent) = self.database.parent()
            && parent != self.root
        {
            roots.push(parent.to_path_buf());
        }
        roots
    }

    fn present(&self) -> bool {
        self.root.is_dir() || self.database.is_file()
    }
}

impl SessionStore for OpenCodeStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        if self.database.is_file() && self.database.parent().is_some_and(|parent| parent == root) {
            return Ok(vec![
                SessionFile::new("database", &self.database).with_format(SessionFormat::Sqlite),
            ]);
        }
        let storage = root.join("storage");
        let mut files = Vec::new();
        for project in entries(&storage.join("session"))? {
            if !project
                .file_type()
                .map_err(|_| PluginError::io("cannot list OpenCode sessions"))?
                .is_dir()
            {
                continue;
            }
            for session in entries(&project.path())? {
                let path = session.path();
                if path.extension().is_none_or(|extension| extension != "json") {
                    continue;
                }
                let Some(id) = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                let messages = json_files(&storage.join("message").join(&id))?;
                let mut related = messages.clone();
                for message in messages {
                    if let Some(message_id) = message.file_stem().and_then(|stem| stem.to_str()) {
                        related.extend(json_files(&storage.join("part").join(message_id))?);
                    }
                }
                related.sort();
                related.dedup();
                files.push(
                    SessionFile::new(id, path)
                        .with_format(SessionFormat::Json)
                        .with_related(related),
                );
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
            .map(json::from_str::<OpenCodeState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid OpenCode parser state"))?;
        Ok(Box::new(OpenCodeParser {
            state: restored.unwrap_or_else(|| OpenCodeState::new(file)),
        }))
    }

    fn read_database(
        &self,
        file: &SessionFile,
        database: &dyn SessionDatabase,
    ) -> Result<Vec<SessionRecord>, PluginError> {
        read_database(file, database)
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id
            .strip_prefix(&format!("{AGENT}:"))
            .unwrap_or(session_id);
        Some(vec![
            "opencode".to_owned(),
            "--session".to_owned(),
            id.to_owned(),
        ])
    }
}

fn entries(path: &Path) -> Result<Vec<std::fs::DirEntry>, PluginError> {
    let Ok(directory) = std::fs::read_dir(path) else {
        return Ok(Vec::new());
    };
    let mut entries = directory
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| PluginError::io("cannot list OpenCode sessions"))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn json_files(path: &Path) -> Result<Vec<PathBuf>, PluginError> {
    Ok(entries(path)?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct OpenCodeState {
    record: SessionRecord,
    named: String,
    prompt: String,
    first_user: String,
}

impl OpenCodeState {
    fn new(file: &SessionFile) -> Self {
        let id = file.id.trim_start_matches(&format!("{AGENT}:"));
        let mut record = SessionRecord::new(AGENT, format!("{AGENT}:{id}"), &file.path);
        id.clone_into(&mut record.title);
        Self {
            record,
            named: String::new(),
            prompt: String::new(),
            first_user: String::new(),
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

struct OpenCodeParser {
    state: OpenCodeState,
}

impl OpenCodeParser {
    fn line(&mut self, value: &Value) {
        if value.get("role").and_then(Value::as_str).is_some() {
            self.message(value);
        } else if value.get("type").and_then(Value::as_str) == Some("text") {
            self.part(value);
        } else {
            self.info(value);
        }
        self.state.finish_title();
    }

    fn info(&mut self, value: &Value) {
        let Some(id) = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        self.state.record.id = format!("{AGENT}:{id}");
        if let Some(cwd) = value.get("directory").and_then(Value::as_str) {
            cwd.clone_into(&mut self.state.record.cwd);
        }
        if let Some(title) = value.get("title").and_then(Value::as_str) {
            let title = title_line(title);
            if !title.starts_with("New session - ") && !title.starts_with("Child session - ") {
                self.state.named = title;
            }
        }
        let created = value
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let updated = value
            .pointer("/time/updated")
            .and_then(Value::as_i64)
            .unwrap_or(created);
        self.saw(created);
        self.saw(updated);
        let parent = value
            .get("parentID")
            .or_else(|| value.get("parent_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !parent.is_empty() {
            self.state.record = self
                .state
                .record
                .clone()
                .with_parent(format!("{AGENT}:{parent}"));
        }
    }

    fn message(&mut self, value: &Value) {
        let created = value
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.saw(created);
        let role = value.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "user" {
            if self.state.first_user.is_empty()
                && value
                    .pointer("/semantics/origin")
                    .and_then(Value::as_str)
                    .is_none_or(|origin| origin.is_empty() || origin == "real_user")
                && let Some(id) = value.get("id").and_then(Value::as_str)
            {
                id.clone_into(&mut self.state.first_user);
            }
            return;
        }
        if role != "assistant" {
            return;
        }
        let completed = value.pointer("/time/completed").and_then(Value::as_i64);
        if let Some(completed) = completed {
            self.saw(completed);
        }
        if self.state.record.cwd.is_empty()
            && let Some(cwd) = value.pointer("/path/cwd").and_then(Value::as_str)
        {
            cwd.clone_into(&mut self.state.record.cwd);
        }
        let model = value
            .get("modelID")
            .or_else(|| value.get("modelId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(tokens) = value.get("tokens") else {
            return;
        };
        let tokens = SessionTokens::new(
            counter(tokens, "input"),
            counter(tokens, "output").saturating_add(counter(tokens, "reasoning")),
            tokens
                .pointer("/cache/read")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            tokens
                .pointer("/cache/write")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        if model.is_empty() || tokens.is_zero() {
            return;
        }
        self.state.record.add_usage(model, tokens);
        if let Some(date) = local_date(completed.unwrap_or(created)) {
            self.state
                .record
                .add_day(SessionDay::new(date, model, tokens));
        }
    }

    fn part(&mut self, value: &Value) {
        if self.state.first_user.is_empty()
            || value.get("messageID").and_then(Value::as_str) != Some(&self.state.first_user)
            || value
                .get("ignored")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || value
                .get("synthetic")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            return;
        }
        let Some(text) = value.get("text").and_then(Value::as_str) else {
            return;
        };
        let trimmed = text.trim();
        if self.state.prompt.is_empty() && !trimmed.is_empty() && !trimmed.starts_with('<') {
            self.state.prompt = title_line(trimmed);
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

impl SessionParser for OpenCodeParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save OpenCode parser state"))?;
        Ok(SessionParse::new(self.state.record, state))
    }
}

#[derive(Debug)]
struct Info {
    id: String,
    parent: String,
    value: Value,
}

fn read_database(
    file: &SessionFile,
    database: &dyn SessionDatabase,
) -> Result<Vec<SessionRecord>, PluginError> {
    let has_v2 = has_table(database, "session_v2")?;
    let has_legacy = has_table(database, "session")?;
    let migrated = has_v2 && has_table(database, "kv")? && migrated(database)?;
    let mut infos = Vec::new();
    let mut infos_cut = false;
    if has_v2 {
        let (rows, cut) = read_infos(database, true, false)?;
        infos_cut |= cut;
        infos.extend(rows);
    }
    if has_legacy && (!has_v2 || !migrated) {
        let (rows, cut) = read_infos(database, false, has_v2)?;
        infos_cut |= cut;
        infos.extend(rows);
    }
    let mut records = Vec::new();
    let mut parents = BTreeMap::new();
    for info in infos {
        parents.insert(info.id.clone(), info.parent.clone());
        let source = SessionFile::new(&info.id, &file.path);
        let mut parser = OpenCodeParser {
            state: OpenCodeState::new(&source),
        };
        parser.info(&info.value);
        let v2 = info
            .value
            .get("v2")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let messages_cut = if v2 {
            read_v2_messages(database, &info.id, &mut parser)?
        } else {
            read_v1_messages(database, &info.id, &mut parser)?
        };
        parser.state.finish_title();
        parser.state.record.cut |= infos_cut || messages_cut;
        records.push(parser.state.record);
    }
    for record in &mut records {
        let Some(parent) = record.parent_id.clone() else {
            continue;
        };
        let mut root = parent.trim_start_matches(&format!("{AGENT}:")).to_owned();
        for _ in 0..64 {
            let Some(next) = parents.get(&root).filter(|next| !next.is_empty()) else {
                break;
            };
            root.clone_from(next);
        }
        record.parent_id = Some(format!("{AGENT}:{root}"));
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(records)
}

fn has_table(database: &dyn SessionDatabase, name: &str) -> Result<bool, PluginError> {
    let rows = database.query(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        &[name],
    )?;
    Ok(rows
        .values
        .first()
        .and_then(|row| row.first())
        .and_then(integer)
        .unwrap_or(0)
        > 0)
}

fn migrated(database: &dyn SessionDatabase) -> Result<bool, PluginError> {
    let rows = database.query("SELECT value FROM kv WHERE key = 'migration.v1-v2'", &[])?;
    Ok(rows
        .values
        .first()
        .and_then(|row| row.first())
        .and_then(json_value)
        .and_then(|value| {
            value
                .get("phase")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("completed"))
}

fn read_infos(
    database: &dyn SessionDatabase,
    v2: bool,
    exclude_v2: bool,
) -> Result<(Vec<Info>, bool), PluginError> {
    let sql = match (v2, exclude_v2) {
        (true, _) => {
            "SELECT id, COALESCE(parent_id, ''), directory, COALESCE(title, ''), time_created, time_updated FROM session_v2"
        }
        (false, true) => {
            "SELECT id, COALESCE(parent_id, ''), directory, COALESCE(title, ''), time_created, time_updated FROM session WHERE id NOT IN (SELECT id FROM session_v2)"
        }
        (false, false) => {
            "SELECT id, COALESCE(parent_id, ''), directory, COALESCE(title, ''), time_created, time_updated FROM session"
        }
    };
    let rows = database.query(sql, &[])?;
    let infos = rows
        .values
        .into_iter()
        .filter_map(|row| {
            if row.len() < 6 {
                return None;
            }
            let id = text(&row[0])?.to_owned();
            let parent = text(&row[1]).unwrap_or("").to_owned();
            let value = json!({
                "id": id,
                "parentID": parent,
                "directory": text(&row[2]).unwrap_or(""),
                "title": text(&row[3]).unwrap_or(""),
                "time": {"created": integer(&row[4]).unwrap_or(0), "updated": integer(&row[5]).unwrap_or(0)},
                "v2": v2,
            });
            Some(Info { id, parent, value })
        })
        .collect();
    Ok((infos, rows.cut))
}

fn read_v1_messages(
    database: &dyn SessionDatabase,
    id: &str,
    parser: &mut OpenCodeParser,
) -> Result<bool, PluginError> {
    if !has_table(database, "message")? {
        return Ok(false);
    }
    let rows = database.query("SELECT data FROM message WHERE session_id = ?", &[id])?;
    for value in rows
        .values
        .iter()
        .filter_map(|row| row.first())
        .filter_map(json_value)
    {
        parser.message(&value);
    }
    let mut cut = rows.cut;
    if !parser.state.first_user.is_empty() && has_table(database, "part")? {
        let parts = database.query(
            "SELECT data FROM part WHERE message_id = ? ORDER BY id",
            &[&parser.state.first_user],
        )?;
        cut |= parts.cut;
        for part in parts
            .values
            .iter()
            .filter_map(|row| row.first())
            .filter_map(json_value)
        {
            parser.part(&part);
        }
    }
    Ok(cut)
}

fn read_v2_messages(
    database: &dyn SessionDatabase,
    id: &str,
    parser: &mut OpenCodeParser,
) -> Result<bool, PluginError> {
    let rows = database.query(
        "SELECT id, type, data FROM session_message WHERE session_id = ? AND type IN ('user', 'assistant', 'compaction', 'skill') ORDER BY seq",
        &[id],
    )?;
    let mut last_model = String::new();
    for row in &rows.values {
        if row.len() < 3 {
            continue;
        }
        let Some(message_id) = text(&row[0]) else {
            continue;
        };
        let kind = text(&row[1]).unwrap_or("");
        let Some(data) = json_value(&row[2]) else {
            continue;
        };
        let mut message = data.clone();
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        object.insert("id".to_owned(), Value::String(message_id.to_owned()));
        object.insert(
            "role".to_owned(),
            Value::String(if kind == "user" { "user" } else { "assistant" }.to_owned()),
        );
        let model = data
            .pointer("/model/id")
            .and_then(Value::as_str)
            .unwrap_or(&last_model)
            .to_owned();
        if kind == "assistant" && !model.is_empty() {
            last_model.clone_from(&model);
        }
        if kind == "user" || kind == "skill" {
            object.insert("modelID".to_owned(), Value::String(String::new()));
            object.remove("tokens");
        } else {
            object.insert("modelID".to_owned(), Value::String(model));
        }
        parser.message(&message);
        if kind == "user"
            && let Some(text) = data.get("text").and_then(Value::as_str)
        {
            parser.part(&json!({"type":"text", "text":text, "messageID":message_id}));
        }
    }
    Ok(rows.cut)
}

fn text(value: &SessionValue) -> Option<&str> {
    match value {
        SessionValue::Text(value) => Some(value),
        _ => None,
    }
}

fn integer(value: &SessionValue) -> Option<i64> {
    match value {
        SessionValue::Integer(value) => Some(*value),
        _ => None,
    }
}

fn json_value(value: &SessionValue) -> Option<Value> {
    match value {
        SessionValue::Text(value) => json::from_str(value).ok(),
        SessionValue::Blob(value) => json::from_slice(value).ok(),
        _ => None,
    }
}

fn counter(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
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
    use super::{AGENT, OpenCodeStore, read_database};
    use hofund_plugin_api::{
        PluginError, SessionDatabase, SessionFile, SessionFormat, SessionRows, SessionStore,
        SessionValue,
    };
    use hofund_plugin_sdk::{
        chrono::{Local, TimeZone},
        json,
    };

    struct CutDatabase {
        infos: SessionRows,
    }

    impl SessionDatabase for CutDatabase {
        fn query(&self, sql: &str, params: &[&str]) -> Result<SessionRows, PluginError> {
            let normalized = sql.to_ascii_lowercase();
            if normalized.contains("sqlite_master") {
                let found = i64::from(params.first().copied() == Some("session"));
                return Ok(SessionRows::new(
                    vec![vec![SessionValue::Integer(found)]],
                    false,
                ));
            }
            if normalized.contains("from session_v2") {
                return Ok(SessionRows::new(Vec::new(), false));
            }
            if normalized.contains("from session") {
                return Ok(self.infos.clone());
            }
            Ok(SessionRows::new(Vec::new(), false))
        }

        fn revision(&self) -> Result<i64, PluginError> {
            Ok(1)
        }
    }

    struct DailyUsageDatabase {
        v2: bool,
        created: i64,
        completed: i64,
    }

    impl DailyUsageDatabase {
        fn infos(&self) -> SessionRows {
            SessionRows::new(
                vec![vec![
                    SessionValue::Text("ses_one".to_owned()),
                    SessionValue::Text(String::new()),
                    SessionValue::Text("/work".to_owned()),
                    SessionValue::Text("one".to_owned()),
                    SessionValue::Integer(self.created),
                    SessionValue::Integer(self.completed),
                ]],
                false,
            )
        }

        fn message(&self) -> String {
            let time = json::json!({"created": self.created, "completed": self.completed});
            let tokens = json::json!({"input": 5, "output": 1});
            let data = if self.v2 {
                json::json!({"time": time, "model": {"id": "m"}, "tokens": tokens})
            } else {
                json::json!({
                    "role": "assistant",
                    "modelID": "m",
                    "time": time,
                    "tokens": tokens
                })
            };
            json::to_string(&data).expect("message JSON")
        }
    }

    impl SessionDatabase for DailyUsageDatabase {
        fn query(&self, sql: &str, params: &[&str]) -> Result<SessionRows, PluginError> {
            let normalized = sql.to_ascii_lowercase();
            if normalized.contains("sqlite_master") {
                let table = params.first().copied().unwrap_or("");
                let found = if self.v2 {
                    table == "session_v2"
                } else {
                    matches!(table, "session" | "message")
                };
                return Ok(SessionRows::new(
                    vec![vec![SessionValue::Integer(i64::from(found))]],
                    false,
                ));
            }
            if normalized.contains("from session_message") && self.v2 {
                return Ok(SessionRows::new(
                    vec![vec![
                        SessionValue::Text("msg_one".to_owned()),
                        SessionValue::Text("assistant".to_owned()),
                        SessionValue::Text(self.message()),
                    ]],
                    false,
                ));
            }
            if normalized.contains("from session_v2") || normalized.contains("from session") {
                return Ok(self.infos());
            }
            if normalized.contains("from message") && !self.v2 {
                return Ok(SessionRows::new(
                    vec![vec![SessionValue::Text(self.message())]],
                    false,
                ));
            }
            Ok(SessionRows::new(Vec::new(), false))
        }

        fn revision(&self) -> Result<i64, PluginError> {
            Ok(1)
        }
    }

    #[test]
    fn database_usage_uses_completion_day_for_v1_and_v2() {
        let created = Local
            .with_ymd_and_hms(2026, 1, 1, 23, 59, 59)
            .single()
            .expect("created local time")
            .timestamp_millis();
        let completed = Local
            .with_ymd_and_hms(2026, 1, 2, 0, 0, 1)
            .single()
            .expect("completed local time")
            .timestamp_millis();
        let file = SessionFile::new("database", "opencode.db").with_format(SessionFormat::Sqlite);
        for v2 in [false, true] {
            let database = DailyUsageDatabase {
                v2,
                created,
                completed,
            };
            let records = read_database(&file, &database).expect("records");
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].days.len(), 1);
            assert_eq!(records[0].days[0].date, "2026-01-02");
            assert_eq!(records[0].days[0].tokens.input, 5);
            assert_eq!(records[0].days[0].tokens.output, 1);
        }
    }

    #[test]
    fn truncated_session_queries_mark_records_cut() {
        let infos = SessionRows::new(
            vec![vec![
                SessionValue::Text("ses_one".to_owned()),
                SessionValue::Text(String::new()),
                SessionValue::Text("/work".to_owned()),
                SessionValue::Text("one".to_owned()),
                SessionValue::Integer(1),
                SessionValue::Integer(2),
            ]],
            true,
        );
        let file = SessionFile::new("database", "opencode.db").with_format(SessionFormat::Sqlite);
        let records = read_database(&file, &CutDatabase { infos }).expect("records");
        assert_eq!(records.len(), 1);
        assert!(
            records[0].cut,
            "truncated info query should mark the record cut"
        );
        assert_eq!(records[0].id, format!("{AGENT}:ses_one"));
    }

    #[test]
    fn absolute_and_relative_database_overrides_are_listed_under_their_parent_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data = temp.path().join("xdg").join("opencode");
        std::fs::create_dir_all(&data).expect("data");
        let other = temp.path().join("other");
        std::fs::create_dir_all(&other).expect("other");
        let absolute = other.join("custom.db");
        std::fs::write(&absolute, b"").expect("absolute db");
        let store = OpenCodeStore::from_parts(data.clone(), absolute.clone());
        let roots = store.locator().roots();
        assert!(roots.contains(&data));
        assert!(roots.contains(&other));
        let listed = store.files(&other).expect("absolute listing");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, absolute);
        assert_eq!(listed[0].format, SessionFormat::Sqlite);
        assert_eq!(
            store.files(&data).expect("default root"),
            Vec::<SessionFile>::new()
        );

        let relative = data.join("nested").join("rel.db");
        std::fs::create_dir_all(relative.parent().expect("parent")).expect("nested");
        std::fs::write(&relative, b"").expect("relative db");
        let store = OpenCodeStore::from_parts(data.clone(), relative.clone());
        let parent = relative.parent().expect("parent").to_path_buf();
        assert!(store.locator().roots().contains(&parent));
        let listed = store.files(&parent).expect("relative listing");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, relative);
    }
}
