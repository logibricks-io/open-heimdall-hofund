//! Reporting windows, filters, and CSV export for the usage ledger.

use std::fmt::Write as _;

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};

use crate::speed::{mean_tokens_per_second, mean_ttft_ms, tokens_per_second};
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

    /// Maps calendar presets to [`Window`] values.
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

/// Parses `--last` as a positive Go-style duration (`24h`, `90m`, `1.5d`).
///
/// # Errors
///
/// Returns a message when the duration is missing or not positive.
pub fn parse_last(value: &str) -> Result<Duration, String> {
    parse_duration_value(value, "--last")
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

fn parse_duration_value(value: &str, flag: &str) -> Result<Duration, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{flag} takes a duration"));
    }
    if let Some(rest) = trimmed.strip_suffix('d') {
        let days: f64 = rest
            .parse()
            .map_err(|_| format!("{flag} takes a duration greater than zero, not {value:?}"))?;
        if !(days.is_finite()) || days <= 0.0 {
            return Err(format!(
                "{flag} takes a duration greater than zero, not {value:?}"
            ));
        }
        return Ok(Duration::seconds((days * 86_400.0).round() as i64));
    }
    parse_hms_duration(trimmed, flag)
}

fn parse_hms_duration(trimmed: &str, flag: &str) -> Result<Duration, String> {
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        return Err(format!("{flag} takes a duration"));
    }
    let (number, unit) = trimmed
        .chars()
        .rev()
        .position(|ch| !ch.is_ascii_digit() && ch != '.')
        .map(|index| trimmed.split_at(trimmed.len() - index))
        .unwrap_or((trimmed, ""));
    let value: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("{flag} takes a duration greater than zero, not {trimmed:?}"))?;
    if !(value.is_finite()) || value <= 0.0 {
        return Err(format!(
            "{flag} takes a duration greater than zero, not {trimmed:?}"
        ));
    }
    let seconds = match unit {
        "h" | "hr" | "hour" | "hours" => value * 3600.0,
        "m" | "min" | "minute" | "minutes" => value * 60.0,
        "s" | "sec" | "second" | "seconds" | "" => value,
        _ => {
            return Err(format!(
                "{flag} takes a duration greater than zero, not {trimmed:?}"
            ));
        }
    };
    Ok(Duration::seconds(seconds.round() as i64))
}

/// CSV column order for `hofund usage --csv`.
pub const CSV_HEADER: &[&str] = &[
    "time",
    "agent",
    "model_asked",
    "provider",
    "model_served",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_5m_tokens",
    "cache_write_1h_tokens",
    "reasoning_tokens",
    "cost_usd",
    "duration_ms",
    "ttft_ms",
    "tokens_per_second",
    "status",
    "account",
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
pub fn format_csv(rows: &[Row], convert_cost: impl Fn(f64) -> f64) -> Result<String, UsageError> {
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
    for row in rows.iter().rev() {
        write_csv_row(&mut out, row, &convert_cost)?;
    }
    Ok(out)
}

fn write_csv_row(
    out: &mut String,
    row: &Row,
    convert_cost: &impl Fn(f64) -> f64,
) -> Result<(), UsageError> {
    let time = DateTime::<Utc>::from_timestamp(row.created_unix, 0)
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_default();
    let account = account_of_upstream(&row.upstream).unwrap_or("");
    let cost = row
        .cost_usd
        .filter(|_| row.priced)
        .map(convert_cost)
        .map(|value| format!("{value:.6}"))
        .unwrap_or_default();
    let speed = tokens_per_second(row.output_tokens, row.latency_ms, row.ttft_ms)
        .map(|value| format!("{value:.6}"))
        .unwrap_or_default();
    let cells = [
        time,
        row.client_agent.clone(),
        row.model.clone(),
        row.upstream.clone(),
        row.model.clone(),
        opt_i64(row.input_tokens),
        opt_i64(row.output_tokens),
        opt_i64(row.cache_read_tokens),
        opt_i64(row.cache_write_5m_tokens),
        opt_i64(row.cache_write_1h_tokens),
        opt_i64(row.reasoning_tokens),
        cost,
        row.latency_ms.to_string(),
        row.ttft_ms
            .filter(|ms| *ms > 0)
            .map(|ms| ms.to_string())
            .unwrap_or_default(),
        speed,
        row.status.clone(),
        account.to_owned(),
    ];
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
    value
        .filter(|tokens| *tokens > 0)
        .map(|tokens| tokens.to_string())
        .unwrap_or_default()
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
        mean_ttft_ms: mean_ttft_ms(rows.iter().map(|row| row.ttft_ms)),
        mean_tokens_per_second: mean_tokens_per_second(
            rows.iter()
                .map(|row| (row.output_tokens, row.latency_ms, row.ttft_ms)),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ReportWindow, account_of_upstream, csv_field, format_csv, parse_days, parse_last,
        parse_since, row_matches_filter,
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
        assert!(parse_days("0").is_err());
        assert!(parse_last("").is_err());
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
        let csv = format_csv(&rows, |usd| usd).expect("csv");
        let header = csv.lines().next().expect("header");
        assert!(header.starts_with("time,agent,model_asked"));
        assert!(header.contains("tokens_per_second"));
    }

    #[test]
    fn r10_report_window_since_unix() {
        let now = chrono::Local::now();
        let rolling = ReportWindow::Rolling(chrono::Duration::hours(2));
        let since = rolling.since_unix(now).expect("since");
        assert!(since <= now.timestamp());
    }
}
