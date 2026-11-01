use chrono::{DateTime, Datelike, Days, Local, NaiveDate, NaiveTime, TimeZone};
use rusqlite::params;

use super::{Ledger, UsageError};

/// Usage totals for one authenticated gateway key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallerTotals {
    /// Number of recorded requests.
    pub calls: u64,
    /// Number of requests whose status is not 2xx.
    pub errors: u64,
    /// Sum of known non-negative input token counts.
    pub input: u64,
    /// Sum of known non-negative output token counts.
    pub output: u64,
}

/// One model's usage totals for an authenticated gateway key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerModel {
    /// Client-facing model id.
    pub id: String,
    /// This model's totals.
    pub totals: CallerTotals,
}

impl Ledger {
    /// Summarizes only `key_id`'s rows at or after `since`, ranked by call count.
    ///
    /// # Errors
    ///
    /// Returns [`UsageError`] when a query fails.
    pub fn caller_summary(
        &self,
        key_id: &str,
        since: Option<i64>,
    ) -> Result<(CallerTotals, Vec<CallerModel>), UsageError> {
        let since = since.unwrap_or(i64::MIN);
        let connection = self
            .connection
            .lock()
            .map_err(|_| UsageError("usage database lock is poisoned".to_owned()))?;
        let totals = connection
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN status NOT LIKE '2%' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(MAX(input_tokens, 0)), 0),
                        COALESCE(SUM(MAX(output_tokens, 0)), 0)
                 FROM requests WHERE created_unix >= ?1 AND caller_key_id = ?2",
                params![since, key_id],
                read_totals,
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let mut statement = connection
            .prepare(
                "SELECT model, COUNT(*),
                        COALESCE(SUM(CASE WHEN status NOT LIKE '2%' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(MAX(input_tokens, 0)), 0),
                        COALESCE(SUM(MAX(output_tokens, 0)), 0)
                 FROM requests WHERE created_unix >= ?1 AND caller_key_id = ?2
                 GROUP BY model
                 ORDER BY COUNT(*) DESC,
                          SUM(MAX(input_tokens, 0) + MAX(output_tokens, 0)) DESC,
                          model ASC",
            )
            .map_err(|error| UsageError(error.to_string()))?;
        let rows = statement
            .query_map(params![since, key_id], |row| {
                Ok(CallerModel {
                    id: row.get(0)?,
                    totals: read_totals_at(row, 1)?,
                })
            })
            .map_err(|error| UsageError(error.to_string()))?;
        let models = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| UsageError(error.to_string()))?;
        Ok((totals, models))
    }
}

fn read_totals(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallerTotals> {
    read_totals_at(row, 0)
}

fn read_totals_at(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<CallerTotals> {
    let count = |index| -> rusqlite::Result<u64> {
        let value: i64 = row.get(index)?;
        Ok(u64::try_from(value).unwrap_or(0))
    };
    Ok(CallerTotals {
        calls: count(offset)?,
        errors: count(offset + 1)?,
        input: count(offset + 2)?,
        output: count(offset + 3)?,
    })
}

/// A stored key-limit window in both query and JSON forms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyLimitWindow {
    /// Window start as Unix seconds.
    pub start: i64,
    /// Window reset as Unix seconds.
    pub reset: i64,
    /// Window start as RFC 3339 local time.
    pub start_rfc3339: String,
    /// Window reset as RFC 3339 local time.
    pub reset_rfc3339: String,
}

impl KeyLimitWindow {
    fn new(start: DateTime<Local>, reset: DateTime<Local>) -> Self {
        Self {
            start: start.timestamp(),
            reset: reset.timestamp(),
            start_rfc3339: start.to_rfc3339(),
            reset_rfc3339: reset.to_rfc3339(),
        }
    }
}

/// The current start and reset for a stored key-limit window.
///
/// Day, week, and month windows follow the local calendar. Repeating day and
/// hour windows count fixed-duration cycles from `since`.
#[must_use]
pub fn key_limit_window(period: &str, days: u32, hours: u32, since: i64) -> Option<KeyLimitWindow> {
    let now = Local::now();
    match period {
        "day" => calendar_window(
            now.date_naive(),
            now.date_naive().checked_add_days(Days::new(1))?,
        ),
        "week" => {
            let start = now
                .date_naive()
                .checked_sub_days(Days::new(u64::from(now.weekday().num_days_from_monday())))?;
            calendar_window(start, start.checked_add_days(Days::new(7))?)
        }
        "month" => {
            let start = NaiveDate::from_ymd_opt(now.year(), now.month(), 1)?;
            let (year, month) = if now.month() == 12 {
                (now.year() + 1, 1)
            } else {
                (now.year(), now.month() + 1)
            };
            calendar_window(start, NaiveDate::from_ymd_opt(year, month, 1)?)
        }
        "days" => cycle_window(now, since, i64::from(days).checked_mul(86_400)?),
        "hours" => cycle_window(now, since, i64::from(hours).checked_mul(3_600)?),
        _ => None,
    }
}

fn calendar_window(start: NaiveDate, reset: NaiveDate) -> Option<KeyLimitWindow> {
    Some(KeyLimitWindow::new(
        local_day_start(start)?,
        local_day_start(reset)?,
    ))
}

fn local_day_start(date: NaiveDate) -> Option<DateTime<Local>> {
    let midnight = date.and_time(NaiveTime::MIN);
    Local.from_local_datetime(&midnight).earliest().or_else(|| {
        (1..=24 * 60).find_map(|minutes| {
            Local
                .from_local_datetime(&(midnight + chrono::Duration::minutes(minutes)))
                .earliest()
        })
    })
}

fn cycle_window(now: DateTime<Local>, since: i64, span: i64) -> Option<KeyLimitWindow> {
    if span <= 0 {
        return None;
    }
    let since = if since > 0 { since } else { now.timestamp() };
    let cycles = now.timestamp().saturating_sub(since).max(0) / span;
    let start = since.checked_add(cycles.checked_mul(span)?)?;
    Some(KeyLimitWindow::new(
        Local.timestamp_opt(start, 0).single()?,
        Local.timestamp_opt(start.checked_add(span)?, 0).single()?,
    ))
}
