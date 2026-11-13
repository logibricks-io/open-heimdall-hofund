use std::cell::Cell;
use std::fs::File;
use std::path::Path;

use hofund_plugin_api::{PluginError, SessionDatabase, SessionRows, SessionValue};
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags, params_from_iter};

use crate::open::open_session_file;

const MAX_DATABASE_ROWS: usize = 50_000;
const MAX_VALUE_BYTES: i32 = 16 * 1024 * 1024;
const MAX_COLUMNS: i32 = 64;

pub(crate) struct ReadOnlyDatabase {
    connection: Connection,
    remaining: Cell<usize>,
    _opened: File,
}

pub(crate) fn open_ro(root: &Path, path: &Path) -> Result<ReadOnlyDatabase, PluginError> {
    let opened = open_session_file(root, path)
        .map_err(|_| PluginError::io("cannot open session database"))?;
    let uri = format!("file:{}?mode=ro", escape_uri(path));
    let connection = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|_| PluginError::io("cannot open session database read-only"))?;
    connection
        .execute_batch("PRAGMA query_only = ON")
        .map_err(|_| PluginError::io("cannot bound session database"))?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES)
        .map_err(|_| PluginError::io("cannot bound session database values"))?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS)
        .map_err(|_| PluginError::io("cannot bound session database columns"))?;
    Ok(ReadOnlyDatabase {
        connection,
        remaining: Cell::new(MAX_DATABASE_ROWS),
        _opened: opened,
    })
}

fn escape_uri(path: &Path) -> String {
    path.to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23")
}

impl SessionDatabase for ReadOnlyDatabase {
    fn query(&self, sql: &str, params: &[&str]) -> Result<SessionRows, PluginError> {
        let normalized = sql.trim_start().to_ascii_lowercase();
        if !(normalized.starts_with("select") || normalized.starts_with("with"))
            || normalized
                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .any(|word| word == "attach")
        {
            return Err(PluginError::decode(
                "session database query is not read-only",
            ));
        }
        let allowed = self.remaining.get();
        if allowed == 0 {
            return Ok(SessionRows::new(Vec::new(), true));
        }
        let mut statement = self
            .connection
            .prepare(sql)
            .map_err(|_| PluginError::decode("cannot prepare session database query"))?;
        let mut rows = statement
            .query(params_from_iter(params.iter()))
            .map_err(|_| PluginError::decode("cannot query session database"))?;
        let mut values = Vec::new();
        let mut cut = false;
        while let Some(row) = rows
            .next()
            .map_err(|_| PluginError::decode("cannot read session database row"))?
        {
            if values.len() == allowed {
                cut = true;
                break;
            }
            let mut record = Vec::new();
            for index in 0..row.as_ref().column_count() {
                let value = row
                    .get_ref(index)
                    .map_err(|_| PluginError::decode("cannot read session database value"))?;
                record.push(match value {
                    rusqlite::types::ValueRef::Null => SessionValue::Null,
                    rusqlite::types::ValueRef::Integer(value) => SessionValue::Integer(value),
                    rusqlite::types::ValueRef::Real(value) => SessionValue::Real(value),
                    rusqlite::types::ValueRef::Text(value) => {
                        SessionValue::Text(String::from_utf8_lossy(value).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(value) => SessionValue::Blob(value.to_vec()),
                });
            }
            values.push(record);
        }
        self.remaining.set(allowed - values.len());
        Ok(SessionRows::new(values, cut))
    }

    fn revision(&self) -> Result<i64, PluginError> {
        self.connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(|_| PluginError::decode("cannot read session database revision"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_attach_rejected() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("sessions.db");
        Connection::open(&path)
            .expect("create")
            .execute_batch("CREATE TABLE items (id TEXT)")
            .expect("schema");
        let database = open_ro(temp.path(), &path).expect("read-only database");
        assert!(
            database
                .query("ATTACH DATABASE 'other.db' AS other", &[])
                .is_err()
        );
        assert!(!temp.path().join("other.db").exists());
    }

    #[test]
    fn sqlite_query_only_rejects_writes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("sessions.db");
        Connection::open(&path)
            .expect("create")
            .execute_batch("CREATE TABLE items (id TEXT)")
            .expect("schema");
        let database = open_ro(temp.path(), &path).expect("read-only database");
        assert!(database.query("DELETE FROM items", &[]).is_err());
        let rows = database
            .query("SELECT COUNT(*) FROM items", &[])
            .expect("count");
        assert_eq!(rows.values[0][0], SessionValue::Integer(0));
    }

    #[test]
    fn sqlite_does_not_create_user_files() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("sessions.db");
        Connection::open(&path)
            .expect("create")
            .execute_batch("CREATE TABLE items (id TEXT)")
            .expect("schema");
        let before: Vec<_> = std::fs::read_dir(temp.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        let database = open_ro(temp.path(), &path).expect("read-only database");
        database.query("SELECT * FROM items", &[]).expect("query");
        drop(database);
        let mut after: Vec<_> = std::fs::read_dir(temp.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        after.sort();
        assert_eq!(before, after);
    }
}
