//! One SQLite row per gateway request.
//!
//! The row stores the model, the upstream id, token totals, latency, and
//! status. Cache counters stay on the canonical usage object and are not
//! written here.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Days, Local, NaiveTime, TimeZone};
use rusqlite::{Connection, params};
use thiserror::Error;

/// A reporting window over the ledger.
///
/// The presets match the reference `usage` command: `Today` starts at local
/// midnight, `Week` covers today and the six days before it, and `Month`
/// covers today and the 29 days before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// Since local midnight.
    Today,
    /// The last seven calendar days, today included.
    Week,
    /// The last thirty calendar days, today included.
    Month,
    /// Every retained row.
    All,
}

impl Window {
    /// The window's first Unix second as of the local clock, or `None` for
    /// [`Window::All`].
    #[must_use]
    pub fn since_now(self) -> Option<i64> {
        window_start(self, Local::now())
    }
}

/// The first Unix second of `window` as of `now`, or `None` for
/// [`Window::All`].
///
/// Days are counted in `now`'s time zone. When local midnight does not exist
/// (a daylight-saving gap), the first instant of that day is used.
#[must_use]
pub fn window_start<Tz: TimeZone>(window: Window, now: DateTime<Tz>) -> Option<i64> {
    let back = match window {
        Window::Today => 0,
        Window::Week => 6,
        Window::Month => 29,
        Window::All => return None,
    };
    let zone = now.timezone();
    let day = now.date_naive().checked_sub_days(Days::new(back))?;
    let midnight = day.and_time(NaiveTime::MIN);
    let start = zone.from_local_datetime(&midnight).earliest().or_else(|| {
        // No local midnight: step forward to the first instant that exists.
        (1..=24 * 60).find_map(|minutes| {
            zone.from_local_datetime(&(midnight + chrono::Duration::minutes(minutes)))
                .earliest()
        })
    })?;
    Some(start.timestamp())
}

/// Aggregate counts over every row in a window, independent of any row cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Number of requests.
    pub requests: u64,
    /// Sum of known non-negative input token counts.
    pub input_tokens: u64,
    /// Sum of known non-negative output token counts.
    pub output_tokens: u64,
}

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
    /// Authenticated gateway key id, or empty for a legacy caller.
    pub caller_key_id: String,
    /// Authenticated gateway key name, or empty for a legacy caller.
    pub caller_key_name: String,
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
                    status TEXT NOT NULL,
                    caller_key_id TEXT NOT NULL DEFAULT '',
                    caller_key_name TEXT NOT NULL DEFAULT ''
                );",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        for column in ["caller_key_id", "caller_key_name"] {
            ensure_column(&connection, column)?;
        }
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
                    created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status,
                    caller_key_id, caller_key_name
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.created_unix,
                    row.model,
                    row.upstream,
                    row.input_tokens,
                    row.output_tokens,
                    row.latency_ms,
                    row.status,
                    row.caller_key_id,
                    row.caller_key_name,
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
                "SELECT created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status,
                        caller_key_id, caller_key_name
                 FROM requests ORDER BY id",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map([], read_row)
            .map_err(|error| UsageError(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))
    }

    /// The newest `row_limit` rows at or after `since` (oldest first), and
    /// totals over every row in the window.
    ///
    /// Totals come from one SQL aggregate, so they stay complete when the row
    /// list is capped. Negative token counts count as zero.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when a query fails.
    pub fn summary(
        &self,
        since: Option<i64>,
        row_limit: usize,
    ) -> Result<(Vec<Row>, Totals), UsageError> {
        let since = since.unwrap_or(i64::MIN);
        let limit = i64::try_from(row_limit).unwrap_or(i64::MAX);
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        let totals = connection
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(MAX(input_tokens, 0)), 0),
                        COALESCE(SUM(MAX(output_tokens, 0)), 0)
                 FROM requests WHERE created_unix >= ?1",
                params![since],
                |row| {
                    let count: i64 = row.get(0)?;
                    let input: i64 = row.get(1)?;
                    let output: i64 = row.get(2)?;
                    Ok(Totals {
                        requests: u64::try_from(count).unwrap_or(0),
                        input_tokens: u64::try_from(input).unwrap_or(0),
                        output_tokens: u64::try_from(output).unwrap_or(0),
                    })
                },
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let mut statement = connection
            .prepare(
                "SELECT created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status,
                        caller_key_id, caller_key_name
                 FROM requests WHERE created_unix >= ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map(params![since, limit], read_row)
            .map_err(|error| UsageError(error.to_string()))?;
        let mut rows = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))?;
        rows.reverse();
        Ok((rows, totals))
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

fn ensure_column(connection: &Connection, column: &str) -> Result<(), UsageError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(requests)")
        .map_err(|error| UsageError(error.to_string()))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| UsageError(error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| UsageError(error.to_string()))?;
    if columns.iter().any(|existing| existing == column) {
        return Ok(());
    }
    connection
        .execute(
            &format!("ALTER TABLE requests ADD COLUMN {column} TEXT NOT NULL DEFAULT ''"),
            [],
        )
        .map(|_| ())
        .map_err(|error| UsageError(error.to_string()))
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        created_unix: row.get(0)?,
        model: row.get(1)?,
        upstream: row.get(2)?,
        input_tokens: row.get(3)?,
        output_tokens: row.get(4)?,
        latency_ms: row.get(5)?,
        status: row.get(6)?,
        caller_key_id: row.get(7)?,
        caller_key_name: row.get(8)?,
    })
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
    use chrono::{FixedOffset, TimeZone};

    use super::{Ledger, Row, Totals, Window, window_start};

    fn row(created_unix: i64, input: Option<i64>, output: Option<i64>) -> Row {
        Row {
            created_unix,
            model: format!("m{created_unix}"),
            upstream: "up".to_owned(),
            input_tokens: input,
            output_tokens: output,
            latency_ms: 1,
            status: "200".to_owned(),
            caller_key_id: String::new(),
            caller_key_name: String::new(),
        }
    }

    #[test]
    fn windows_start_at_local_midnight() {
        let zone = FixedOffset::east_opt(-4 * 3600).expect("offset");
        let now = zone
            .with_ymd_and_hms(2026, 10, 28, 13, 5, 7)
            .single()
            .expect("now");
        let midnight = zone
            .with_ymd_and_hms(2026, 10, 28, 0, 0, 0)
            .single()
            .expect("midnight")
            .timestamp();
        assert_eq!(window_start(Window::Today, now), Some(midnight));
        assert_eq!(window_start(Window::Week, now), Some(midnight - 6 * 86_400));
        assert_eq!(
            window_start(Window::Month, now),
            Some(midnight - 29 * 86_400)
        );
        assert_eq!(window_start(Window::All, now), None);
        let at_midnight = zone
            .with_ymd_and_hms(2026, 10, 28, 0, 0, 0)
            .single()
            .expect("at midnight");
        assert_eq!(window_start(Window::Today, at_midnight), Some(midnight));
    }

    #[test]
    fn summary_caps_rows_but_not_totals() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("open");
        ledger.record(&row(5, Some(100), Some(100))).expect("old");
        ledger.record(&row(10, Some(1), None)).expect("first");
        ledger
            .record(&row(11, Some(-7), Some(2)))
            .expect("negative");
        ledger.record(&row(12, None, Some(3))).expect("third");
        let (rows, totals) = ledger.summary(Some(10), 2).expect("summary");
        assert_eq!(
            rows.iter().map(|row| row.created_unix).collect::<Vec<_>>(),
            [11, 12]
        );
        assert_eq!(
            totals,
            Totals {
                requests: 3,
                input_tokens: 1,
                output_tokens: 5,
            }
        );
        let (rows, totals) = ledger.summary(None, 10).expect("all");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].created_unix, 5);
        assert_eq!(totals.requests, 4);
        assert_eq!(totals.input_tokens, 101);
        let (rows, totals) = ledger.summary(Some(13), 10).expect("empty");
        assert!(rows.is_empty());
        assert_eq!(totals, Totals::default());
    }

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
                caller_key_id: String::new(),
                caller_key_name: String::new(),
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
                caller_key_id: String::new(),
                caller_key_name: String::new(),
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

    #[test]
    fn k5_ledger_adds_key_columns_to_existing_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("usage.sqlite");
        {
            let connection = rusqlite::Connection::open(&path).expect("old database");
            connection
                .execute_batch(
                    "CREATE TABLE requests (
                        id INTEGER PRIMARY KEY,
                        created_unix INTEGER NOT NULL,
                        model TEXT NOT NULL,
                        upstream TEXT NOT NULL,
                        input_tokens INTEGER,
                        output_tokens INTEGER,
                        latency_ms INTEGER NOT NULL,
                        status TEXT NOT NULL
                    );
                    INSERT INTO requests VALUES (1, 1, 'old', 'up', NULL, NULL, 1, '200');",
                )
                .expect("old schema");
        }
        let ledger = Ledger::open(&path).expect("migrate");
        let rows = ledger.rows().expect("old row");
        assert_eq!(rows[0].caller_key_id, "");
        let mut attributed = row(2, None, None);
        attributed.caller_key_id = "key-id".to_owned();
        attributed.caller_key_name = "Key name".to_owned();
        ledger.record(&attributed).expect("new row");
        let rows = ledger.rows().expect("rows");
        assert_eq!(rows[1].caller_key_id, "key-id");
        assert_eq!(rows[1].caller_key_name, "Key name");
    }
}
