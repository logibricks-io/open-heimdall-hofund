//! Hermes Agent session discovery and read-only `state.db` parsing.
//!
//! Each store (`state.db` under the home or a profile) holds many sessions.
//! The kernel opens SQLite read-only; this crate supplies fixed-schema SQL.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone, Utc};
use hofund_plugin_api::{
    PluginError, SessionDatabase, SessionDay, SessionFile, SessionFormat, SessionLocator,
    SessionRecord, SessionStore, SessionTokens, SessionValue,
};
use hofund_plugin_sdk::{chrono, json};
use json::Value;
use sha2::{Digest, Sha256};

/// The session agent id.
pub const AGENT: &str = "hermes";

/// Hermes home and profile `state.db` stores.
#[derive(Clone, Debug)]
pub struct HermesStore {
    locator: HermesLocator,
}

impl HermesStore {
    /// Uses `HERMES_HOME` or the default Hermes data directory.
    #[must_use]
    pub fn new() -> Self {
        Self::with_home(hermes_home())
    }

    /// Uses an explicit home tree for tests.
    #[must_use]
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            locator: HermesLocator { home: home.into() },
        }
    }
}

impl Default for HermesStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory-only Hermes locator.
#[derive(Clone, Debug)]
pub struct HermesLocator {
    home: PathBuf,
}

impl SessionLocator for HermesLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.home.clone()]
    }

    fn present(&self) -> bool {
        let home = &self.home;
        if !home.is_dir() {
            return false;
        }
        home.join("state.db").is_file() || home.join("profiles").is_dir()
    }
}

impl SessionStore for HermesStore {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn locator(&self) -> &dyn SessionLocator {
        &self.locator
    }

    fn parser(
        &self,
        file: &SessionFile,
        _state: Option<&str>,
    ) -> Result<Box<dyn hofund_plugin_api::SessionParser>, PluginError> {
        if file.format == SessionFormat::Sqlite {
            return Err(PluginError::decode("hermes sessions are read from SQLite"));
        }
        Err(PluginError::decode("unexpected hermes parser format"))
    }

    fn files(&self, root: &Path) -> Result<Vec<SessionFile>, PluginError> {
        let mut files = Vec::new();
        for dir in store_roots(root) {
            let db = dir.join("state.db");
            if !db.is_file() {
                continue;
            }
            let id = store_id(&db);
            files.push(SessionFile::new(id, db).with_format(SessionFormat::Sqlite));
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    fn read_database(
        &self,
        file: &SessionFile,
        database: &dyn SessionDatabase,
    ) -> Result<Vec<SessionRecord>, PluginError> {
        if file.format != SessionFormat::Sqlite {
            return Err(PluginError::decode("unexpected hermes parser format"));
        }
        let store = store_id(&file.path);
        let session_cols = table_columns(database, "sessions")?;
        if !session_cols.contains("id") {
            return Ok(Vec::new());
        }
        let rows = database.query(
            "SELECT id FROM sessions WHERE id IS NOT NULL AND id != ''",
            &[],
        )?;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for row in &rows.values {
            let Some(SessionValue::Text(sid)) = row.first() else {
                continue;
            };
            if sid.is_empty() || !seen.insert(sid.clone()) {
                continue;
            }
            if let Some(record) = parse_session(database, &file.path, &store, sid, &session_cols)? {
                out.push(record);
            }
        }
        Ok(out)
    }
}

fn hermes_home() -> PathBuf {
    if let Ok(value) = std::env::var("HERMES_HOME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    #[cfg(windows)]
    {
        home_dir().join("AppData").join("Local").join("hermes")
    }
    #[cfg(not(windows))]
    {
        home_dir().join(".hermes")
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

fn store_roots(home: &Path) -> Vec<PathBuf> {
    let mut root = home.to_path_buf();
    if root
        .parent()
        .and_then(|parent| parent.file_name())
        .is_some_and(|name| name == "profiles")
        && let Some(grand) = root.parent().and_then(Path::parent)
    {
        root = grand.to_path_buf();
    }
    let mut dirs = vec![root.clone()];
    let profiles = root.join("profiles");
    if profiles.is_dir() {
        let Ok(entries) = std::fs::read_dir(&profiles) else {
            return dedupe_store_dirs(dirs);
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            dirs.push(entry.path());
        }
    }
    dedupe_store_dirs(dirs)
}

fn dedupe_store_dirs(dirs: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for dir in dirs {
        let db = dir.join("state.db");
        if !db.is_file() {
            continue;
        }
        let canon = canonical_path(&db);
        if seen.insert(canon) {
            out.push(dir);
        }
    }
    out
}

fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn store_id(db_path: &Path) -> String {
    let canon = canonical_path(db_path);
    let hash = Sha256::digest(canon.to_string_lossy().as_bytes());
    format!("h{}", hex_bytes(&hash[..8]))
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, byte| {
            use std::fmt::Write;
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

fn table_columns(
    database: &dyn SessionDatabase,
    table: &str,
) -> Result<BTreeSet<String>, PluginError> {
    let sql = match table {
        "sessions" => "SELECT name FROM pragma_table_info('sessions')",
        "messages" => "SELECT name FROM pragma_table_info('messages')",
        "session_model_usage" => "SELECT name FROM pragma_table_info('session_model_usage')",
        _ => return Err(PluginError::decode("unknown hermes table")),
    };
    let rows = database.query(sql, &[])?;
    Ok(rows
        .values
        .iter()
        .filter_map(|row| match row.first()? {
            SessionValue::Text(name) => Some(name.clone()),
            _ => None,
        })
        .collect())
}

fn column_expr(columns: &BTreeSet<String>, name: &str, fallback: &str) -> String {
    if columns.contains(name) {
        format!("COALESCE({name}, {fallback})")
    } else {
        fallback.to_owned()
    }
}

fn parse_session(
    database: &dyn SessionDatabase,
    db_path: &Path,
    store: &str,
    sid: &str,
    session_cols: &BTreeSet<String>,
) -> Result<Option<SessionRecord>, PluginError> {
    let message_cols = table_columns(database, "messages")?;
    let usage_cols = table_columns(database, "session_model_usage")?;

    let meta_sql = format!(
        "SELECT {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} FROM sessions WHERE id = ?",
        column_expr(session_cols, "title", "''"),
        column_expr(session_cols, "cwd", "''"),
        column_expr(session_cols, "model", "''"),
        column_expr(session_cols, "billing_provider", "''"),
        column_expr(session_cols, "started_at", "0"),
        column_expr(session_cols, "ended_at", "0"),
        column_expr(session_cols, "last_activity_at", "0"),
        column_expr(session_cols, "input_tokens", "0"),
        column_expr(session_cols, "output_tokens", "0"),
        column_expr(session_cols, "cache_read_tokens", "0"),
        column_expr(session_cols, "cache_write_tokens", "0"),
        column_expr(session_cols, "reasoning_tokens", "0"),
        column_expr(session_cols, "api_call_count", "0"),
    );
    let meta = database.query(&meta_sql, &[sid])?;
    let Some(row) = meta.values.first() else {
        return Ok(None);
    };

    let title = text_at(row, 0);
    let cwd = text_at(row, 1);
    let fallback_model = text_at(row, 2);
    let _billing = text_at(row, 3);
    let started = real_at(row, 4);
    let ended = real_at(row, 5);
    let last_activity = real_at(row, 6);
    let fallback = SessionTokens::new(
        int_at(row, 7),
        int_at(row, 8),
        int_at(row, 9),
        int_at(row, 10),
    );

    let public_id = format!("{AGENT}:{store}-{sid}");
    let mut record = SessionRecord::new(AGENT, &public_id, db_path);
    record.title = title_or_trim(&title);
    record.cwd = cwd;
    record.start_ms = hermes_ms(started);
    record.last_ms = hermes_ms(last_activity.max(ended).max(started));

    if let Ok(messages) = load_messages(database, &message_cols, sid) {
        let mut last = record.start_ms;
        for message in messages {
            let at = hermes_ms(message.at);
            if at > 0 {
                last = last.max(at);
                if record.start_ms == 0 || record.start_ms > at {
                    record.start_ms = at;
                }
            }
            match message.role.as_str() {
                "user" if record.title.is_empty() || record.title == public_id => {
                    let text = hermes_text(&message.content);
                    let trimmed = title_or_trim(&text);
                    if !trimmed.is_empty() {
                        record.title = trimmed;
                    }
                }
                _ => {}
            }
        }
        if last > record.last_ms {
            record.last_ms = last;
        }
    }

    if record.title.is_empty() {
        record.title.clone_from(&public_id);
    }

    let usages = session_usages(database, &usage_cols, sid, fallback_model, fallback)?;
    for usage in usages {
        let at = hermes_ms(usage.last);
        let day = local_date(if at > 0 { at } else { record.last_ms });
        if !usage.model.is_empty() && !usage.tokens.is_zero() {
            record.add_usage(&usage.model, usage.tokens);
            record.add_day(SessionDay::new(day, &usage.model, usage.tokens));
        }
        if at > record.last_ms {
            record.last_ms = at;
        }
    }

    Ok(Some(record))
}

struct HermesMessage {
    role: String,
    content: String,
    at: f64,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct MessageIdentity {
    role: String,
    content: String,
    tools: String,
    tool_id: String,
    tool_name: String,
    at_bits: u64,
}

#[derive(Clone, Debug)]
struct HermesUsage {
    model: String,
    tokens: SessionTokens,
    last: f64,
}

fn load_messages(
    database: &dyn SessionDatabase,
    columns: &BTreeSet<String>,
    sid: &str,
) -> Result<Vec<HermesMessage>, PluginError> {
    if !columns.contains("session_id") || !columns.contains("role") {
        return Ok(Vec::new());
    }
    let select = vec![
        column_expr(columns, "role", "NULL"),
        column_expr(columns, "content", "NULL"),
        column_expr(columns, "tool_calls", "NULL"),
        column_expr(columns, "tool_call_id", "NULL"),
        column_expr(columns, "tool_name", "NULL"),
        column_expr(columns, "display_kind", "NULL"),
        format!("{} AS ts", column_expr(columns, "timestamp", "0")),
        column_expr(columns, "active", "1"),
        column_expr(columns, "id", "rowid"),
    ];
    let mut sql = format!(
        "SELECT {} FROM messages WHERE session_id = ?",
        select.join(", ")
    );
    if columns.contains("active") {
        sql.push_str(" AND (COALESCE(active, 1) != 0");
        if columns.contains("compacted") {
            sql.push_str(" OR COALESCE(compacted, 0) != 0");
        }
        sql.push(')');
    }
    if columns.contains("display_metadata") {
        sql.push_str(
            " AND COALESCE(json_extract(CASE WHEN json_valid(display_metadata) THEN display_metadata ELSE '{}' END, '$.model_only'), 0) = 0",
        );
    }
    let _ = write!(sql, " ORDER BY {}", column_expr(columns, "id", "rowid"));

    let rows = database.query(&sql, &[sid])?;
    let mut best: HashMap<MessageIdentity, (i32, i64, HermesMessage)> = HashMap::new();
    for row in &rows.values {
        let role = text_at(row, 0);
        let mut content = text_at(row, 1);
        let tools = text_at(row, 2);
        let tool_id = text_at(row, 3);
        let tool_name = text_at(row, 4);
        let display_kind = text_at(row, 5);
        let at = real_at(row, 6);
        let active = i32::try_from(int_at(row, 7)).unwrap_or(i32::MAX);
        let msg_id = i64::try_from(int_at(row, 8)).unwrap_or(0);

        if role == "user" {
            content = display_content_key(&content, &display_kind);
        }

        let key = MessageIdentity {
            role: role.clone(),
            content: content.clone(),
            tools: tools.clone(),
            tool_id: tool_id.clone(),
            tool_name: tool_name.clone(),
            at_bits: at.to_bits(),
        };
        let message = HermesMessage { role, content, at };
        match best.get(&key) {
            Some((best_active, best_id, _))
                if active > *best_active || (active == *best_active && msg_id > *best_id) =>
            {
                best.insert(key, (active, msg_id, message));
            }
            Some(_) => {}
            None => {
                best.insert(key, (active, msg_id, message));
            }
        }
    }
    let mut messages: Vec<_> = best.into_values().map(|(_, _, message)| message).collect();
    messages.sort_by(|left, right| {
        left.at
            .partial_cmp(&right.at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(messages)
}

fn session_usages(
    database: &dyn SessionDatabase,
    columns: &BTreeSet<String>,
    sid: &str,
    fallback_model: String,
    fallback: SessionTokens,
) -> Result<Vec<HermesUsage>, PluginError> {
    if !columns.contains("session_id") || !columns.contains("model") {
        if !fallback.is_zero() && !fallback_model.is_empty() {
            return Ok(vec![HermesUsage {
                model: fallback_model,
                tokens: fallback,
                last: 0.0,
            }]);
        }
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {}, {}, {}, {}, {}, {}, {}, {}, {} FROM session_model_usage WHERE session_id = ?",
        column_expr(columns, "model", "''"),
        column_expr(columns, "billing_provider", "''"),
        column_expr(columns, "task", "''"),
        column_expr(columns, "input_tokens", "0"),
        column_expr(columns, "output_tokens", "0"),
        column_expr(columns, "cache_read_tokens", "0"),
        column_expr(columns, "cache_write_tokens", "0"),
        column_expr(columns, "reasoning_tokens", "0"),
        column_expr(columns, "last_seen", "0"),
    );
    let rows = database.query(&sql, &[sid])?;
    let mut out = Vec::new();
    let mut main_total = SessionTokens::default();
    let has_task = columns.contains("task");
    for row in &rows.values {
        let model = text_at(row, 0);
        let _billing = text_at(row, 1);
        let task = text_at(row, 2);
        let tokens = SessionTokens::new(
            int_at(row, 3),
            int_at(row, 4),
            int_at(row, 5),
            int_at(row, 6),
        );
        let last = real_at(row, 8);
        if has_task && !task.is_empty() {
            continue;
        }
        main_total.add(tokens);
        out.push(HermesUsage {
            model,
            tokens,
            last,
        });
    }
    if out.is_empty() {
        if !fallback.is_zero() {
            out.push(HermesUsage {
                model: fallback_model,
                tokens: fallback,
                last: 0.0,
            });
        }
        return Ok(out);
    }
    if has_task {
        let mut residual = fallback;
        residual.input = residual.input.saturating_sub(main_total.input);
        residual.output = residual.output.saturating_sub(main_total.output);
        residual.cache_read = residual.cache_read.saturating_sub(main_total.cache_read);
        residual.cache_write = residual.cache_write.saturating_sub(main_total.cache_write);
        if !residual.is_zero() && !fallback_model.is_empty() {
            out.push(HermesUsage {
                model: fallback_model,
                tokens: residual,
                last: 0.0,
            });
        }
    }
    Ok(out)
}

fn hermes_ms(seconds: f64) -> i64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        (seconds * 1000.0).round() as i64
    }
}

fn local_date(ms: i64) -> String {
    if ms <= 0 {
        return Local::now().date_naive().format("%Y-%m-%d").to_string();
    }
    let Some(utc) = Utc.timestamp_millis_opt(ms).single() else {
        return Local::now().date_naive().format("%Y-%m-%d").to_string();
    };
    utc.with_timezone(&Local)
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

fn title_or_trim(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.len() > 120 {
        format!("{}…", &trimmed[..120])
    } else {
        trimmed.to_owned()
    }
}

fn text_at(row: &[SessionValue], index: usize) -> String {
    match row.get(index) {
        Some(SessionValue::Text(value)) => value.clone(),
        Some(SessionValue::Integer(value)) => value.to_string(),
        Some(SessionValue::Real(value)) => value.to_string(),
        Some(SessionValue::Blob(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
        _ => String::new(),
    }
}

fn int_at(row: &[SessionValue], index: usize) -> u64 {
    match row.get(index) {
        Some(SessionValue::Integer(value)) => value.unsigned_abs(),
        Some(SessionValue::Real(value)) => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                value.max(0.0) as u64
            }
        }
        Some(SessionValue::Text(value)) => value.parse().unwrap_or(0),
        _ => 0,
    }
}

fn real_at(row: &[SessionValue], index: usize) -> f64 {
    match row.get(index) {
        Some(SessionValue::Real(value)) => *value,
        Some(SessionValue::Integer(value)) => {
            #[allow(clippy::cast_precision_loss)]
            {
                *value as f64
            }
        }
        Some(SessionValue::Text(value)) => value.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn hermes_text(raw: &str) -> String {
    let Some(encoded) = raw.strip_prefix("\u{0}json:") else {
        return raw.to_owned();
    };
    let Ok(Value::Array(parts)) = json::from_str::<Value>(encoded) else {
        return encoded.to_owned();
    };
    parts
        .iter()
        .filter_map(|part| {
            let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "text" {
                part.get("text").and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn display_content_key(raw: &str, display_kind: &str) -> String {
    if !display_kind.is_empty() && display_kind != "hidden" {
        return raw.to_owned();
    }
    if let Some(live) = display_text_key(raw) {
        return live;
    }
    if let Some(encoded) = raw.strip_prefix("\u{0}json:") {
        let Ok(parts) = json::from_str::<Vec<Value>>(encoded) else {
            return raw.to_owned();
        };
        let texts: Vec<String> = parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .collect();
        let joined = texts.join("\n");
        if display_text_key(&joined).is_some() {
            return format!("\u{0}json:{encoded}");
        }
    }
    raw.to_owned()
}

const MERGED: &str = "[END OF PRIOR CONTEXT — COMPACTION SUMMARY BELOW]";
const HANDOFF_END: &str =
    "--- END OF CONTEXT SUMMARY — respond to the message below, not the summary above ---";
const CURRENT_SUMMARY: &str = "[CONTEXT COMPACTION — REFERENCE ONLY]";
const LEGACY_SUMMARY: &str = "[CONTEXT SUMMARY]:";

fn display_text_key(content: &str) -> Option<String> {
    if let Some(index) = content.find(MERGED) {
        let summary = content[index + MERGED.len()..].trim_start();
        if starts_summary(summary) {
            let prior = content[..index].trim();
            let prior = prior
                .strip_prefix("[PRIOR CONTEXT — for reference only; not a new message]")
                .unwrap_or(prior)
                .trim();
            if !prior.is_empty() {
                return Some(prior.to_owned());
            }
        }
        return None;
    }
    if !starts_summary(content.trim_start()) {
        return None;
    }
    if let Some(index) = content.find(HANDOFF_END) {
        let live = content[index + HANDOFF_END.len()..].trim();
        if !live.is_empty() {
            return Some(live.to_owned());
        }
    }
    None
}

fn starts_summary(text: &str) -> bool {
    text.starts_with(CURRENT_SUMMARY) || text.starts_with(LEGACY_SUMMARY)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use hofund_plugin_api::{SessionFormat, SessionStore};

    use super::HermesStore;

    fn fixture_home() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/root/.hermes")
            .canonicalize()
            .expect("hermes fixtures")
    }

    #[test]
    fn lists_default_and_profile_stores() {
        let store = HermesStore::with_home(fixture_home());
        let files = store.files(&fixture_home()).expect("files");
        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .all(|file| file.format == SessionFormat::Sqlite)
        );
        let ids: Vec<_> = files.iter().map(|file| file.id.as_str()).collect();
        assert_ne!(ids[0], ids[1]);
    }
}
