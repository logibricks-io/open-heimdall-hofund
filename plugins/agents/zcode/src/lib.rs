//! `ZCode` CLI session discovery and read-only OpenCode-shaped SQLite parsing.
//!
//! The kernel opens the database read-only; this crate owns `ZCode` table layout
//! and token semantics (`~/.zcode/cli/db/db.sqlite`).

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
pub const AGENT: &str = "zcode";

/// `ZCode`'s SQLite session store.
#[derive(Clone, Debug)]
pub struct ZCodeStore {
    locator: ZCodeLocator,
    database: PathBuf,
}

impl ZCodeStore {
    /// Uses `~/.zcode` and `cli/db/db.sqlite` under the OS home directory.
    #[must_use]
    pub fn new() -> Self {
        let root = zcode_home();
        let database = zcode_database(&root);
        Self {
            locator: ZCodeLocator { root },
            database,
        }
    }

    /// Uses an explicit `ZCode` home tree (expects `cli/db/db.sqlite` beneath it).
    #[must_use]
    pub fn with_home(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let database = zcode_database(&root);
        Self {
            locator: ZCodeLocator { root },
            database,
        }
    }
}

fn zcode_home() -> PathBuf {
    home_dir().join(".zcode")
}

fn zcode_database(root: &Path) -> PathBuf {
    root.join("cli").join("db").join("db.sqlite")
}

impl Default for ZCodeStore {
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

/// Directory-only `ZCode` locator.
#[derive(Clone, Debug)]
pub struct ZCodeLocator {
    root: PathBuf,
}

impl SessionLocator for ZCodeLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        zcode_database(&self.root).is_file()
    }
}

impl SessionStore for ZCodeStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, _root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        if !self.database.is_file() {
            return Ok(Vec::new());
        }
        Ok(vec![
            SessionFile::new("database", &self.database).with_format(SessionFormat::Sqlite),
        ])
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        let restored = state
            .map(json::from_str::<ZCodeState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid ZCode parser state"))?;
        Ok(Box::new(ZCodeParser {
            state: restored.unwrap_or_else(|| ZCodeState::new(file)),
        }))
    }

    fn read_database(
        &self,
        file: &SessionFile,
        database: &dyn SessionDatabase,
    ) -> Result<Vec<SessionRecord>, PluginError> {
        read_database(file, database)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct ZCodeState {
    record: SessionRecord,
    named: String,
    prompt: String,
    first_user: String,
}

impl ZCodeState {
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

struct ZCodeParser {
    state: ZCodeState,
}

impl ZCodeParser {
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
        if let Some(completed) = value.pointer("/time/completed").and_then(Value::as_i64) {
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
        if let Some(date) = local_date(created) {
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

impl SessionParser for ZCodeParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        self.line(value);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<SessionParse, PluginError> {
        self.state.finish_title();
        let state = json::to_string(&self.state)
            .map_err(|_| PluginError::internal("cannot save ZCode parser state"))?;
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
    if has_v2 {
        infos.extend(read_infos(database, true, false)?);
    }
    if has_legacy && (!has_v2 || !migrated) {
        infos.extend(read_infos(database, false, has_v2)?);
    }
    let mut records = Vec::new();
    let mut parents = BTreeMap::new();
    for info in infos {
        parents.insert(info.id.clone(), info.parent.clone());
        let source = SessionFile::new(&info.id, &file.path);
        let mut parser = ZCodeParser {
            state: ZCodeState::new(&source),
        };
        parser.info(&info.value);
        let v2 = info
            .value
            .get("v2")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if v2 {
            read_v2_messages(database, &info.id, &mut parser)?;
        } else {
            read_v1_messages(database, &info.id, &mut parser)?;
        }
        parser.state.finish_title();
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
) -> Result<Vec<Info>, PluginError> {
    let sql = match (v2, exclude_v2) {
        (true, _) => {
            "SELECT id, COALESCE(parent_id, ''), directory, COALESCE(title, ''), time_created, time_updated FROM session_v2"
        }
        (false, true) => {
            "SELECT id, COALESCE(parent_id, ''), directory, COALESCE(title, ''), COALESCE(title_source, ''), time_created, time_updated FROM session WHERE id NOT IN (SELECT id FROM session_v2)"
        }
        (false, false) => {
            "SELECT id, COALESCE(parent_id, ''), directory, COALESCE(title, ''), COALESCE(title_source, ''), time_created, time_updated FROM session"
        }
    };
    let rows = database.query(sql, &[])?;
    Ok(rows
        .values
        .into_iter()
        .filter_map(|row| {
            let (id, parent, directory, title, created, updated) = if v2 {
                if row.len() < 6 {
                    return None;
                }
                (
                    text(&row[0])?.to_owned(),
                    text(&row[1]).unwrap_or("").to_owned(),
                    text(&row[2]).unwrap_or("").to_owned(),
                    text(&row[3]).unwrap_or("").to_owned(),
                    integer(&row[4]).unwrap_or(0),
                    integer(&row[5]).unwrap_or(0),
                )
            } else if row.len() < 7 {
                return None;
            } else {
                let title_source = text(&row[4]).unwrap_or("");
                let mut title = text(&row[3]).unwrap_or("").to_owned();
                if title_source == "default" {
                    title.clear();
                }
                (
                    text(&row[0])?.to_owned(),
                    text(&row[1]).unwrap_or("").to_owned(),
                    text(&row[2]).unwrap_or("").to_owned(),
                    title,
                    integer(&row[5]).unwrap_or(0),
                    integer(&row[6]).unwrap_or(0),
                )
            };
            let value = json!({
                "id": id,
                "parentID": parent,
                "directory": directory,
                "title": title,
                "time": {"created": created, "updated": updated},
                "v2": v2,
            });
            Some(Info { id, parent, value })
        })
        .collect())
}

fn read_v1_messages(
    database: &dyn SessionDatabase,
    id: &str,
    parser: &mut ZCodeParser,
) -> Result<(), PluginError> {
    if !has_table(database, "message")? {
        return Ok(());
    }
    let rows = database.query("SELECT data FROM message WHERE session_id = ?", &[id])?;
    for value in rows
        .values
        .iter()
        .filter_map(|row| row.first())
        .filter_map(json_value)
    {
        if message_copied(database, &value)? {
            continue;
        }
        parser.message(&value);
    }
    if !parser.state.first_user.is_empty() && has_table(database, "part")? {
        let parts = database.query(
            "SELECT data FROM part WHERE message_id = ? ORDER BY id",
            &[&parser.state.first_user],
        )?;
        for part in parts
            .values
            .iter()
            .filter_map(|row| row.first())
            .filter_map(json_value)
        {
            parser.part(&part);
        }
    }
    Ok(())
}

fn read_v2_messages(
    database: &dyn SessionDatabase,
    id: &str,
    parser: &mut ZCodeParser,
) -> Result<(), PluginError> {
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
    Ok(())
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

fn message_copied(database: &dyn SessionDatabase, value: &Value) -> Result<bool, PluginError> {
    let Some(origin) = value
        .pointer("/metadata/forkOrigin/messageId")
        .and_then(Value::as_str)
    else {
        return Ok(false);
    };
    let msg_id = value.get("id").and_then(Value::as_str).unwrap_or("");
    if origin.is_empty() || origin == msg_id {
        return Ok(false);
    }
    let rows = database.query("SELECT COUNT(*) FROM message WHERE id = ?", &[origin])?;
    Ok(rows
        .values
        .first()
        .and_then(|row| row.first())
        .and_then(integer)
        .unwrap_or(0)
        > 0)
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
    use super::*;

    #[test]
    fn lists_sqlite_database_file() {
        let home = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root");
        let store = ZCodeStore::with_home(home);
        let files = store.files(&store.locator().roots()[0]).expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].format, SessionFormat::Sqlite);
    }
}
