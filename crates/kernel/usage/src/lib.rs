//! One SQLite row per gateway request.
//!
//! The row stores the model, the upstream id, token totals, latency, and
//! status. Additive columns also keep uncached input and cache counters for
//! gateway-key budgets while ordinary rows retain protocol input totals.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Days, Local, NaiveTime, TimeZone};
use rusqlite::{Connection, params};
use thiserror::Error;

mod attribution;
mod decode;
mod fx;
pub mod gateway_sessions;
mod key;
mod limits;
mod price;
mod report;
mod speed;
mod spent;
pub use attribution::{
    BreakdownDimension, BreakdownGroup, NO_GATEWAY_KEY, NO_PROVIDER_KEY, UNKNOWN_CLIENT_AGENT,
    masked_provider_key_id, normalize_client_agent,
};
pub use decode::{FlowTracker, decode_of, decode_row, decode_window, row_tokens_per_second};
pub use fx::{CnyRate, DisplayCurrency, Fx, RateFetcher};
pub use gateway_sessions::{GatewayModelTotals, GatewaySession, gateway_sessions, local_date};
pub use key::{CallerModel, CallerTotals};
pub use limits::{KeyUsage, UsageDetails};
pub use price::{RowPricing, TokenPrice, token_bearing};
pub use report::{
    CSV_HEADER, ReportWindow, SpeedTotals, account_of_upstream, csv_field, format_csv, parse_days,
    parse_last, parse_since, row_matches_filter, speed_totals,
};
pub use speed::{mean_tokens_per_second, mean_ttft_ms};
pub use spent::{KeySpent, SpendCapPeriod, spend_cap_retry_after, spend_cap_since};

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

/// Outcome marker for a try handed to another member after billing tokens.
pub const OUTCOME_HANDED_ON: &str = "handed_on";

/// Whether a row counts toward request totals.
#[must_use]
pub fn counts_as_request(outcome: &str) -> bool {
    !matches!(outcome, OUTCOME_HANDED_ON | "refusal" | "turned_away")
}

/// Aggregate counts over every row in a window, independent of any row cap.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Totals {
    /// Number of requests.
    pub requests: u64,
    /// Sum of known non-negative input token counts.
    pub input_tokens: u64,
    /// Sum of known non-negative output token counts.
    pub output_tokens: u64,
    /// Sum of cache-read tokens.
    pub cache_read_tokens: u64,
    /// Sum of five-minute cache-write tokens.
    pub cache_write_5m_tokens: u64,
    /// Sum of one-hour cache-write tokens.
    pub cache_write_1h_tokens: u64,
    /// Sum of reasoning tokens reported in usage.
    pub reasoning_tokens: u64,
    /// Sum of stored USD costs for priced rows.
    pub cost_usd: f64,
    /// Rows with token counts but no stored list price.
    pub unpriced: u64,
}

/// A stored request.
#[derive(Clone, Debug, Default, PartialEq)]
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
    /// Issuer of the authenticated key, or empty for a direct caller.
    pub caller_issuer_id: String,
    /// Cache-read tokens when stored.
    pub cache_read_tokens: Option<i64>,
    /// Five-minute cache-write tokens when stored.
    pub cache_write_5m_tokens: Option<i64>,
    /// One-hour cache-write tokens when stored.
    pub cache_write_1h_tokens: Option<i64>,
    /// Reasoning tokens when stored.
    pub reasoning_tokens: Option<i64>,
    /// Whether reasoning streamed without a usage count.
    pub reasoning_hidden: bool,
    /// Whether a list price was stored for this row.
    pub priced: bool,
    /// Stored USD cost when [`Self::priced`] is true.
    pub cost_usd: Option<f64>,
    /// List price per million tokens stored on the row.
    pub price: Option<TokenPrice>,
    /// Row kind for handed-off, refusal, or turned-away tries; empty for ordinary rows.
    pub outcome: String,
    /// Five-byte API-key fingerprint of the provider key that served the request.
    /// Empty when the member has no API key.
    pub provider_key_id: String,
    /// Client agent that sent the request, or [`UNKNOWN_CLIENT_AGENT`] when unset.
    pub client_agent: String,
    /// Time to first content in milliseconds when measured.
    pub ttft_ms: Option<i64>,
    /// First visible text on the stream, in milliseconds from start.
    pub first_text_ms: Option<i64>,
    /// First reasoning delta on the stream, in milliseconds from start.
    pub first_reasoning_ms: Option<i64>,
    /// Content delivery span used for burst detection, in milliseconds.
    pub flow_ms: Option<i64>,
    /// Model id reported by the upstream reply when recorded.
    pub served_model: String,
    /// Client session id from gateway headers when recorded.
    pub session_id: String,
    /// First native session header, when one was sent.
    pub native_session_id: String,
    /// Request path for hub tool calls.
    pub endpoint: String,
    /// Empty for model calls, `mcp` for hub tool calls.
    pub kind: String,
}

/// One successful request as routing measurements read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingRow {
    /// Unix seconds when the row was written.
    pub created_unix: i64,
    /// Client-facing model id.
    pub model: String,
    /// Member id that answered.
    pub upstream: String,
    /// Input tokens when reported.
    pub input_tokens: Option<i64>,
    /// Output tokens when reported.
    pub output_tokens: Option<i64>,
    /// Whole answer duration in milliseconds.
    pub latency_ms: i64,
    /// Time to first content in milliseconds, when streamed and measured.
    pub ttft_ms: Option<i64>,
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
                    caller_key_name TEXT NOT NULL DEFAULT '',
                    caller_issuer_id TEXT NOT NULL DEFAULT '',
                    ttft_ms INTEGER
                );",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        for (column, declaration) in [
            ("caller_key_id", "TEXT NOT NULL DEFAULT ''"),
            ("caller_key_name", "TEXT NOT NULL DEFAULT ''"),
            ("caller_issuer_id", "TEXT NOT NULL DEFAULT ''"),
            ("ttft_ms", "INTEGER"),
            ("uncached_input_tokens", "INTEGER"),
            ("cache_read_tokens", "INTEGER"),
            ("cache_write_tokens", "INTEGER"),
            ("cache_write_5m_tokens", "INTEGER"),
            ("cache_write_1h_tokens", "INTEGER"),
            ("reasoning_tokens", "INTEGER"),
            ("reasoning_hidden", "INTEGER NOT NULL DEFAULT 0"),
            ("priced", "INTEGER NOT NULL DEFAULT 0"),
            ("cost_usd", "REAL"),
            ("price_input", "REAL"),
            ("price_output", "REAL"),
            ("price_cache_read", "REAL"),
            ("price_cache_write", "REAL"),
            ("price_cache_write_1h", "REAL"),
            ("outcome", "TEXT NOT NULL DEFAULT ''"),
            ("provider_key_id", "TEXT NOT NULL DEFAULT ''"),
            ("client_agent", "TEXT NOT NULL DEFAULT ''"),
            ("session_id", "TEXT"),
            ("native_session_id", "TEXT"),
            ("endpoint", "TEXT"),
            ("kind", "TEXT NOT NULL DEFAULT ''"),
            ("first_text_ms", "INTEGER"),
            ("first_reasoning_ms", "INTEGER"),
            ("flow_ms", "INTEGER"),
            ("served_model", "TEXT NOT NULL DEFAULT ''"),
        ] {
            ensure_column(&connection, column, declaration)?;
        }
        // Routing reads recent rows on every weighed request; the index keeps
        // that read to the window it asks for.
        connection
            .execute(
                "CREATE INDEX IF NOT EXISTS requests_created_unix ON requests(created_unix)",
                [],
            )
            .map_err(|error| UsageError(error.to_string()))?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Inserts `row` without a first-content measurement.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the insert fails.
    pub fn record(&self, row: &Row) -> Result<(), UsageError> {
        self.record_with_ttft(row, None)
    }

    /// Inserts `row` with an optional first-content latency in milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the insert fails.
    pub fn record_with_ttft(&self, row: &Row, ttft_ms: Option<i64>) -> Result<(), UsageError> {
        self.record_with_details(row, ttft_ms, UsageDetails::default(), None)
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
                        caller_key_id, caller_key_name, caller_issuer_id,
                        cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens,
                        reasoning_tokens, reasoning_hidden,
                        priced, cost_usd, price_input, price_output, price_cache_read,
                        price_cache_write, price_cache_write_1h, outcome, provider_key_id,
                        client_agent, ttft_ms, session_id, native_session_id, endpoint, kind,
                        first_text_ms, first_reasoning_ms, flow_ms, served_model
                 FROM requests ORDER BY id",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map([], read_row)
            .map_err(|error| UsageError(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))
    }

    /// Each upstream's newest answered request at or after `since`: rows
    /// with a 2xx status and an upstream, as `(upstream, created_unix)`.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the query fails.
    pub fn last_served(&self, since: i64) -> Result<Vec<(String, i64)>, UsageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        let mut statement = connection
            .prepare(
                "SELECT upstream, MAX(created_unix) FROM requests
                 WHERE created_unix >= ?1 AND upstream != '' AND status LIKE '2%'
                 GROUP BY upstream ORDER BY upstream",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map([since], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|error| UsageError(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))
    }

    /// Successful rows at or after `since`, including routing speed fields.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the query fails.
    pub fn routing_rows(&self, since: i64) -> Result<Vec<RoutingRow>, UsageError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        let mut statement = connection
            .prepare(
                "SELECT created_unix, model, upstream, input_tokens, output_tokens, latency_ms, ttft_ms
                 FROM requests
                 WHERE created_unix >= ?1 AND upstream != '' AND status LIKE '2%'
                 ORDER BY id",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map([since], |row| {
                Ok(RoutingRow {
                    created_unix: row.get(0)?,
                    model: row.get(1)?,
                    upstream: row.get(2)?,
                    input_tokens: row.get(3)?,
                    output_tokens: row.get(4)?,
                    latency_ms: row.get(5)?,
                    ttft_ms: row.get(6)?,
                })
            })
            .map_err(|error| UsageError(error.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))
    }

    /// The newest `row_limit` rows at or after `since` (newest first), and
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
                "SELECT COUNT(CASE WHEN outcome NOT IN ('handed_on', 'refusal', 'turned_away') OR outcome = '' THEN 1 END),
                        COALESCE(SUM(MAX(input_tokens, 0)), 0),
                        COALESCE(SUM(MAX(output_tokens, 0)), 0),
                        COALESCE(SUM(MAX(cache_read_tokens, 0)), 0),
                        COALESCE(SUM(MAX(cache_write_5m_tokens, 0)), 0),
                        COALESCE(SUM(MAX(cache_write_1h_tokens, 0)), 0),
                        COALESCE(SUM(MAX(reasoning_tokens, 0)), 0),
                        COALESCE(SUM(CASE WHEN priced = 1 THEN cost_usd ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN priced = 0 AND (MAX(COALESCE(input_tokens, 0), 0) + MAX(COALESCE(output_tokens, 0), 0)) > 0 THEN 1 ELSE 0 END), 0)
                 FROM requests WHERE created_unix >= ?1",
                params![since],
                |row| {
                    let count: i64 = row.get(0)?;
                    let input: i64 = row.get(1)?;
                    let output: i64 = row.get(2)?;
                    let cache_read: i64 = row.get(3)?;
                    let cache_write_5m: i64 = row.get(4)?;
                    let cache_write_1h: i64 = row.get(5)?;
                    let reasoning: i64 = row.get(6)?;
                    let cost: f64 = row.get(7)?;
                    let unpriced: i64 = row.get(8)?;
                    Ok(Totals {
                        requests: u64::try_from(count).unwrap_or(0),
                        input_tokens: u64::try_from(input).unwrap_or(0),
                        output_tokens: u64::try_from(output).unwrap_or(0),
                        cache_read_tokens: u64::try_from(cache_read).unwrap_or(0),
                        cache_write_5m_tokens: u64::try_from(cache_write_5m).unwrap_or(0),
                        cache_write_1h_tokens: u64::try_from(cache_write_1h).unwrap_or(0),
                        reasoning_tokens: u64::try_from(reasoning).unwrap_or(0),
                        cost_usd: cost,
                        unpriced: u64::try_from(unpriced).unwrap_or(0),
                    })
                },
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let mut statement = connection
            .prepare(
                "SELECT created_unix, model, upstream, input_tokens, output_tokens, latency_ms, status,
                        caller_key_id, caller_key_name, caller_issuer_id,
                        cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens,
                        reasoning_tokens, reasoning_hidden,
                        priced, cost_usd, price_input, price_output, price_cache_read,
                        price_cache_write, price_cache_write_1h, outcome, provider_key_id,
                        client_agent, ttft_ms, session_id, native_session_id, endpoint, kind,
                        first_text_ms, first_reasoning_ms, flow_ms, served_model
                 FROM requests WHERE created_unix >= ?1 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map(params![since, limit], read_row)
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))?;
        Ok((rows, totals))
    }

    /// Like [`Self::summary`], with optional model and account filters.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when a query fails.
    pub fn summary_filtered(
        &self,
        since: Option<i64>,
        model: Option<&str>,
        account: Option<&str>,
        row_limit: usize,
    ) -> Result<(Vec<Row>, Totals), UsageError> {
        if model.filter(|value| !value.is_empty()).is_none()
            && account.filter(|value| !value.is_empty()).is_none()
        {
            return self.summary(since, row_limit);
        }
        let (all_rows, _) = self.summary(since, usize::MAX)?;
        let filtered: Vec<Row> = all_rows
            .into_iter()
            .filter(|row| row_matches_filter(row, model, account))
            .collect();
        let totals = totals_of_rows(&filtered);
        let keep = row_limit.min(filtered.len());
        Ok((filtered[..keep].to_vec(), totals))
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

fn ensure_column(
    connection: &Connection,
    column: &str,
    declaration: &str,
) -> Result<(), UsageError> {
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
            &format!("ALTER TABLE requests ADD COLUMN {column} {declaration}"),
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
        caller_issuer_id: row.get(9)?,
        cache_read_tokens: row.get(10)?,
        cache_write_5m_tokens: row.get(11)?,
        cache_write_1h_tokens: row.get(12)?,
        reasoning_tokens: row.get(13)?,
        reasoning_hidden: row.get::<_, i64>(14)? != 0,
        priced: row.get::<_, i64>(15)? != 0,
        cost_usd: row.get(16)?,
        price: read_price(row, 17)?,
        outcome: row.get(22)?,
        provider_key_id: row.get(23)?,
        client_agent: normalize_client_agent(&row.get::<_, String>(24)?),
        ttft_ms: row.get(25)?,
        session_id: row.get::<_, Option<String>>(26)?.unwrap_or_default(),
        native_session_id: row.get::<_, Option<String>>(27)?.unwrap_or_default(),
        endpoint: row.get::<_, Option<String>>(28)?.unwrap_or_default(),
        kind: row.get(29)?,
        first_text_ms: row.get(30)?,
        first_reasoning_ms: row.get(31)?,
        flow_ms: row.get(32)?,
        served_model: row.get(33)?,
    })
}

pub(crate) fn sql_nullable_text(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

fn totals_of_rows(rows: &[Row]) -> Totals {
    let mut totals = Totals::default();
    for row in rows {
        if matches!(
            row.outcome.as_str(),
            "handed_on" | "refusal" | "turned_away"
        ) {
            continue;
        }
        totals.requests += 1;
        totals.input_tokens += row.input_tokens.filter(|t| *t > 0).unwrap_or(0) as u64;
        totals.output_tokens += row.output_tokens.filter(|t| *t > 0).unwrap_or(0) as u64;
        totals.cache_read_tokens += row.cache_read_tokens.filter(|t| *t > 0).unwrap_or(0) as u64;
        totals.cache_write_5m_tokens +=
            row.cache_write_5m_tokens.filter(|t| *t > 0).unwrap_or(0) as u64;
        totals.cache_write_1h_tokens +=
            row.cache_write_1h_tokens.filter(|t| *t > 0).unwrap_or(0) as u64;
        totals.reasoning_tokens += row.reasoning_tokens.filter(|t| *t > 0).unwrap_or(0) as u64;
        if row.priced {
            totals.cost_usd += row.cost_usd.unwrap_or(0.0);
        } else if row.input_tokens.unwrap_or(0) + row.output_tokens.unwrap_or(0) > 0 {
            totals.unpriced += 1;
        }
    }
    totals
}

impl Ledger {
    /// Sum of stored USD for one provider key at or after `since`.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when the query fails.
    pub fn provider_key_cost(&self, provider_key_id: &str, since: i64) -> Result<f64, UsageError> {
        if provider_key_id.is_empty() {
            return Ok(0.0);
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        connection
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN priced = 1 THEN cost_usd ELSE 0 END), 0)
                 FROM requests
                 WHERE provider_key_id = ?1 AND created_unix >= ?2",
                params![provider_key_id, since],
                |row| row.get(0),
            )
            .map_err(|error| UsageError(error.to_string()))
    }
}

fn read_price(row: &rusqlite::Row<'_>, start: usize) -> rusqlite::Result<Option<TokenPrice>> {
    let input: Option<f64> = row.get(start)?;
    if input.is_none() {
        return Ok(None);
    }
    Ok(Some(TokenPrice {
        input: input.unwrap_or(0.0),
        output: row.get(start + 1)?,
        cache_read: row.get(start + 2)?,
        cache_write: row.get(start + 3)?,
        cache_write_1h: row.get(start + 4)?,
    }))
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
    let cache_read: i64 = rows.iter().filter_map(|row| row.cache_read_tokens).sum();
    let cache_write_5m: i64 = rows
        .iter()
        .filter_map(|row| row.cache_write_5m_tokens)
        .sum();
    let cache_write_1h: i64 = rows
        .iter()
        .filter_map(|row| row.cache_write_1h_tokens)
        .sum();
    let reasoning: i64 = rows.iter().filter_map(|row| row.reasoning_tokens).sum();
    out.push_str(&format!(
        "totals  requests={}  in={input}  out={output}  cache_read={cache_read}  cache_write_5m={cache_write_5m}  cache_write_1h={cache_write_1h}  reasoning={reasoning}\n",
        rows.len(),
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
            ..Default::default()
        }
    }

    #[test]
    fn q8c_last_served_is_the_newest_answered_row_per_upstream() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger");
        let served = |at: i64, upstream: &str, status: &str| Row {
            upstream: upstream.to_owned(),
            status: status.to_owned(),
            ..row(at, None, None)
        };
        for entry in [
            served(100, "account:p:a", "200"),
            served(300, "account:p:a", "201"),
            served(400, "account:p:a", "429"),
            served(50, "old", "200"),
            served(500, "", "200"),
            served(200, "plain", "200"),
            served(250, "failed", "500"),
        ] {
            ledger.record(&entry).expect("record");
        }
        assert_eq!(
            ledger.last_served(100).expect("served"),
            [("account:p:a".to_owned(), 300), ("plain".to_owned(), 200)]
        );
        assert_eq!(
            ledger.last_served(0).expect("served").len(),
            3,
            "the since bound is inclusive and old rows return"
        );
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
            [12, 11]
        );
        assert_eq!(
            totals,
            Totals {
                requests: 3,
                input_tokens: 1,
                output_tokens: 5,
                cost_usd: 0.0,
                unpriced: 3,
                ..Totals::default()
            }
        );
        let (rows, totals) = ledger.summary(None, 10).expect("all");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].created_unix, 12);
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
                caller_issuer_id: String::new(),
                ..Default::default()
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
                caller_issuer_id: String::new(),
                ..Default::default()
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

    #[test]
    fn r33_ledger_adds_gateway_session_columns() {
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
                        status TEXT NOT NULL,
                        session_id TEXT NOT NULL DEFAULT ''
                    );
                    INSERT INTO requests (created_unix, model, upstream, latency_ms, status, session_id)
                    VALUES (1, 'old', 'up', 1, '200', '');",
                )
                .expect("old schema");
        }
        let ledger = Ledger::open(&path).expect("migrate");
        let mut row = row(2, None, None);
        row.session_id = "sess".to_owned();
        row.native_session_id = "native".to_owned();
        row.endpoint = "/v1/chat".to_owned();
        row.kind = "mcp".to_owned();
        ledger.record(&row).expect("insert");
        let rows = ledger.rows().expect("rows");
        assert_eq!(rows[1].session_id, "sess");
        assert_eq!(rows[1].native_session_id, "native");
        assert_eq!(rows[1].endpoint, "/v1/chat");
        assert_eq!(rows[1].kind, "mcp");
    }

    #[test]
    fn b4c_routing_rows_keep_ttft_and_migrate_an_old_ledger() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("usage.sqlite");
        rusqlite::Connection::open(&path)
            .expect("old database")
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
                );",
            )
            .expect("old schema");
        let ledger = Ledger::open(&path).expect("migrate");
        ledger
            .record_with_ttft(
                &Row {
                    created_unix: 10,
                    model: "demo".to_owned(),
                    upstream: "primary".to_owned(),
                    input_tokens: Some(3),
                    output_tokens: Some(7),
                    latency_ms: 120,
                    status: "200".to_owned(),
                    caller_key_id: String::new(),
                    caller_key_name: String::new(),
                    caller_issuer_id: String::new(),
                    ..Default::default()
                },
                Some(12),
            )
            .expect("insert");
        let rows = ledger.routing_rows(0).expect("routing rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ttft_ms, Some(12));
        assert_eq!(rows[0].output_tokens, Some(7));
        assert_eq!(ledger.rows().expect("ordinary rows").len(), 1);
        // a window after the row reads nothing, and the read uses the index
        assert!(ledger.routing_rows(11).expect("window").is_empty());
        drop(ledger);
        let plan: String = rusqlite::Connection::open(&path)
            .expect("reopen")
            .query_row(
                "EXPLAIN QUERY PLAN SELECT id FROM requests WHERE created_unix >= 11",
                [],
                |row| row.get(3),
            )
            .expect("plan");
        assert!(plan.contains("requests_created_unix"), "{plan}");
    }
}
