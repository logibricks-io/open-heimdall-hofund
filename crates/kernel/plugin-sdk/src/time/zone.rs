//! IANA time zones and local wall-clock instants for subscription reset lines.
//!
//! Plugins parse vendor text; this module resolves `(America/Los_Angeles)` and
//! similar names, falls back to the host local zone (UTC when unknown), and
//! maps ambiguous DST readings the way Claude Code's usage output expects.

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono_tz::Tz;

pub use chrono;
pub use chrono::{Datelike, LocalResult, NaiveDate, TimeZone, Timelike};
pub use chrono_tz;

static TEST_LOCAL_TZ: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Overrides the host local zone in unit tests. Cleared with `None`.
///
/// # Panics
///
/// Panics if the test override mutex is poisoned.
pub fn set_test_host_local(iana: Option<&str>) {
    *TEST_LOCAL_TZ.lock().expect("test tz lock") = iana.map(str::to_owned);
}

/// Parses an IANA identifier, or [`host_local`](host_local) when `name` is missing or invalid.
#[must_use]
pub fn resolve_iana(name: Option<&str>) -> Tz {
    if let Some(name) = name
        && let Ok(tz) = Tz::from_str(name)
    {
        return tz;
    }
    host_local()
}

/// The host's current IANA zone, or UTC when it cannot be read.
///
/// # Panics
///
/// Panics if the test override mutex is poisoned.
#[must_use]
pub fn host_local() -> Tz {
    if let Some(name) = TEST_LOCAL_TZ.lock().expect("test tz lock").as_deref()
        && let Ok(tz) = Tz::from_str(name)
    {
        return tz;
    }
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| Tz::from_str(&name).ok())
        .unwrap_or(chrono_tz::UTC)
}

/// UTC [`chrono::DateTime`] for `now`.
#[must_use]
pub fn utc_datetime(now: SystemTime) -> Option<chrono::DateTime<chrono::Utc>> {
    let dur = now.duration_since(UNIX_EPOCH).ok()?;
    match chrono::Utc.timestamp_opt(dur.as_secs().cast_signed(), dur.subsec_nanos()) {
        LocalResult::Single(dt) => Some(dt),
        _ => None,
    }
}

/// Maps a zone-aware instant to [`SystemTime`]. Times before 1970 return `None`.
#[must_use]
pub fn system_from_datetime<Tz: TimeZone>(dt: &chrono::DateTime<Tz>) -> Option<SystemTime> {
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs.cast_unsigned()))
}

/// Converts a [`Duration`] to chrono's span, clamping overflow to two hours.
#[must_use]
pub fn chrono_span(span: Duration) -> chrono::Duration {
    chrono::Duration::from_std(span).unwrap_or_else(|_| chrono::Duration::hours(2))
}

/// One local wall-clock reading on `year-month-day` at `hour:minute` in `loc`.
#[must_use]
pub fn local_wall_instant(
    loc: &Tz,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<SystemTime> {
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    let dt = match loc.from_local_datetime(&naive) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(_, later) => later,
        LocalResult::None => gap_fallback(loc, year, month, day, hour, minute)?,
    };
    system_from_datetime(&dt)
}

/// Every instant that matches one local wall time (zero, one, or two around DST).
#[must_use]
pub fn wall_clock_readings(
    loc: &Tz,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Vec<chrono::DateTime<Tz>> {
    let Some(naive) =
        NaiveDate::from_ymd_opt(year, month, day).and_then(|d| d.and_hms_opt(hour, minute, 0))
    else {
        return Vec::new();
    };
    match loc.from_local_datetime(&naive) {
        LocalResult::Single(t) => vec![t],
        LocalResult::Ambiguous(earlier, later) => vec![earlier, later],
        LocalResult::None => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
/// Chooses the earliest wall-clock reading on a calendar day that is still on or after `ref_dt` or `lim`.
#[must_use]
pub fn next_reading_wall(
    loc: &Tz,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    ref_dt: chrono::DateTime<Tz>,
    lim: chrono::DateTime<Tz>,
) -> Option<chrono::DateTime<Tz>> {
    let readings = wall_clock_readings(loc, year, month, day, hour, minute);
    if readings.is_empty() {
        return None;
    }
    let first = readings[0];
    let last = *readings.last().unwrap_or(&first);
    for from in [ref_dt, lim] {
        for r in [first, last] {
            if r >= from {
                return Some(r);
            }
        }
    }
    None
}

/// Maps a local time in a DST gap by shifting forward up to three hours.
#[must_use]
pub fn gap_fallback(
    loc: &Tz,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<chrono::DateTime<Tz>> {
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    for offset in 0..=3i64 {
        let shifted = naive + chrono::Duration::hours(offset);
        match loc.from_local_datetime(&shifted) {
            LocalResult::Single(dt) => return Some(dt),
            LocalResult::Ambiguous(earlier, _) => return Some(earlier),
            LocalResult::None => {}
        }
    }
    None
}

/// Picks a yearless month/day reset instant within one year of `ref_dt`.
#[must_use]
pub fn pick_yearless(
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    ref_dt: chrono::DateTime<Tz>,
    latest: chrono::DateTime<Tz>,
    loc: &Tz,
) -> Option<SystemTime> {
    let lim = ref_dt - chrono::Duration::days(1);
    let mut past = None;
    for year in (ref_dt.year() - 1)..=(ref_dt.year() + 1) {
        if let Some(chosen) = next_reading_wall(loc, year, month, day, hour, minute, ref_dt, lim)
            && chosen <= latest
        {
            return system_from_datetime(&chosen);
        }
        for candidate in wall_clock_readings(loc, year, month, day, hour, minute) {
            if candidate < ref_dt {
                past = Some(candidate);
            }
        }
    }
    past.as_ref().and_then(system_from_datetime)
}

/// Picks a time-only reset instant within a few days of `ref_dt`.
#[must_use]
pub fn pick_time_only(
    hour: u32,
    minute: u32,
    ref_dt: chrono::DateTime<Tz>,
    latest: chrono::DateTime<Tz>,
    loc: &Tz,
) -> Option<SystemTime> {
    for day_offset in 0..=2i64 {
        let day = ref_dt.date_naive() + chrono::Duration::days(day_offset);
        if let Some(chosen) = next_reading_wall(
            loc,
            day.year(),
            day.month(),
            day.day(),
            hour,
            minute,
            ref_dt,
            ref_dt,
        ) && chosen <= latest
        {
            return system_from_datetime(&chosen);
        }
    }
    for day_offset in 0..=2i64 {
        let day = ref_dt.date_naive() - chrono::Duration::days(day_offset);
        for candidate in wall_clock_readings(loc, day.year(), day.month(), day.day(), hour, minute)
        {
            if candidate.time().hour() == hour
                && candidate.time().minute() == minute
                && candidate < ref_dt
            {
                return system_from_datetime(&candidate);
            }
        }
    }
    None
}
