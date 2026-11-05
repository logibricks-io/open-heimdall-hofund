//! Reporting windows, filters, and CSV export for the usage ledger.

use std::fmt::Write as _;

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};

use crate::speed::{mean_tokens_per_second, mean_ttft_ms};
use crate::{Ledger, Row, Totals, UsageError};

/// Subscription account label parsed from an `account:provider:name` upstream id.
#[must_use]
pub fn account_of_upstream(upstream: &str) -> Option<&str> {
    let rest = upstream.strip_prefix("account:")?;
    rest.split_once(':').map(|(_, account)| account)
}

/// Whether `row` matches optional model and account filters (case-insensitive account).
#[must_use]
pub fn row_matches_filter(row: &Row, model: Option<&str>, account: Option<&str>) -> bool {
    if let Some(want) = model.filter(|value| !value.is_empty())
        && !row.model.eq_ignore_ascii_case(want)
    {
        return false;
    }
    if let Some(want) = account.filter(|value| !value.is_empty()) {
        let Some(got) = account_of_upstream(&row.upstream) else {
            return false;
        };
        if !got.eq_ignore_ascii_case(want) {
            return false;
        }
    }
    true
}

/// How a report selects rows in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportWindow {
    /// Calendar day ending now (local midnight start).
    Today,
    /// Last seven local calendar days including today.
    Week,
    /// Last thirty local calendar days including today.
    Month,
    /// Every retained row.
    All,
    /// Rolling duration ending now.
    Rolling(Duration),
    /// Rows at or after this Unix second (inclusive).
    Since(i64),
}

impl ReportWindow {
    /// Inclusive lower bound in Unix seconds, or `None` for all time.
    #[must_use]
    pub fn since_unix(self, now: DateTime<Local>) -> Option<i64> {
        match self {
            Self::All => None,
            Self::Rolling(duration) => {
                let end = now.timestamp();
                Some(end.saturating_sub(duration.num_seconds()))
            }
            Self::Since(since) => Some(since),
            Self::Today => Some(crate::window_start(crate::Window::Today, now).expect("today")),
            Self::Week => Some(crate::window_start(crate::Window::Week, now).expect("week")),
            Self::Month => Some(crate::window_start(crate::Window::Month, now).expect("month")),
        }
    }

    /// Maps calendar presets to [`crate::Window`] values.
    #[must_use]
    pub const fn calendar_window(self) -> Option<crate::Window> {
        match self {
            Self::Today => Some(crate::Window::Today),
            Self::Week => Some(crate::Window::Week),
            Self::Month => Some(crate::Window::Month),
            Self::All => Some(crate::Window::All),
            Self::Rolling(_) | Self::Since(_) => None,
        }
    }
}

/// Parses `--since` as RFC3339 or `YYYY-MM-DD` (local midnight).
///
/// # Errors
///
/// Returns a message when the value is not a valid timestamp or date.
pub fn parse_since(value: &str) -> Result<i64, String> {
    let trimmed = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(parsed.timestamp());
    }
    let date = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .map_err(|_| format!("--since {trimmed:?} is not RFC3339 or YYYY-MM-DD"))?;
    Local
        .from_local_datetime(&date.and_time(NaiveTime::MIN))
        .single()
        .ok_or_else(|| format!("--since date {trimmed} is ambiguous in the local time zone"))
        .map(|dt| dt.timestamp())
}

/// Parses `--last` as a positive duration: Go-style units (`24h`, `90m`, `1h30m`)
/// or a whole-value day suffix (`1.5d`), which is a hofund extension beyond Go
/// `ParseDuration`.
///
/// # Errors
///
/// Returns a message when the duration is missing, not positive, or out of range.
pub fn parse_last(value: &str) -> Result<Duration, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("--last takes a duration".to_owned());
    }
    if let Some(rest) = trimmed.strip_suffix('d') {
        let days: f64 = rest
            .parse()
            .map_err(|_| format!("--last takes a duration greater than zero, not {value:?}"))?;
        if !(days.is_finite()) || days <= 0.0 {
            return Err(format!(
                "--last takes a duration greater than zero, not {value:?}"
            ));
        }
        let seconds_f = days * 86_400.0;
        if !seconds_f.is_finite() || seconds_f >= i64::MAX as f64 {
            return Err(format!("--last takes at most 100 years, not {value:?}"));
        }
        return finish_last_seconds(seconds_f.round() as i64, value);
    }
    if !duration_has_unit(trimmed) {
        return Err(format!(
            "--last takes a duration greater than zero, not {value:?}"
        ));
    }
    let ms = parse_go_duration(trimmed, "--last")?;
    finish_last_milliseconds(ms, value)
}

fn max_last_seconds() -> i64 {
    (100_f64 * 365_f64 * 86_400_f64).round() as i64
}

fn finish_last_seconds(seconds: i64, value: &str) -> Result<Duration, String> {
    if seconds <= 0 {
        return Err(format!(
            "--last takes a duration greater than zero, not {value:?}"
        ));
    }
    if seconds > max_last_seconds() {
        return Err(format!("--last takes at most 100 years, not {value:?}"));
    }
    Duration::try_seconds(seconds)
        .ok_or_else(|| format!("--last takes a duration greater than zero, not {value:?}"))
}

fn finish_last_milliseconds(ms: i64, value: &str) -> Result<Duration, String> {
    let max_ms = max_last_seconds().saturating_mul(1000);
    if ms <= 0 {
        return Err(format!(
            "--last takes a duration greater than zero, not {value:?}"
        ));
    }
    if ms > max_ms {
        return Err(format!("--last takes at most 100 years, not {value:?}"));
    }
    Duration::try_milliseconds(ms)
        .ok_or_else(|| format!("--last takes a duration greater than zero, not {value:?}"))
}

fn duration_has_unit(trimmed: &str) -> bool {
    trimmed
        .chars()
        .any(|ch| ch.is_ascii_alphabetic() || ch == 'µ' || ch == 'μ')
}

fn parse_go_duration(trimmed: &str, flag: &str) -> Result<i64, String> {
    let mut total_ms = 0f64;
    let mut rest = trimmed.trim();
    while !rest.is_empty() {
        let start = rest
            .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
            .unwrap_or(rest.len());
        if start == 0 {
            return Err(format!(
                "{flag} takes a duration greater than zero, not {trimmed:?}"
            ));
        }
        let (number, after) = rest.split_at(start);
        let value: f64 = number
            .parse()
            .map_err(|_| format!("{flag} takes a duration greater than zero, not {trimmed:?}"))?;
        if !(value.is_finite()) || value < 0.0 {
            return Err(format!(
                "{flag} takes a duration greater than zero, not {trimmed:?}"
            ));
        }
        let (unit_ms, next) = if let Some(stripped) = after.strip_prefix("ns") {
            (value / 1_000_000.0, stripped)
        } else if let Some(stripped) = after.strip_prefix("us") {
            (value / 1_000.0, stripped)
        } else if let Some(stripped) = after.strip_prefix("µs") {
            (value / 1_000.0, stripped)
        } else if let Some(stripped) = after.strip_prefix("μs") {
            (value / 1_000.0, stripped)
        } else if let Some(stripped) = after.strip_prefix("ms") {
            (value, stripped)
        } else if let Some(stripped) = after.strip_prefix('h') {
            (value * 3_600_000.0, stripped)
        } else if let Some(stripped) = after.strip_prefix('m') {
            (value * 60_000.0, stripped)
        } else if let Some(stripped) = after.strip_prefix('s') {
            (value * 1_000.0, stripped)
        } else {
            return Err(format!(
                "{flag} takes a duration greater than zero, not {trimmed:?}"
            ));
        };
        total_ms += unit_ms;
        rest = next;
    }
    if !(total_ms.is_finite()) || total_ms <= 0.0 || total_ms >= i64::MAX as f64 {
        return Err(format!(
            "{flag} takes a duration greater than zero, not {trimmed:?}"
        ));
    }
    Ok((total_ms.round() as i64).max(1))
}

/// Parses `--days` as a positive number of days (fractions allowed).
///
/// # Errors
///
/// Returns a message when the value is not a positive number of days.
pub fn parse_days(value: &str) -> Result<Duration, String> {
    let days: f64 = value
        .trim()
        .parse()
        .map_err(|_| format!("--days {value:?} is not a number of days"))?;
    if !(days.is_finite()) || days <= 0.0 || days > 100.0 * 365.0 {
        return Err(format!(
            "--days takes a number of days greater than zero, not {value:?}"
        ));
    }
    let seconds = (days * 86_400.0).round() as i64;
    Ok(Duration::seconds(seconds.max(1)))
}

/// CSV column order for `hofund usage --csv` (matches the reference ledger header).
pub const CSV_HEADER: &[&str] = &[
    "time",
    "agent",
    "requested_model",
    "provider",
    "host",
    "model",
    "served_model",
    "swapped",
    "effort",
    "input_tokens",
    "output_tokens",
    "cache_write_tokens",
    "cache_read_tokens",
    "reasoning_tokens",
    "cost_usd",
    "duration_ms",
    "ttft_ms",
    "status",
    "error",
    "session",
    "kind",
    "provider_key_id",
    "provider_key_name",
    "provider_account",
    "route_id",
    "request_id",
    "endpoint",
    "error_message",
    "error_type",
    "source",
    "rejected",
    "session_provider",
    "session_account",
    "session_official_login",
    "caller_key_id",
    "caller_key_name",
    "response_id",
    "upstream",
    "cache_write_5m_tokens",
    "cache_write_1h_tokens",
    "tries",
    "handed_on",
    "tier_asked",
    "tier_sent",
    "tier_served",
    "upstream_stop",
    "credits",
];

/// Escapes one CSV field (RFC4180-style quoting).
#[must_use]
pub fn csv_field(value: &str) -> String {
    if value.contains(['"', ',', '\n', '\r']) {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            if ch == '"' {
                out.push_str("\"\"");
            } else {
                out.push(ch);
            }
        }
        out.push('"');
        out
    } else {
        value.to_owned()
    }
}

/// Formats rows as CSV, header first, `rows` newest first.
///
/// # Errors
///
/// Returns [`UsageError`] when formatting fails.
pub fn format_csv(rows: &[Row]) -> Result<String, UsageError> {
    let mut out = String::new();
    out.push_str(
        &CSV_HEADER
            .iter()
            .copied()
            .map(csv_field)
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for row in rows {
        write_csv_row(&mut out, row)?;
    }
    Ok(out)
}

fn write_csv_row(out: &mut String, row: &Row) -> Result<(), UsageError> {
    let time = DateTime::<Utc>::from_timestamp(row.created_unix, 0)
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_default();
    let account = account_of_upstream(&row.upstream).unwrap_or("");
    let cost = row
        .cost_usd
        .filter(|_| row.priced)
        .map(|value| format!("{value:.6}"))
        .unwrap_or_default();
    let cache_write_5m = row.cache_write_5m_tokens.unwrap_or(0);
    let cache_write_1h = row.cache_write_1h_tokens.unwrap_or(0);
    let cache_write_total = cache_write_5m + cache_write_1h;
    let status_code = row.status.parse::<u16>().ok();
    let failed = status_code.is_some_and(|code| code >= 400);
    let handed_on = row.outcome == crate::OUTCOME_HANDED_ON;
    let cells = [
        time,
        row.client_agent.clone(),
        row.model.clone(),
        String::new(),
        String::new(),
        String::new(),
        row.served_model.clone(),
        String::new(),
        String::new(),
        opt_i64(row.input_tokens),
        opt_i64(row.output_tokens),
        opt_i64(Some(cache_write_total)),
        opt_i64(row.cache_read_tokens),
        opt_i64(row.reasoning_tokens),
        cost,
        row.latency_ms.to_string(),
        opt_i64(row.ttft_ms),
        row.status.clone(),
        bool_cell(failed),
        row.session_id.clone(),
        String::new(),
        row.provider_key_id.clone(),
        String::new(),
        account.to_owned(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        row.caller_key_id.clone(),
        row.caller_key_name.clone(),
        String::new(),
        row.upstream.clone(),
        opt_i64(row.cache_write_5m_tokens),
        opt_i64(row.cache_write_1h_tokens),
        String::new(),
        bool_cell(handed_on),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ];
    assert_eq!(
        cells.len(),
        CSV_HEADER.len(),
        "csv columns {} vs header {}",
        cells.len(),
        CSV_HEADER.len()
    );
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write!(out, "{}", csv_field(cell)).map_err(|error| UsageError(error.to_string()))?;
    }
    out.push('\n');
    Ok(())
}

fn opt_i64(value: Option<i64>) -> String {
    value.map(|tokens| tokens.to_string()).unwrap_or_default()
}

fn bool_cell(value: bool) -> String {
    if value {
        "true".to_owned()
    } else {
        String::new()
    }
}

impl Ledger {
    /// Rows and totals for a report, optionally filtered, newest first in `rows`.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when a query fails.
    pub fn report(
        &self,
        since: Option<i64>,
        model: Option<&str>,
        account: Option<&str>,
        row_limit: usize,
    ) -> Result<(Vec<Row>, Totals), UsageError> {
        self.summary_filtered(since, model, account, row_limit)
    }
}

/// Speed summaries over ledger rows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SpeedTotals {
    /// Mean TTFT in milliseconds when any row measured it.
    pub mean_ttft_ms: Option<f64>,
    /// Weighted mean output tokens per second.
    pub mean_tokens_per_second: Option<f64>,
}

/// Aggregates speed fields from usage rows.
#[must_use]
pub fn speed_totals(rows: &[Row]) -> SpeedTotals {
    SpeedTotals {
        mean_ttft_ms: mean_ttft_ms(rows.iter()),
        mean_tokens_per_second: mean_tokens_per_second(rows.iter()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CSV_HEADER, ReportWindow, account_of_upstream, csv_field, format_csv, parse_days,
        parse_last, parse_since, row_matches_filter,
    };
    use crate::{Ledger, Row, UsageDetails};
    use chrono::TimeZone;

    #[test]
    fn r10_csv_field_quotes_commas_quotes_and_newlines() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn r10_since_local_date_starts_at_midnight() {
        let since = parse_since("2026-03-01").expect("date");
        let local = chrono::Local
            .timestamp_opt(since, 0)
            .single()
            .expect("local");
        assert_eq!(local.date_naive().to_string(), "2026-03-01");
        assert_eq!(
            local.time(),
            chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("midnight")
        );
    }

    #[test]
    fn r10_period_parsing_days_and_last() {
        assert_eq!(parse_days("1.5").expect("days").num_seconds(), 129_600);
        assert_eq!(parse_last("90m").expect("last").num_minutes(), 90);
        assert_eq!(parse_last("1h30m").expect("last").num_seconds(), 5400);
        assert_eq!(parse_last("500ms").expect("last").num_milliseconds(), 500);
        assert!(parse_days("0").is_err());
        assert!(parse_last("0d").is_err());
        assert!(parse_last("-1d").is_err());
        assert!(parse_last("").is_err());
        assert!(parse_last("90").is_err());
        assert!(parse_last("0").is_err());
        assert!(parse_last("0s").is_err());
        assert!(parse_last("0h0m").is_err());
        assert_eq!(parse_last("1ns").expect("nano").num_milliseconds(), 1);
        assert_eq!(parse_last("1us").expect("micro").num_milliseconds(), 1);
        assert_eq!(parse_last("1h0m").expect("zero part").num_minutes(), 60);
        assert_eq!(parse_last("1h30m").expect("compound").num_minutes(), 90);
        assert_eq!(parse_last("1.5ms").expect("fraction").num_milliseconds(), 2);
        assert_eq!(parse_last("1µs").expect("micro").num_milliseconds(), 1);
        assert_eq!(
            parse_last("1μs").expect("greek micro").num_milliseconds(),
            1
        );
        assert!(
            parse_last("200000000000d")
                .expect_err("days overflow")
                .contains("at most 100 years")
        );
        assert!(
            parse_last("200000000000h")
                .expect_err("hours overflow")
                .contains("at most 100 years")
        );
        assert!(parse_last("36500d").is_ok());
        assert!(parse_last("36501d").is_err());
    }

    #[test]
    fn r10_account_filter_matches_upstream() {
        assert_eq!(
            account_of_upstream("account:relay:me@x.test"),
            Some("me@x.test")
        );
        let row = Row {
            model: "demo".to_owned(),
            upstream: "account:relay:me@x.test".to_owned(),
            ..Default::default()
        };
        assert!(row_matches_filter(&row, None, Some("me@x.test")));
        assert!(!row_matches_filter(&row, None, Some("other")));
        assert!(row_matches_filter(&row, Some("demo"), Some("me@x.test")));
    }

    #[test]
    fn r10_csv_header_order_and_newest_first() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("u.sqlite")).expect("ledger");
        let base = chrono::Utc::now().timestamp();
        for (offset, model) in [(0, "b"), (1, "a")] {
            ledger
                .record_with_details(
                    &Row {
                        created_unix: base + offset,
                        model: model.to_owned(),
                        upstream: "primary".to_owned(),
                        input_tokens: Some(1),
                        output_tokens: Some(2),
                        latency_ms: 1000,
                        status: "200".to_owned(),
                        client_agent: "agent".to_owned(),
                        ..Default::default()
                    },
                    Some(200),
                    UsageDetails::default(),
                    None,
                )
                .expect("row");
        }
        let (rows, _) = ledger.report(None, None, None, 100).expect("report");
        assert_eq!(rows.first().map(|row| row.model.as_str()), Some("a"));
        let csv = format_csv(&rows).expect("csv");
        let header = csv.lines().next().expect("header");
        assert_eq!(
            header,
            CSV_HEADER
                .iter()
                .copied()
                .map(csv_field)
                .collect::<Vec<_>>()
                .join(",")
        );
        let row_line = csv.lines().nth(1).expect("row");
        let fields: Vec<_> = row_line.split(',').collect();
        assert_eq!(fields.len(), CSV_HEADER.len());
        assert_eq!(fields[1], "agent");
        assert_eq!(fields[2], "a");
    }

    #[test]
    fn r10_csv_full_row_values() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("u.sqlite")).expect("ledger");
        let row = Row {
            created_unix: 1_700_000_000,
            model: "asked".to_owned(),
            upstream: "account:relay:me@x.test".to_owned(),
            input_tokens: Some(0),
            output_tokens: Some(7),
            latency_ms: 1200,
            status: "502".to_owned(),
            outcome: crate::OUTCOME_HANDED_ON.to_owned(),
            caller_key_id: "gk".to_owned(),
            caller_key_name: "Guest".to_owned(),
            provider_key_id: "pk".to_owned(),
            client_agent: "fixture".to_owned(),
            session_id: "sess".to_owned(),
            served_model: "served-id".to_owned(),
            ttft_ms: Some(0),
            priced: true,
            cost_usd: Some(0.25),
            ..Default::default()
        };
        ledger
            .record_with_details(
                &row,
                Some(0),
                UsageDetails::default(),
                Some(crate::RowPricing {
                    priced: true,
                    cost_usd: 0.25,
                    price: crate::TokenPrice {
                        input: 1.0,
                        output: 1.0,
                        cache_read: 0.0,
                        cache_write: 0.0,
                        cache_write_1h: 0.0,
                    },
                }),
            )
            .expect("row");
        let (rows, _) = ledger.report(None, None, None, 10).expect("report");
        let csv = format_csv(&rows).expect("csv");
        let header_line = csv.lines().next().expect("header");
        assert_eq!(
            header_line,
            CSV_HEADER
                .iter()
                .copied()
                .map(csv_field)
                .collect::<Vec<_>>()
                .join(",")
        );
        let data: Vec<&str> = csv.lines().nth(1).expect("row").split(',').collect();
        assert_eq!(data.len(), CSV_HEADER.len());
        let expected = [
            "2023-11-14T22:13:20+00:00",
            "fixture",
            "asked",
            "",
            "",
            "",
            "served-id",
            "",
            "",
            "0",
            "7",
            "0",
            "",
            "",
            "0.250000",
            "1200",
            "0",
            "502",
            "true",
            "sess",
            "",
            "pk",
            "",
            "me@x.test",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "gk",
            "Guest",
            "",
            "account:relay:me@x.test",
            "",
            "",
            "",
            "true",
            "",
            "",
            "",
            "",
            "",
        ];
        for (index, (name, value)) in CSV_HEADER.iter().zip(expected.iter()).enumerate() {
            assert_eq!(data[index], *value, "column {name}");
        }
    }

    #[test]
    fn r10_filtered_summary_keeps_newest_matching_rows() {
        let dir = tempfile::tempdir().expect("dir");
        let ledger = Ledger::open(&dir.path().join("u.sqlite")).expect("ledger");
        let base = 1_700_000_000;
        for index in 0..5 {
            ledger
                .record(&Row {
                    created_unix: base + index,
                    model: if index % 2 == 0 { "keep" } else { "drop" }.to_owned(),
                    upstream: "m".to_owned(),
                    latency_ms: 1,
                    status: "200".to_owned(),
                    ..Default::default()
                })
                .expect("row");
        }
        let (rows, _) = ledger
            .summary_filtered(None, Some("keep"), None, 2)
            .expect("filtered");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model, "keep");
        assert_eq!(rows[0].created_unix, base + 4);
        assert_eq!(rows[1].created_unix, base + 2);
    }

    #[test]
    fn r10_report_window_since_unix() {
        let now = chrono::Local::now();
        let rolling = ReportWindow::Rolling(chrono::Duration::hours(2));
        let since = rolling.since_unix(now).expect("since");
        assert!(since <= now.timestamp());
    }
}
