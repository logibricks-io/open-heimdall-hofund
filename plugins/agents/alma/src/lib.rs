//! Alma session discovery and read-only `chat_threads.db` parsing.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::path::{Path, PathBuf};

use hofund_plugin_api::{
    PluginError, SessionDatabase, SessionFile, SessionFormat, SessionLocator, SessionParser,
    SessionRecord, SessionStore, SessionTokens, SessionValue,
};
use hofund_plugin_sdk::{chrono, json};
use json::Value;

/// The session agent id.
pub const AGENT: &str = "alma";

/// Alma session store under the OS config `alma/` directory.
#[derive(Clone, Debug)]
pub struct AlmaStore {
    locator: AlmaLocator,
    database: PathBuf,
}

impl AlmaStore {
    /// Uses the OS config `alma/chat_threads.db` path.
    #[must_use]
    pub fn new() -> Self {
        let root = alma_config_dir();
        Self::with_root(root)
    }

    /// Uses an explicit Alma config folder for tests.
    #[must_use]
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let database = root.join("chat_threads.db");
        Self {
            locator: AlmaLocator { root },
            database,
        }
    }
}

impl Default for AlmaStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
struct AlmaLocator {
    root: PathBuf,
}

impl SessionLocator for AlmaLocator {
    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn present(&self) -> bool {
        self.root.join("chat_threads.db").is_file()
    }
}

impl SessionStore for AlmaStore {
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
        _file: &SessionFile,
        _state: Option<&str>,
    ) -> Result<Box<dyn SessionParser>, PluginError> {
        Err(PluginError::decode("alma sessions are read from SQLite"))
    }

    fn read_database(
        &self,
        file: &SessionFile,
        database: &dyn SessionDatabase,
    ) -> Result<Vec<SessionRecord>, PluginError> {
        if file.format != SessionFormat::Sqlite {
            return Err(PluginError::decode("unexpected alma file format"));
        }
        let threads = list_threads(database)?;
        let mut records = Vec::new();
        for thread in threads {
            records.push(read_thread(database, &file.path, &thread)?);
        }
        records.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(records)
    }
}

fn alma_config_dir() -> PathBuf {
    std::env::var_os("ALMA_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(alma_default_config_dir, PathBuf::from)
}

fn alma_default_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("alma");
        }
    }
    let home = user_home();
    if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(config).join("alma");
    }
    home.join(".config").join("alma")
}

fn user_home() -> PathBuf {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[derive(Clone, Debug)]
struct ThreadRow {
    id: String,
    title: String,
    cwd: String,
    created_at: String,
    updated_at: String,
    rev: String,
}

fn list_threads(database: &dyn SessionDatabase) -> Result<Vec<ThreadRow>, PluginError> {
    let sql = "SELECT t.id, COALESCE(t.title, ''), COALESCE(t.created_at, ''), COALESCE(t.updated_at, ''), COALESCE(w.path, '') \
        FROM chat_threads t LEFT JOIN workspaces w ON w.id = t.workspace_id \
        WHERE COALESCE(t.is_incognito, 0) = 0 \
          AND COALESCE(json_extract(CASE WHEN json_valid(t.metadata) THEN t.metadata END, '$.isCron'), 0) = 0 \
          AND instr(COALESCE(t.title, ''), '⏰ Cron:') <> 1";
    let rows = database
        .query(sql, &[])
        .map_err(|_| PluginError::decode("cannot list alma threads"))?;
    let mut out = Vec::new();
    for row in &rows.values {
        let id = text_at(row, 0);
        if id.is_empty() {
            continue;
        }
        let title = text_at(row, 1);
        let created = text_at(row, 2);
        let updated = text_at(row, 3);
        let cwd = text_at(row, 4);
        if message_count(database, &id)? == 0 && usage_count(database, &id)? == 0 {
            continue;
        }
        let rev = format!("{updated}|{}", message_count(database, &id)?);
        out.push(ThreadRow {
            id: id.clone(),
            title: title.clone(),
            cwd: cwd.clone(),
            created_at: created.clone(),
            updated_at: updated.clone(),
            rev,
        });
    }
    Ok(out)
}

fn read_thread(
    database: &dyn SessionDatabase,
    db_path: &Path,
    thread: &ThreadRow,
) -> Result<SessionRecord, PluginError> {
    let mut record = SessionRecord::new(
        AGENT,
        format!("{AGENT}:{}", thread.id),
        PathBuf::from(format!("{}#{}", db_path.display(), thread.id)),
    );
    record.cwd.clone_from(&thread.cwd);
    record.title = title_from(&thread.title, database, &thread.id)?;
    record.start_ms = alma_time_ms(&thread.created_at);
    record.last_ms = alma_time_ms(&thread.updated_at).max(record.start_ms);
    apply_usage(database, &thread.id, &mut record)?;
    apply_first_prompt(database, &thread.id, &mut record)?;
    let _ = &thread.rev;
    Ok(record)
}

fn title_from(
    stored: &str,
    database: &dyn SessionDatabase,
    thread_id: &str,
) -> Result<String, PluginError> {
    let stored = stored.trim();
    if !stored.is_empty() && stored != "New Chat" {
        return Ok(normalize_title(stored));
    }
    let sql = "SELECT message FROM chat_messages WHERE thread_id = ? AND COALESCE(parent_tool_call_id, '') = '' ORDER BY timestamp LIMIT 5";
    let rows = database
        .query(sql, &[thread_id])
        .map_err(|_| PluginError::decode("cannot read alma messages"))?;
    for row in &rows.values {
        let raw = text_at(row, 0);
        if raw.is_empty() {
            continue;
        }
        if let Ok(value) = json::from_str::<Value>(&raw)
            && let Some(text) = first_user_text(&value)
        {
            return Ok(normalize_title(&text));
        }
    }
    Ok(String::new())
}

fn apply_first_prompt(
    database: &dyn SessionDatabase,
    thread_id: &str,
    record: &mut SessionRecord,
) -> Result<(), PluginError> {
    if !record.title.is_empty() {
        return Ok(());
    }
    record.title = title_from("", database, thread_id)?;
    Ok(())
}

fn apply_usage(
    database: &dyn SessionDatabase,
    thread_id: &str,
    record: &mut SessionRecord,
) -> Result<(), PluginError> {
    let sql = "SELECT model, provider_id, input_tokens, output_tokens, cached_input_tokens, cache_write_input_tokens FROM usage_records WHERE thread_id = ?";
    let rows = database
        .query(sql, &[thread_id])
        .map_err(|_| PluginError::decode("cannot read alma usage"))?;
    for row in &rows.values {
        let model = text_at(row, 0);
        let provider = text_at(row, 1);
        let input = i64::try_from(int_at(row, 2)).unwrap_or(0).max(0);
        let output = i64::try_from(int_at(row, 3)).unwrap_or(0).max(0);
        let cache_read = i64::try_from(int_at(row, 4)).unwrap_or(0).max(0);
        let cache_write = i64::try_from(int_at(row, 5)).unwrap_or(0).max(0);
        let name = alma_model(&model, &provider);
        let tokens = SessionTokens::new(
            u64::try_from(input - cache_read).unwrap_or(0),
            u64::try_from(output).unwrap_or(0),
            u64::try_from(cache_read).unwrap_or(0),
            u64::try_from(cache_write).unwrap_or(0),
        );
        record.add_usage(name, tokens);
    }
    let sql = "SELECT model, provider_id, input_tokens, output_tokens FROM aux_usage_records WHERE thread_id = ?";
    let rows = database
        .query(sql, &[thread_id])
        .map_err(|_| PluginError::decode("cannot read alma aux usage"))?;
    for row in &rows.values {
        let model = text_at(row, 0);
        let provider = text_at(row, 1);
        let input = i64::try_from(int_at(row, 2)).unwrap_or(0).max(0);
        let output = i64::try_from(int_at(row, 3)).unwrap_or(0).max(0);
        let name = alma_model(&model, &provider);
        record.add_usage(
            name,
            SessionTokens::new(
                u64::try_from(input).unwrap_or(0),
                u64::try_from(output).unwrap_or(0),
                0,
                0,
            ),
        );
    }
    Ok(())
}

fn alma_model(model: &str, provider_id: &str) -> String {
    let model = model.trim();
    if model.is_empty() {
        return String::new();
    }
    let provider = if provider_id.is_empty() {
        model.split(':').next().unwrap_or("")
    } else {
        provider_id
    };
    let Some(rest) = model.strip_prefix(&format!("{provider}:")) else {
        return model.to_owned();
    };
    if provider == "plugin" {
        let parts: Vec<&str> = rest.splitn(3, ':').collect();
        if parts.len() == 3 && !parts[2].is_empty() {
            return parts[2].to_owned();
        }
    }
    if rest.is_empty() {
        model.to_owned()
    } else {
        rest.to_owned()
    }
}

fn first_user_text(value: &Value) -> Option<String> {
    if value.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    for part in value.get("parts").and_then(Value::as_array)? {
        if part.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            return Some(text.to_owned());
        }
    }
    None
}

fn normalize_title(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn message_count(database: &dyn SessionDatabase, thread_id: &str) -> Result<i64, PluginError> {
    let rows = database
        .query(
            "SELECT COUNT(*) FROM chat_messages WHERE thread_id = ?",
            &[thread_id],
        )
        .map_err(|_| PluginError::decode("cannot count alma messages"))?;
    Ok(rows
        .values
        .first()
        .map_or(0, |row| i64::try_from(int_at(row, 0)).unwrap_or(0)))
}

fn usage_count(database: &dyn SessionDatabase, thread_id: &str) -> Result<i64, PluginError> {
    let rows = database
        .query(
            "SELECT COUNT(*) FROM usage_records WHERE thread_id = ?",
            &[thread_id],
        )
        .map_err(|_| PluginError::decode("cannot count alma usage"))?;
    Ok(rows
        .values
        .first()
        .map_or(0, |row| i64::try_from(int_at(row, 0)).unwrap_or(0)))
}

fn alma_time_ms(raw: &str) -> i64 {
    if raw.is_empty() {
        return 0;
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw.trim()) {
        return parsed.timestamp_millis();
    }
    chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%S%.fZ")
        .map_or(0, |dt| dt.and_utc().timestamp_millis())
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
        Some(SessionValue::Text(value)) => value.parse().unwrap_or(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_sqlite_database_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/root")
            .canonicalize()
            .expect("fixtures");
        let store = AlmaStore::with_root(root.clone());
        let files = store.files(&root).expect("files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].format, SessionFormat::Sqlite);
        assert!(store.locator().present());
    }
}
