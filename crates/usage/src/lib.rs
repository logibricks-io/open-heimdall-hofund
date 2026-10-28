//! One SQLite row per gateway request.
//!
//! The row stores the model, the upstream id, token totals, latency, and
//! status. Cache counters stay on the canonical usage object and are not
//! written here.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, params};
use thiserror::Error;

/// A stored request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Unix seconds when the row was written.
    pub created_unix: i64,
    /// Model the client requested.
    pub model: String,
    /// Provider id that answered, or empty when none did.
    pub upstream: String,
    /// Input tokens, when the upstream reported them.
    pub input_tokens: Option<i64>,
    /// Output tokens, when the upstream reported them.
    pub output_tokens: Option<i64>,
    /// Milliseconds from dispatch to the recorded outcome.
    pub latency_ms: i64,
    /// HTTP status, as text.
    pub status: String,
}

/// The usage database.
pub struct Ledger {
    connection: Mutex<Connection>,
}

/// A failure while opening or updating the database.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct UsageError(String);

impl Ledger {
    /// Opens `path`, creating the file and the table when needed.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the file or the schema cannot be opened.
    pub fn open(path: &Path) -> Result<Self, UsageError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| UsageError(error.to_string()))?;
        }
        let connection = Connection::open(path).map_err(|error| UsageError(error.to_string()))?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS requests (
                    id INTEGER PRIMARY KEY,
                    created_unix INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    upstream TEXT NOT NULL,
                    input_tokens INTEGER,
                    output_tokens INTEGER,
                    latency_ms INTEGER NOT NULL,
                    status TEXT NOT NULL
                );",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Inserts `row`.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the insert fails.
    pub fn record(&self, row: &Row) -> Result<(), UsageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        connection
            .execute(
                "INSERT INTO requests (
                    created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.created_unix,
                    row.model,
                    row.upstream,
                    row.input_tokens,
                    row.output_tokens,
                    row.latency_ms,
                    row.status,
                ],
            )
            .map(|_| ())
            .map_err(|error| UsageError(error.to_string()))
    }

    /// Every row, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the query fails.
    pub fn rows(&self) -> Result<Vec<Row>, UsageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        let mut statement = connection
            .prepare(
                "SELECT created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status
                 FROM requests ORDER BY id",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                Ok(Row {
                    created_unix: row.get(0)?,
                    model: row.get(1)?,
                    upstream: row.get(2)?,
                    input_tokens: row.get(3)?,
                    output_tokens: row.get(4)?,
                    latency_ms: row.get(5)?,
                    status: row.get(6)?,
                })
            })
            .map_err(|error| UsageError(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))
    }

    /// A text table of every row.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the query fails.
    pub fn table(&self) -> Result<String, UsageError> {
        Ok(format_table(&self.rows()?))
    }
}

/// Renders `rows` as a fixed-column table.
#[must_use]
pub fn format_table(rows: &[Row]) -> String {
    let header = [
        "MODEL",
        "UPSTREAM",
        "IN",
        "OUT",
        "LATENCY_MS",
        "STATUS",
        "TIME",
    ];
    let body: Vec<[String; 7]> = rows
        .iter()
        .map(|row| {
            [
                row.model.clone(),
                row.upstream.clone(),
                token(row.input_tokens),
                token(row.output_tokens),
                row.latency_ms.to_string(),
                row.status.clone(),
                local_stamp(row.created_unix),
            ]
        })
        .collect();
    let mut widths = header.map(str::len);
    for cells in &body {
        for (width, cell) in widths.iter_mut().zip(cells) {
            *width = (*width).max(cell.len());
        }
    }
    let mut out = String::new();
    push_line(&mut out, &header.map(str::to_owned), &widths);
    for cells in body {
        push_line(&mut out, &cells, &widths);
    }
    let input: i64 = rows.iter().filter_map(|row| row.input_tokens).sum();
    let output: i64 = rows.iter().filter_map(|row| row.output_tokens).sum();
    out.push_str(&format!(
        "totals  requests={}  in={input}  out={output}\n",
        rows.len()
    ));
    out
}

fn local_stamp(unix: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| unix.to_string())
}

fn token(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn push_line(out: &mut String, cells: &[String; 7], widths: &[usize; 7]) {
    for (index, (cell, width)) in cells.iter().zip(widths).enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index + 1 < cells.len() {
            out.extend(std::iter::repeat_n(' ', width.saturating_sub(cell.len())));
        }
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::{Ledger, Row};

    #[test]
    fn a_row_round_trips_into_the_table() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("usage.sqlite");
        let ledger = Ledger::open(&path).expect("open");
        ledger
            .record(&Row {
                created_unix: 10,
                model: "demo".to_owned(),
                upstream: "primary".to_owned(),
                input_tokens: Some(3),
                output_tokens: None,
                latency_ms: 12,
                status: "200".to_owned(),
            })
            .expect("insert");
        let rows = ledger.rows().expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].input_tokens, Some(3));
        assert_eq!(rows[0].output_tokens, None);
        ledger
            .record(&Row {
                created_unix: 20,
                model: "other".to_owned(),
                upstream: "backup".to_owned(),
                input_tokens: Some(2),
                output_tokens: Some(4),
                latency_ms: 1,
                status: "200".to_owned(),
            })
            .expect("second");
        let table = ledger.table().expect("table");
        assert!(table.contains("demo"));
        assert!(table.contains("primary"));
        assert!(
            table.contains("1969-") || table.contains("1970-"),
            "{table}"
        );
        assert!(table.contains("totals  requests=2  in=5  out=4"), "{table}");
    }
}
