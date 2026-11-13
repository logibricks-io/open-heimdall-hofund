//! Alma session discovery and read-only `chat_threads.db` parsing.

#![deny(missing_docs, unreachable_pub, rust_2018_idioms)]
#![warn(
    clippy::pedantic,
    clippy::missing_errors_doc,
    clippy::must_use_candidate
)]

use std::path::{Path, PathBuf};

use hofund_plugin_api::{
    PluginError, SessionDatabase, SessionDay, SessionFile, SessionFormat, SessionLocator,
    SessionParser, SessionRecord, SessionStore, SessionTokens, SessionValue,
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
    let sql = "SELECT model, provider_id, input_tokens, output_tokens, cached_input_tokens, cache_write_input_tokens, date, timestamp FROM usage_records WHERE thread_id = ?";
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
        let date = usage_date(&text_at(row, 6), &text_at(row, 7), record.last_ms);
        record.add_usage(name.clone(), tokens);
        record.add_day(SessionDay::new(date, name, tokens));
    }
    let sql = "SELECT model, provider_id, input_tokens, output_tokens, date, timestamp FROM aux_usage_records WHERE thread_id = ?";
    let rows = database
        .query(sql, &[thread_id])
        .map_err(|_| PluginError::decode("cannot read alma aux usage"))?;
    for row in &rows.values {
        let model = text_at(row, 0);
        let provider = text_at(row, 1);
        let input = i64::try_from(int_at(row, 2)).unwrap_or(0).max(0);
        let output = i64::try_from(int_at(row, 3)).unwrap_or(0).max(0);
        let name = alma_model(&model, &provider);
        let tokens = SessionTokens::new(
            u64::try_from(input).unwrap_or(0),
            u64::try_from(output).unwrap_or(0),
            0,
            0,
        );
        let date = usage_date(&text_at(row, 4), &text_at(row, 5), record.last_ms);
        record.add_usage(name.clone(), tokens);
        record.add_day(SessionDay::new(date, name, tokens));
    }
    Ok(())
}

fn usage_date(date: &str, timestamp: &str, fallback_ms: i64) -> String {
    let ms = alma_time_ms(timestamp);
    if let Some(at) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        && ms > 0
    {
        return at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        return date.format("%Y-%m-%d").to_string();
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(fallback_ms)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
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
    use hofund_plugin_api::SessionRows;

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

    #[test]
    fn present_is_false_without_the_threads_database() {
        let dir = tempfile::tempdir().expect("temp");
        let locator = AlmaLocator {
            root: dir.path().to_path_buf(),
        };
        assert!(!locator.present());
        std::fs::write(dir.path().join("chat_threads.db"), b"").expect("db file");
        assert!(locator.present());
    }

    #[test]
    fn config_and_home_paths_are_not_empty() {
        assert!(!alma_config_dir().as_os_str().is_empty());
        assert!(!alma_default_config_dir().as_os_str().is_empty());
        assert!(!user_home().as_os_str().is_empty());
        if let Some(home) = std::env::var_os("ALMA_HOME").filter(|value| !value.is_empty()) {
            assert_eq!(alma_config_dir(), PathBuf::from(home));
        }
    }

    struct FakeDb {
        threads: Vec<Vec<SessionValue>>,
        message_counts: std::collections::BTreeMap<String, i64>,
        usage_counts: std::collections::BTreeMap<String, i64>,
        messages: std::collections::BTreeMap<String, Vec<String>>,
        usage: std::collections::BTreeMap<String, Vec<Vec<SessionValue>>>,
        aux: std::collections::BTreeMap<String, Vec<Vec<SessionValue>>>,
    }

    impl FakeDb {
        fn empty() -> Self {
            Self {
                threads: Vec::new(),
                message_counts: std::collections::BTreeMap::new(),
                usage_counts: std::collections::BTreeMap::new(),
                messages: std::collections::BTreeMap::new(),
                usage: std::collections::BTreeMap::new(),
                aux: std::collections::BTreeMap::new(),
            }
        }
    }

    impl SessionDatabase for FakeDb {
        fn query(&self, sql: &str, params: &[&str]) -> Result<SessionRows, PluginError> {
            if sql.contains("FROM chat_threads") {
                return Ok(SessionRows::new(self.threads.clone(), false));
            }
            let id = params.first().copied().unwrap_or("");
            if sql.contains("COUNT(*) FROM chat_messages") {
                let count = self.message_counts.get(id).copied().unwrap_or(0);
                return Ok(SessionRows::new(
                    vec![vec![SessionValue::Integer(count)]],
                    false,
                ));
            }
            if sql.contains("COUNT(*) FROM usage_records") {
                let count = self.usage_counts.get(id).copied().unwrap_or(0);
                return Ok(SessionRows::new(
                    vec![vec![SessionValue::Integer(count)]],
                    false,
                ));
            }
            if sql.contains("FROM chat_messages") {
                let rows = self
                    .messages
                    .get(id)
                    .map(|messages| {
                        messages
                            .iter()
                            .map(|message| vec![SessionValue::Text(message.clone())])
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(SessionRows::new(rows, false));
            }
            if sql.contains("FROM aux_usage_records") {
                return Ok(SessionRows::new(
                    self.aux.get(id).cloned().unwrap_or_default(),
                    false,
                ));
            }
            if sql.contains("FROM usage_records") {
                return Ok(SessionRows::new(
                    self.usage.get(id).cloned().unwrap_or_default(),
                    false,
                ));
            }
            Err(PluginError::decode("unexpected alma test query"))
        }

        fn revision(&self) -> Result<i64, PluginError> {
            Ok(1)
        }
    }

    fn thread_row(id: &str, title: &str) -> Vec<SessionValue> {
        vec![
            SessionValue::Text(id.into()),
            SessionValue::Text(title.into()),
            SessionValue::Text(String::new()),
            SessionValue::Text(String::new()),
            SessionValue::Text(String::new()),
        ]
    }

    fn user_message(text: &str) -> String {
        format!(r#"{{"role":"user","parts":[{{"type":"text","text":"{text}"}}]}}"#)
    }

    #[test]
    fn list_threads_keeps_rows_with_only_messages_or_only_usage() {
        let mut db = FakeDb::empty();
        db.threads = vec![
            thread_row("keep-msg", "m"),
            thread_row("keep-usage", "u"),
            thread_row("skip-empty", "s"),
            thread_row("", "no-id"),
        ];
        db.message_counts.extend([
            ("keep-msg".into(), 2),
            ("keep-usage".into(), 0),
            ("skip-empty".into(), 0),
            (String::new(), 4),
        ]);
        db.usage_counts.extend([
            ("keep-msg".into(), 0),
            ("keep-usage".into(), 3),
            ("skip-empty".into(), 0),
            (String::new(), 3),
        ]);
        let threads = list_threads(&db).expect("threads");
        let ids: Vec<_> = threads.iter().map(|thread| thread.id.as_str()).collect();
        assert_eq!(ids, ["keep-msg", "keep-usage"]);
    }

    #[test]
    fn title_from_falls_back_when_stored_is_placeholder() {
        let mut db = FakeDb::empty();
        db.messages
            .insert("t1".into(), vec![user_message("hello from db")]);
        assert_eq!(
            title_from("New Chat", &db, "t1").expect("title"),
            "hello from db"
        );
        assert_eq!(
            title_from("   ", &db, "t1").expect("title"),
            "hello from db"
        );
        assert_eq!(title_from("Saved", &db, "t1").expect("title"), "Saved");
    }

    #[test]
    fn apply_first_prompt_fills_an_empty_title() {
        let mut db = FakeDb::empty();
        db.messages
            .insert("t1".into(), vec![user_message("prompt title")]);
        let mut record = SessionRecord::new("alma", "alma:t1", "x");
        record.title.clear();
        apply_first_prompt(&db, "t1", &mut record).expect("prompt");
        assert_eq!(record.title, "prompt title");
    }

    #[test]
    fn apply_usage_subtracts_cached_input_tokens_and_adds_daily_totals() {
        let mut db = FakeDb::empty();
        db.usage.insert(
            "t1".into(),
            vec![vec![
                SessionValue::Text("gpt".into()),
                SessionValue::Text(String::new()),
                SessionValue::Integer(10),
                SessionValue::Integer(2),
                SessionValue::Integer(3),
                SessionValue::Integer(1),
                SessionValue::Text("2026-10-01".into()),
                SessionValue::Text("2026-10-01T10:00:05.000Z".into()),
            ]],
        );
        db.aux.insert(
            "t1".into(),
            vec![vec![
                SessionValue::Text("aux".into()),
                SessionValue::Text(String::new()),
                SessionValue::Integer(4),
                SessionValue::Integer(1),
                SessionValue::Text("2026-10-02".into()),
                SessionValue::Text(String::new()),
            ]],
        );
        let mut record = SessionRecord::new("alma", "alma:t1", "x");
        apply_usage(&db, "t1", &mut record).expect("usage");
        let tokens = record.tokens();
        assert_eq!(tokens.input, 11);
        assert_eq!(tokens.output, 3);
        assert_eq!(tokens.cache_read, 3);
        assert_eq!(tokens.cache_write, 1);
        assert_eq!(record.models.get("gpt").map(|t| t.input), Some(7));
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-01T10:00:05.000Z")
            .expect("timestamp")
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
        let day = record
            .days
            .iter()
            .find(|day| day.model == "gpt")
            .expect("usage day");
        assert_eq!(day.date, expected);
        assert_eq!(day.tokens, SessionTokens::new(7, 2, 3, 1));
        let aux = record
            .days
            .iter()
            .find(|day| day.model == "aux")
            .expect("aux day");
        assert_eq!(aux.date, "2026-10-02");
        assert_eq!(aux.tokens, SessionTokens::new(4, 1, 0, 0));
    }

    #[test]
    fn alma_model_strips_provider_and_plugin_suffix() {
        assert_eq!(alma_model("", "openai"), "");
        assert_eq!(alma_model("gpt-4", "openai"), "gpt-4");
        assert_eq!(alma_model("openai:gpt-4", "openai"), "gpt-4");
        assert_eq!(alma_model("plugin:pack:slot:real", "plugin"), "real");
        assert_eq!(alma_model("plugin:pack:slot:", "plugin"), "pack:slot:");
        assert_eq!(alma_model("plugin:only", "plugin"), "only");
    }

    #[test]
    fn first_user_text_reads_non_empty_user_text_parts() {
        let user = json::from_str::<Value>(&user_message("hello")).expect("json");
        assert_eq!(first_user_text(&user).as_deref(), Some("hello"));
        let assistant = json::from_str::<Value>(
            r#"{"role":"assistant","parts":[{"type":"text","text":"nope"}]}"#,
        )
        .expect("json");
        assert_eq!(first_user_text(&assistant), None);
        let blank = json::from_str::<Value>(&user_message("   ")).expect("json");
        assert_eq!(first_user_text(&blank), None);
        let other =
            json::from_str::<Value>(r#"{"role":"user","parts":[{"type":"image","text":"hello"}]}"#)
                .expect("json");
        assert_eq!(first_user_text(&other), None);
    }

    #[test]
    fn counts_and_time_helpers_parse_values() {
        let mut db = FakeDb::empty();
        db.message_counts.insert("t1".into(), 5);
        db.usage_counts.insert("t1".into(), 4);
        assert_eq!(message_count(&db, "t1").expect("messages"), 5);
        assert_eq!(usage_count(&db, "t1").expect("usage"), 4);
        assert_eq!(alma_time_ms(""), 0);
        let raw = "2020-01-02T03:04:05.000Z";
        let ms = alma_time_ms(raw);
        let expected = chrono::DateTime::parse_from_rfc3339(raw)
            .expect("rfc3339")
            .timestamp_millis();
        assert_eq!(ms, expected);
        assert!(ms > 1);
    }

    #[test]
    fn text_at_and_int_at_cover_value_shapes() {
        let row = vec![
            SessionValue::Text("7".into()),
            SessionValue::Integer(8),
            SessionValue::Real(9.0),
            SessionValue::Blob(b"blob".to_vec()),
            SessionValue::Null,
        ];
        assert_eq!(text_at(&row, 0), "7");
        assert_eq!(text_at(&row, 1), "8");
        assert_eq!(text_at(&row, 2), "9");
        assert_eq!(text_at(&row, 3), "blob");
        assert_eq!(text_at(&row, 4), "");
        assert_eq!(int_at(&row, 0), 7);
        assert_eq!(int_at(&row, 1), 8);
        assert_eq!(int_at(&[], 0), 0);
        assert_eq!(int_at(&[SessionValue::Null], 0), 0);
    }
}
