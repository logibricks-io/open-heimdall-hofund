//! Reset time parsing for Claude Code `/usage` lines (reference `claudeResetTime`).

use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, LocalResult, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use regex::Regex;

/// How long a reset time may read ahead of its window (clock slack).
pub(super) const RESET_SLACK: Duration = Duration::from_secs(2 * 60 * 60);

#[cfg(test)]
static TEST_LOCAL_TZ: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) fn set_test_local_tz(iana: Option<&str>) {
    *TEST_LOCAL_TZ.lock().expect("test tz lock") = iana.map(str::to_owned);
}

fn month_day_time() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^([A-Za-z]{3})\s+(\d{1,2})(?:,\s*(\d{4}))?(?:(?:\s+at\s+)|(?:,\s*))(\d{1,2})(?::(\d{2}))?\s*(am|pm)\s*$",
        )
        .expect("month day time")
    })
}

fn time_only() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(\d{1,2})(?::(\d{2}))?\s*(am|pm)\s*$").expect("time only"))
}

pub(super) fn reset_time(
    reset: Option<&str>,
    now: SystemTime,
    span: Duration,
) -> Option<SystemTime> {
    let reset = reset.map(str::trim).filter(|s| !s.is_empty())?;
    let (zone_name, rest) = split_zone(reset);
    let loc = resolve_loc(zone_name);
    let ref_dt = datetime_from_system(now)?.with_timezone(&loc);
    let slack = chrono::Duration::from_std(span + RESET_SLACK)
        .unwrap_or_else(|_| chrono::Duration::hours(2));
    let latest = ref_dt + slack;
    let rest = rest
        .replace("AM", "am")
        .replace("PM", "pm")
        .replace("Am", "am")
        .replace("Pm", "pm");
    if let Some(caps) = month_day_time().captures(&rest) {
        let month = parse_month(caps.get(1)?.as_str())?;
        let day: u32 = caps.get(2)?.as_str().parse().ok()?;
        let year = caps.get(3).and_then(|m| m.as_str().parse::<i32>().ok());
        let hour = hour12(caps.get(4)?.as_str().parse().ok()?, caps.get(6)?.as_str())?;
        let minute: u32 = caps
            .get(5)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        if let Some(year) = year {
            return explicit_year(loc, year, month, day, hour, minute);
        }
        return pick_yearless(month, day, hour, minute, ref_dt, latest, &loc);
    }
    if let Some(caps) = time_only().captures(&rest) {
        let hour = hour12(caps.get(1)?.as_str().parse().ok()?, caps.get(3)?.as_str())?;
        let minute: u32 = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        return pick_time_only(hour, minute, ref_dt, latest, &loc);
    }
    None
}

fn resolve_loc(zone_name: Option<&str>) -> Tz {
    if let Some(name) = zone_name
        && let Ok(tz) = Tz::from_str(name)
    {
        return tz;
    }
    host_local_tz()
}

fn host_local_tz() -> Tz {
    #[cfg(test)]
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

fn explicit_year(
    loc: Tz,
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
        LocalResult::None => gap_fallback(&loc, year, month, day, hour, minute)?,
    };
    system_from_datetime(dt)
}

#[allow(clippy::too_many_arguments)]
fn next_reading_wall(
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

fn gap_fallback(
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

fn wall_clock_readings(
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

fn pick_yearless(
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
            return system_from_datetime(chosen);
        }
        for candidate in wall_clock_readings(loc, year, month, day, hour, minute) {
            if candidate < ref_dt {
                past = Some(candidate);
            }
        }
    }
    past.and_then(system_from_datetime)
}

fn pick_time_only(
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
            return system_from_datetime(chosen);
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
                return system_from_datetime(candidate);
            }
        }
    }
    None
}

fn hour12(hour: u32, ampm: &str) -> Option<u32> {
    if !(1..=12).contains(&hour) {
        return None;
    }
    let pm = ampm.eq_ignore_ascii_case("pm");
    Some(if hour == 12 {
        if pm { 12 } else { 0 }
    } else if pm {
        hour + 12
    } else {
        hour
    })
}

fn parse_month(name: &str) -> Option<u32> {
    match name.to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

pub(super) fn split_zone(s: &str) -> (Option<&str>, String) {
    let Some(open) = s.rfind('(') else {
        return (None, s.to_owned());
    };
    if !s.ends_with(')') {
        return (None, s.to_owned());
    }
    (
        Some(s[open + 1..s.len() - 1].trim()),
        s[..open].trim().to_owned(),
    )
}

fn datetime_from_system(now: SystemTime) -> Option<chrono::DateTime<chrono::Utc>> {
    let dur = now.duration_since(UNIX_EPOCH).ok()?;
    match chrono::Utc.timestamp_opt(dur.as_secs() as i64, dur.subsec_nanos()) {
        LocalResult::Single(dt) => Some(dt),
        _ => None,
    }
}

pub(super) fn system_from_datetime<Tz: TimeZone>(dt: chrono::DateTime<Tz>) -> Option<SystemTime> {
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::{America::Los_Angeles, Asia::Shanghai};

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> SystemTime {
        let dt = chrono::Utc
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("time");
        UNIX_EPOCH + Duration::from_secs(dt.timestamp() as u64)
    }

    fn tz_time(tz: Tz, y: i32, m: u32, d: u32, h: u32, min: u32) -> SystemTime {
        let dt = tz
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("time");
        UNIX_EPOCH + Duration::from_secs(dt.timestamp() as u64)
    }

    #[test]
    fn r5_12_reset_golden_matches_reference_parse_test() {
        let now = utc(2026, 10, 1, 4, 0);
        let text = "Oct 1 at 3:30pm (Asia/Shanghai)";
        assert_eq!(
            reset_time(Some(text), now, Duration::from_secs(5 * 3600)),
            Some(tz_time(Shanghai, 2026, 10, 1, 15, 30))
        );
        assert_eq!(
            reset_time(
                Some("Oct 3 at 2pm (Asia/Shanghai)"),
                now,
                Duration::from_secs(7 * 86400)
            ),
            Some(tz_time(Shanghai, 2026, 10, 3, 14, 0))
        );
        assert_eq!(
            reset_time(
                Some("Jan 2 at 2pm (Asia/Shanghai)"),
                now,
                Duration::from_secs(7 * 86400)
            ),
            Some(tz_time(Shanghai, 2026, 1, 2, 14, 0))
        );
        assert_eq!(
            reset_time(Some("3am (UTC)"), now, Duration::from_secs(7 * 86400)),
            Some(utc(2026, 10, 2, 3, 0))
        );
    }

    #[test]
    fn r5_13_time_only_reset_at_three_pm_utc() {
        let now = utc(2026, 11, 1, 13, 0);
        assert_eq!(
            reset_time(Some("3pm (UTC)"), now, Duration::from_secs(5 * 3600)),
            Some(utc(2026, 11, 1, 15, 0))
        );
    }

    #[test]
    fn r5_14_unknown_zone_uses_host_local_not_utc() {
        set_test_local_tz(Some("America/Los_Angeles"));
        let _guard = RestoreTz;
        let now = tz_time(Los_Angeles, 2026, 9, 28, 10, 0);
        assert_eq!(
            reset_time(
                Some("Oct 1, 2026 at 3:30pm (Not/AZone)"),
                now,
                Duration::from_secs(7 * 86400)
            ),
            Some(tz_time(Los_Angeles, 2026, 10, 1, 15, 30))
        );
    }

    #[test]
    fn r5_34_time_only_without_zone_uses_host_local() {
        set_test_local_tz(Some("America/Los_Angeles"));
        let _guard = RestoreTz;
        let now = tz_time(Los_Angeles, 2026, 6, 15, 10, 0);
        assert_eq!(
            reset_time(Some("3pm"), now, Duration::from_secs(5 * 3600)),
            Some(tz_time(Los_Angeles, 2026, 6, 15, 15, 0))
        );
    }

    struct RestoreTz;
    impl Drop for RestoreTz {
        fn drop(&mut self) {
            set_test_local_tz(None);
        }
    }

    #[test]
    fn r5_15_dec_to_jan_year_rollover() {
        let now = utc(2026, 12, 31, 23, 0);
        assert_eq!(
            reset_time(
                Some("Jan 2 at 2pm (UTC)"),
                now,
                Duration::from_secs(7 * 86400)
            ),
            Some(utc(2027, 1, 2, 14, 0))
        );
    }

    #[test]
    fn r5_35_dst_fall_back_nov_1_new_york() {
        let week = Duration::from_secs(7 * 86400);
        let now = utc(2026, 11, 1, 6, 20);
        assert_eq!(
            reset_time(Some("Nov 1, 1:30am (America/New_York)"), now, week),
            Some(utc(2026, 11, 1, 6, 30))
        );
        assert_eq!(
            reset_time(Some("1:30am (America/New_York)"), now, week),
            Some(utc(2026, 11, 1, 6, 30))
        );
    }
}
