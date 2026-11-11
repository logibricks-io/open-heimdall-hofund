//! Cursor session discovery and parsing for CLI chats and editor transcripts.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hofund_plugin_api::{
    PluginError, SessionDatabase, SessionDay, SessionFile, SessionFormat, SessionLocator,
    SessionParse, SessionParser, SessionRecord, SessionStore, SessionTokens, SessionValue,
};
use hofund_plugin_sdk::{chrono, json, serde};
use json::Value;
use regex::Regex;

/// The session agent id.
pub const AGENT: &str = "cursor";

/// Cursor CLI and editor session store.
#[derive(Clone, Debug)]
pub struct CursorStore {
    locator: CursorLocator,
    data_root: PathBuf,
}

impl CursorStore {
    /// Uses environment overrides and default Cursor directories.
    #[must_use]
    pub fn new() -> Self {
        let root = cursor_config_dir();
        let data_root = cursor_data_dir();
        Self::with_roots(root, data_root)
    }

    /// Uses explicit roots, for tests.
    #[must_use]
    pub fn with_roots(cli_root: impl Into<PathBuf>, data_root: impl Into<PathBuf>) -> Self {
        Self {
            locator: CursorLocator {
                root: cli_root.into(),
            },
            data_root: data_root.into(),
        }
    }

    /// Uses one tree that holds both CLI chats and project transcripts.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self::with_roots(root.clone(), root)
    }
}

impl Default for CursorStore {
    fn default() -> Self {
        Self::new()
    }
}

/// CLI config directory locator.
#[derive(Clone, Debug)]
pub struct CursorLocator {
    root: PathBuf,
}

impl SessionLocator for CursorLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("chats").is_dir()
    }
}

impl SessionStore for CursorStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let mut files = cli_chat_files(root)?;
        let cli_ids: BTreeSet<String> = files.iter().map(|file| file.id.clone()).collect();
        files.extend(editor_transcript_files(&self.data_root, &cli_ids)?);
        Ok(files)
    }

    fn parser(
        &self,
        file: &SessionFile,
        state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        if file.format != SessionFormat::JsonLines {
            return Err(PluginError::decode("unexpected cursor parser format"));
        }
        let restored = state
            .map(json::from_str::<TranscriptState>)
            .transpose()
            .map_err(|_| PluginError::decode("invalid cursor transcript parser state"))?;
        Ok(Box::new(TranscriptParser {
            state: restored.unwrap_or_else(|| TranscriptState::new(file)),
        }))
    }

    fn read_database(
        &self,
        file: &SessionFile,
        database: &dyn SessionDatabase,
    ) -> Result<Vec<SessionRecord>, PluginError> {
        let chat_dir = file
            .path
            .parent()
            .ok_or_else(|| PluginError::decode("cursor chat path has no parent"))?;
        let chat_id = file.id.clone();
        let sidecar = chat_meta_sidecar(file);
        let record = parse_cli_store(database, chat_dir, &chat_id, &file.path, sidecar.as_deref())?;
        if record.is_none() {
            return Ok(Vec::new());
        }
        let Some(mut record) = record else {
            return Ok(Vec::new());
        };
        if let Some(parent) = parent_id_from_db(database, &chat_id)?
            && parent != chat_id
            && safe_id(&parent)
        {
            record.main = false;
            record.parent_id = Some(format!("{AGENT}:{parent}"));
        }
        record.id = format!("{AGENT}:{chat_id}");
        Ok(vec![record])
    }

    fn resume_argv(&self, session_id: &str, _path: &Path) -> Option<Vec<String>> {
        let id = session_id.strip_prefix(&format!("{AGENT}:"))?;
        if !safe_id(id) {
            return None;
        }
        Some(vec![
            "cursor-agent".to_owned(),
            "--resume".to_owned(),
            id.to_owned(),
        ])
    }
}

fn cursor_config_dir() -> PathBuf {
    if let Ok(value) = std::env::var("CURSOR_CONFIG_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Ok(value) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("cursor");
        }
    }
    home_join(".cursor")
}

fn cursor_data_dir() -> PathBuf {
    if let Ok(value) = std::env::var("CURSOR_DATA_DIR") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    home_join(".cursor")
}

fn home_join(relative: &str) -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(
        || PathBuf::from(relative.trim_start_matches('.')),
        |home| PathBuf::from(home).join(relative),
    )
}

fn cli_chat_files(root: &Path) -> Result<Vec<SessionFile>, PluginError> {
    let chats = root.join("chats");
    if !chats.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    walk_store_dbs(&chats, &mut files)?;
    Ok(files)
}

fn walk_store_dbs(lexical: &Path, out: &mut Vec<SessionFile>) -> Result<(), PluginError> {
    let entries =
        std::fs::read_dir(lexical).map_err(|_| PluginError::io("cannot list cursor chats"))?;
    for entry in entries {
        let entry = entry.map_err(|_| PluginError::io("cannot list cursor chats"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list cursor chats"))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            walk_store_dbs(&path, out)?;
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) != Some("store.db") {
            continue;
        }
        let Some(id) = path
            .parent()
            .and_then(|chat_dir| chat_dir.file_name())
            .and_then(|name| name.to_str())
        else {
            continue;
        };
        if !safe_id(id) {
            continue;
        }
        let meta = path
            .parent()
            .map(|chat_dir| chat_dir.join("meta.json"))
            .filter(|meta| meta.is_file());
        let related = meta.map_or_else(Vec::new, |meta| vec![meta]);
        out.push(
            SessionFile::new(id, path.clone())
                .with_format(SessionFormat::Sqlite)
                .with_related(related),
        );
    }
    Ok(())
}

fn editor_transcript_files(
    data_root: &Path,
    cli_ids: &BTreeSet<String>,
) -> Result<Vec<SessionFile>, PluginError> {
    let projects = data_root.join("projects");
    if !projects.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    walk_transcript_jsonl(&projects, &mut paths)?;
    let mut files = Vec::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let id = name.strip_suffix(".jsonl").unwrap_or(name);
        if !safe_id(id) || cli_ids.contains(id) {
            continue;
        }
        if !transcript_has_messages(&path)? {
            continue;
        }
        files.push(SessionFile::new(id.to_owned(), path));
    }
    Ok(files)
}

fn walk_transcript_jsonl(projects: &Path, out: &mut Vec<PathBuf>) -> Result<(), PluginError> {
    let entries =
        std::fs::read_dir(projects).map_err(|_| PluginError::io("cannot list cursor projects"))?;
    for entry in entries {
        let entry = entry.map_err(|_| PluginError::io("cannot list cursor projects"))?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list cursor projects"))?
            .is_dir()
        {
            continue;
        }
        let transcripts = path.join("agent-transcripts");
        if !transcripts.is_dir() {
            continue;
        }
        collect_transcript_jsonl(&transcripts, out)?;
    }
    Ok(())
}

fn collect_transcript_jsonl(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), PluginError> {
    let entries = std::fs::read_dir(dir).map_err(|_| PluginError::io("cannot list transcripts"))?;
    for entry in entries {
        let entry = entry.map_err(|_| PluginError::io("cannot list transcripts"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| PluginError::io("cannot list transcripts"))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_transcript_jsonl(&path, out)?;
            continue;
        }
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn transcript_has_messages(path: &Path) -> Result<bool, PluginError> {
    let bytes =
        std::fs::read(path).map_err(|_| PluginError::io("cannot read cursor transcript"))?;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(value) = json::from_slice::<Value>(line) else {
            continue;
        };
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = message_text(value.get("message").unwrap_or(&value));
        if role == "user" {
            let (typed, _) = cursor_typed_text(&text);
            if !typed.trim().is_empty() {
                return Ok(true);
            }
        } else if !text.trim().is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn chat_meta_sidecar(file: &SessionFile) -> Option<PathBuf> {
    file.related
        .iter()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("meta.json"))
        .cloned()
}

fn parent_id_from_db(
    database: &dyn SessionDatabase,
    chat_id: &str,
) -> Result<Option<String>, PluginError> {
    let rows = database.query("SELECT value FROM meta WHERE key = '0'", &[])?;
    let Some(value) = rows.values.first().and_then(|row| row.first()) else {
        return Ok(None);
    };
    let meta = decode_meta_value(value)?;
    if meta.subagent.is_none() {
        return Ok(None);
    }
    let sub = meta.subagent.as_ref().expect("checked");
    let parent = if sub.root_parent.is_empty() {
        &sub.parent
    } else {
        &sub.root_parent
    };
    if parent.is_empty() || parent == chat_id {
        return Ok(None);
    }
    Ok(Some(parent.to_owned()))
}

fn parse_cli_store(
    database: &dyn SessionDatabase,
    _chat_dir: &Path,
    chat_id: &str,
    store_path: &Path,
    meta_path: Option<&Path>,
) -> Result<Option<SessionRecord>, PluginError> {
    let rows = database.query("SELECT value FROM meta WHERE key = '0'", &[])?;
    let db_meta = rows
        .values
        .first()
        .and_then(|row| row.first())
        .and_then(|value| decode_meta_value(value).ok());
    let sidecar = meta_path.and_then(read_meta_json);
    let public_id = format!("{AGENT}:{chat_id}");
    let mut record = SessionRecord::new(AGENT, &public_id, store_path);
    let mut named = String::new();
    let mut cwd = String::new();
    let mut start_ms = 0_i64;
    let mut last_ms = 0_i64;
    if let Some(side) = &sidecar {
        cwd.clone_from(&side.cwd);
        if !side.subagent {
            named = cursor_title(&side.title);
        }
        start_ms = side.created_ms;
        last_ms = side.updated_ms;
    }
    if let Some(meta) = &db_meta {
        if named.is_empty() {
            named = cursor_title(&meta.name);
        }
        if start_ms == 0 {
            start_ms = meta.created_at_ms;
        }
    }
    if last_ms == 0 {
        last_ms = start_ms;
    }
    let root = db_meta
        .as_ref()
        .map(|meta| meta.root_blob.clone())
        .unwrap_or_default();
    let started = cursor_started(database, &root);
    if started > start_ms {
        start_ms = started;
    }
    record.cwd = cwd;
    record.start_ms = start_ms;
    record.last_ms = last_ms.max(start_ms);
    let date = local_date(record.last_ms);
    let mut prompts = 0_u64;
    let mut replies = 0_u64;
    if !root.is_empty() {
        for message in cursor_messages(database, &root)? {
            let role = message.role.as_str();
            let parts = cursor_parts(&message.content);
            match role {
                "user" => {
                    let (typed, other) = cursor_typed(&parts);
                    if typed.is_empty() {
                        if record.title == public_id && !other.is_empty() {
                            record.title = untagged(&other);
                        }
                        continue;
                    }
                    prompts += 1;
                    if record.title == public_id {
                        record.title = title_first_line(&typed);
                    }
                }
                "assistant" => {
                    replies += 1;
                }
                _ => {}
            }
        }
    }
    if named.is_empty() && prompts == 0 && replies == 0 {
        return Ok(None);
    }
    if !named.is_empty() {
        record.title = named;
    }
    if prompts > 0 || replies > 0 {
        record.add_day(SessionDay::new(
            date,
            String::new(),
            SessionTokens::default(),
        ));
    }
    let _ = (prompts, replies);
    Ok(Some(record))
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct SidecarMeta {
    title: String,
    #[serde(rename = "createdAtMs", default)]
    created_ms: i64,
    #[serde(rename = "updatedAtMs", default)]
    updated_ms: i64,
    #[serde(rename = "isSubagent", default)]
    subagent: bool,
    #[serde(default)]
    cwd: String,
}

fn read_meta_json(path: &Path) -> Option<SidecarMeta> {
    let bytes = std::fs::read(path).ok()?;
    json::from_slice(&bytes).ok()
}

#[derive(Clone, Debug)]
struct DbMeta {
    name: String,
    created_at_ms: i64,
    root_blob: String,
    subagent: Option<SubagentInfo>,
}

#[derive(Clone, Debug)]
struct SubagentInfo {
    parent: String,
    root_parent: String,
}

fn decode_meta_value(value: &SessionValue) -> Result<DbMeta, PluginError> {
    let text = match value {
        SessionValue::Text(text) => text.clone(),
        SessionValue::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        _ => return Err(PluginError::decode("cursor meta row is not text")),
    };
    let trimmed = text.trim();
    let json_text = if trimmed.starts_with('{') {
        trimmed.to_owned()
    } else {
        let bytes = hex::decode(trimmed)
            .map_err(|()| PluginError::decode("cursor meta row is not hex json"))?;
        String::from_utf8(bytes).map_err(|_| PluginError::decode("cursor meta row is not utf-8"))?
    };
    let value: Value = json::from_str(&json_text)
        .map_err(|_| PluginError::decode("cursor meta row is not json"))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let created_at_ms = value
        .get("createdAt")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let root_blob = value
        .get("latestRootBlobId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let subagent = value.get("subagentInfo").map(|info| SubagentInfo {
        parent: info
            .get("parentAgentId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        root_parent: info
            .get("rootParentAgentId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    });
    Ok(DbMeta {
        name,
        created_at_ms,
        root_blob,
        subagent,
    })
}

#[derive(Clone, Debug)]
struct CursorMessage {
    role: String,
    content: Value,
}

#[derive(Clone, Debug)]
struct CursorPart {
    kind: String,
    text: String,
    _tool_name: String,
}

fn cursor_messages(
    database: &dyn SessionDatabase,
    root: &str,
) -> Result<Vec<CursorMessage>, PluginError> {
    let root_id =
        hex::decode(root).map_err(|()| PluginError::decode("cursor root blob id invalid"))?;
    let state = blob_bytes(database, &root_id)?;
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let mut ids: Vec<Vec<u8>> = Vec::new();
    let (archives, _) = pb_fields(&state, 13, 0);
    for archive in archives {
        if let Some(data) = blob_bytes(database, &archive)? {
            let (summed, _) = pb_fields(&data, 1, 0);
            ids.extend(summed);
        }
    }
    let (own, _) = pb_fields(&state, 1, 0);
    ids.extend(own);
    let mut out = Vec::new();
    for id in ids {
        let Some(data) = blob_bytes(database, &id)? else {
            continue;
        };
        let Ok(message) = json::from_slice::<CursorMessageJson>(&data) else {
            continue;
        };
        out.push(CursorMessage {
            role: message.role,
            content: message.content,
        });
    }
    Ok(out)
}

#[derive(serde::Deserialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct CursorMessageJson {
    role: String,
    content: Value,
}

fn blob_bytes(database: &dyn SessionDatabase, id: &[u8]) -> Result<Option<Vec<u8>>, PluginError> {
    let hex_id = hex::encode(id);
    let rows = database.query("SELECT data FROM blobs WHERE id = ?", &[hex_id.as_str()])?;
    let Some(row) = rows.values.first() else {
        return Ok(None);
    };
    let Some(value) = row.first() else {
        return Ok(None);
    };
    Ok(match value {
        SessionValue::Blob(bytes) => Some(bytes.clone()),
        SessionValue::Text(text) => Some(text.as_bytes().to_vec()),
        _ => None,
    })
}

fn cursor_started(database: &dyn SessionDatabase, root: &str) -> i64 {
    let Ok(root_id) = hex::decode(root) else {
        return 0;
    };
    let Ok(Some(data)) = blob_bytes(database, &root_id) else {
        return 0;
    };
    let (_, at) = pb_fields(&data, -1, 26);
    i64::try_from(at).unwrap_or(0)
}

fn pb_fields(bytes: &[u8], bytes_field: i32, var_field: i32) -> (Vec<Vec<u8>>, u64) {
    let mut rest = bytes;
    let mut vals = Vec::new();
    let mut varint = 0_u64;
    while !rest.is_empty() {
        let Some((key, consumed)) = read_uvarint(rest) else {
            break;
        };
        rest = &rest[consumed..];
        let num = i32::try_from(key >> 3).unwrap_or(i32::MAX);
        let typ = key & 7;
        match typ {
            0 => {
                let Some((value, consumed)) = read_uvarint(rest) else {
                    break;
                };
                rest = &rest[consumed..];
                if num == var_field {
                    varint = value;
                }
            }
            1 => {
                if rest.len() < 8 {
                    break;
                }
                rest = &rest[8..];
            }
            2 => {
                let Some((length, consumed)) = read_uvarint(rest) else {
                    break;
                };
                rest = &rest[consumed..];
                let length = usize::try_from(length).unwrap_or(usize::MAX);
                if rest.len() < length {
                    break;
                }
                if num == bytes_field {
                    vals.push(rest[..length].to_vec());
                }
                rest = &rest[length..];
            }
            5 => {
                if rest.len() < 4 {
                    break;
                }
                rest = &rest[4..];
            }
            _ => break,
        }
    }
    (vals, varint)
}

fn read_uvarint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut result = 0_u64;
    for (index, byte) in bytes.iter().enumerate() {
        let value = u64::from(*byte & 0x7f);
        result |= value << (index * 7);
        if *byte & 0x80 == 0 {
            return Some((result, index + 1));
        }
        if index == 9 {
            return None;
        }
    }
    None
}

fn cursor_parts(content: &Value) -> Vec<CursorPart> {
    if let Some(text) = content.as_str() {
        return vec![CursorPart {
            kind: "text".to_owned(),
            text: text.to_owned(),
            _tool_name: String::new(),
        }];
    }
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    Some(CursorPart {
                        kind: part.get("type")?.as_str()?.to_owned(),
                        text: part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        _tool_name: part
                            .get("toolName")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cursor_typed(parts: &[CursorPart]) -> (String, String) {
    let texts: Vec<String> = parts
        .iter()
        .filter(|part| part.kind == "text")
        .map(|part| part.text.clone())
        .collect();
    cursor_typed_text(&texts.join("\n"))
}

fn cursor_typed_text(all: &str) -> (String, String) {
    static QUERY: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static CONTEXT: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let query = QUERY.get_or_init(|| {
        Regex::new(r"(?s)<user_query>(.*?)</user_query>").expect("user_query regex")
    });
    let context = CONTEXT.get_or_init(|| {
        Regex::new(r"(?s)(?:<user_info(?:\s[^>]*)?>.*?</user_info>|<project_layout(?:\s[^>]*)?>.*?</project_layout>|<rules(?:\s[^>]*)?>.*?</rules>|<always_applied_workspace_rules(?:\s[^>]*)?>.*?</always_applied_workspace_rules>|<agent_requestable_workspace_rules(?:\s[^>]*)?>.*?</agent_requestable_workspace_rules>|<user_rules(?:\s[^>]*)?>.*?</user_rules>|<agent_skills(?:\s[^>]*)?>.*?</agent_skills>|<available_skills(?:\s[^>]*)?>.*?</available_skills>|<open_and_recently_viewed_files(?:\s[^>]*)?>.*?</open_and_recently_viewed_files>|<system_reminder(?:\s[^>]*)?>.*?</system_reminder>|<system-reminder(?:\s[^>]*)?>.*?</system-reminder>|<instructions_update(?:\s[^>]*)?>.*?</instructions_update>|<mcp_instructions(?:\s[^>]*)?>.*?</mcp_instructions>|<mcp_file_system(?:\s[^>]*)?>.*?</mcp_file_system>|<mcp_file_system_servers(?:\s[^>]*)?>.*?</mcp_file_system_servers>|<git_status(?:\s[^>]*)?>.*?</git_status>|<agent_transcripts(?:\s[^>]*)?>.*?</agent_transcripts>|<cursor_rules_context(?:\s[^>]*)?>.*?</cursor_rules_context>|<attached_files(?:\s[^>]*)?>.*?</attached_files>|<system_notification(?:\s[^>]*)?>.*?</system_notification>|<task_notification(?:\s[^>]*)?>.*?</task_notification>|<agent_notification(?:\s[^>]*)?>.*?</agent_notification>)").expect("context regex")
    });
    let queries: Vec<String> = query
        .captures_iter(all)
        .filter_map(|capture| capture.get(1).map(|m| m.as_str().trim().to_owned()))
        .collect();
    if !queries.is_empty() {
        return (queries.join("\n"), String::new());
    }
    let rest = context.replace_all(all, "").trim().to_string();
    if rest.starts_with('<') {
        return (String::new(), rest);
    }
    (rest, String::new())
}

fn message_text(value: &Value) -> String {
    let content = value.get("content").unwrap_or(value);
    cursor_parts(content)
        .into_iter()
        .filter(|part| part.kind == "text")
        .map(|part| part.text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn cursor_title(title: &str) -> String {
    let trimmed = title_first_line(title);
    if trimmed.eq_ignore_ascii_case("New Agent") {
        String::new()
    } else {
        trimmed
    }
}

fn title_first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_owned()
}

fn untagged(text: &str) -> String {
    cursor_typed_text(text).0
}

fn local_date(ms: i64) -> String {
    let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms) else {
        return LocalFallback::today();
    };
    dt.with_timezone(&chrono::Local)
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

struct LocalFallback;

impl LocalFallback {
    fn today() -> String {
        chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(crate = "hofund_plugin_sdk::serde")]
struct TranscriptState {
    record: SessionRecord,
    title_set: bool,
}

impl TranscriptState {
    fn new(file: &SessionFile) -> Self {
        Self {
            record: SessionRecord::new(AGENT, format!("{AGENT}:{}", file.id), &file.path),
            title_set: false,
        }
    }
}

struct TranscriptParser {
    state: TranscriptState,
}

impl SessionParser for TranscriptParser {
    fn push(&mut self, value: &Value) -> Result<(), PluginError> {
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "user" && role != "assistant" {
            return Ok(());
        }
        let text = message_text(value.get("message").unwrap_or(value));
        let at = self.state.record.last_ms;
        match role {
            "user" => {
                let (typed, _) = cursor_typed_text(&text);
                if typed.trim().is_empty() {
                    return Ok(());
                }
                if !self.state.title_set {
                    self.state.record.title = title_first_line(&typed);
                    self.state.title_set = true;
                }
                self.state.record.add_day(SessionDay::new(
                    local_date(at),
                    String::new(),
                    hofund_plugin_api::SessionTokens::default(),
                ));
            }
            "assistant" => {
                if text.trim().is_empty() {
                    return Ok(());
                }
                self.state.record.add_day(SessionDay::new(
                    local_date(at),
                    String::new(),
                    hofund_plugin_api::SessionTokens::default(),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SessionParse, PluginError> {
        let Self { mut state } = *self;
        if state.title_set {
            state.record.id = format!(
                "{AGENT}:{}",
                state
                    .record
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&state.record.id)
                    .strip_suffix(".jsonl")
                    .unwrap_or(&state.record.id)
            );
            let metadata = std::fs::metadata(&state.record.path).ok();
            state.record.last_ms = metadata
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|duration| i64::try_from(duration.as_millis()).ok())
                .unwrap_or(state.record.last_ms);
            return Ok(SessionParse::new(state.record, "{}"));
        }
        Ok(SessionParse::default())
    }
}

mod hex {
    pub(super) fn decode(text: &str) -> Result<Vec<u8>, ()> {
        let text = text.trim();
        if !text.len().is_multiple_of(2) {
            return Err(());
        }
        let mut out = Vec::with_capacity(text.len() / 2);
        let bytes = text.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let high = from_hex(bytes[index])?;
            let low = from_hex(bytes[index + 1])?;
            out.push((high << 4) | low);
            index += 2;
        }
        Ok(out)
    }

    pub(super) fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = core::fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}"));
        }
        out
    }

    fn from_hex(byte: u8) -> Result<u8, ()> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use hofund_plugin_api::SessionStore;

    #[test]
    fn safe_id_matches_cursor_chat_ids() {
        assert!(safe_id("6f0c1d2e-3a4b-4c5d-8e6f-7a8b9c0d1e2f"));
        assert!(!safe_id("../escape"));
    }

    #[test]
    fn cursor_typed_pulls_user_query() {
        let text = "<user_info>x</user_info>\n<user_query>hello\nworld</user_query>";
        let (typed, other) = cursor_typed_text(text);
        assert_eq!(typed, "hello\nworld");
        assert_eq!(other, "");
    }

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root")
    }

    #[test]
    fn lists_cli_store_fixture() {
        let store = CursorStore::with_root(fixture_root());
        let files = store
            .files(&fixture_root())
            .expect("files")
            .into_iter()
            .filter(|file| file.format == SessionFormat::Sqlite)
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].id, "sess-demo-001");
    }
}
