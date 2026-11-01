//! Local-time scheduling for daily warm-up, ported from the reference daily logic.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};

/// How late after a configured start the warm-up still runs when the machine wakes.
pub const DAILY_SLACK: Duration = Duration::from_secs(60 * 60);

/// Slack before a window's end counts as unused, matching the reference warm-up.
pub const WINDOW_SLACK: Duration = Duration::from_secs(10 * 60);

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Parses `06:00` into hour and minute.
pub fn clock(at: &str) -> Option<(u32, u32)> {
    let (hour, minute) = at.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

/// The fixed offset for `now` in the host's local zone.
pub fn local_offset(now: SystemTime) -> FixedOffset {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let local = chrono::Local
        .timestamp_opt(secs as i64, 0)
        .single()
        .unwrap_or_else(|| chrono::Local.timestamp_opt(0, 0).single().expect("epoch"));
    *local.offset()
}

/// The latest configured start at or before `now` in `zone`.
pub fn day_start(ats: &[String], now: SystemTime, zone: FixedOffset) -> Option<SystemTime> {
    let local = zoned(now, zone);
    let mut latest: Option<SystemTime> = None;
    for at in ats {
        let (hour, minute) = clock(at)?;
        let date = local.date_naive();
        let mut candidate = local_datetime(date, hour, minute)?;
        if candidate > local.naive_local() {
            candidate = local_datetime(date.pred_opt()?, hour, minute)?;
        }
        let as_system = naive_to_system(candidate, zone)?;
        if latest.is_none_or(|t| as_system > t) {
            latest = Some(as_system);
        }
    }
    latest
}

/// The first configured start strictly after `now` in `zone`.
pub fn next_day_start(ats: &[String], now: SystemTime, zone: FixedOffset) -> Option<SystemTime> {
    let mut first: Option<SystemTime> = None;
    for at in ats {
        let start = day_start(std::slice::from_ref(at), now, zone)?;
        let start_local = zoned(start, zone);
        let next_date = start_local.date_naive().succ_opt()?;
        let (hour, minute) = clock(at)?;
        let next = naive_to_system(local_datetime(next_date, hour, minute)?, zone)?;
        if first.is_none_or(|t| next < t) {
            first = Some(next);
        }
    }
    first
}

/// Names a day's start for persistence: `2006-01-02 15:04` in `zone`.
pub fn start_key(t: SystemTime, zone: FixedOffset) -> String {
    zoned(t, zone).format("%Y-%m-%d %H:%M").to_string()
}

/// Whether `last` already records start `key`.
pub fn had_start(last: &str, key: &str) -> bool {
    if last == key {
        return true;
    }
    if last.len() == 10 && key.starts_with(last) && key.as_bytes().get(10) == Some(&b' ') {
        return true;
    }
    false
}

/// Whether a short window wants starting for the latest day start.
///
/// Returns the start key to persist when due.
pub fn daily_due(
    ats: &[String],
    last: &str,
    window_running: bool,
    now: SystemTime,
    zone: FixedOffset,
) -> Option<String> {
    let start = day_start(ats, now, zone)?;
    let elapsed = now.duration_since(start).ok()?;
    if elapsed > DAILY_SLACK {
        return None;
    }
    let key = start_key(start, zone);
    if had_start(last, &key) || window_running {
        return None;
    }
    Some(key)
}

/// Whether a reset warm-up should wait for the next day start instead of running now.
#[allow(dead_code)] // reserved for week/all reset warm-up (A-3b and reference reset loop)
pub fn held_for_day(ats: &[String], span: Duration, now: SystemTime, zone: FixedOffset) -> bool {
    let Some(next) = next_day_start(ats, now, zone) else {
        return false;
    };
    next.duration_since(now)
        .ok()
        .is_some_and(|left| left < span)
}

/// Whether a configured start passed since `last_tick`.
#[allow(dead_code)] // reference sleep-after-day-start guard; daily tick uses `daily_due` only
pub fn day_start_passed(
    ats: &[String],
    last_tick: SystemTime,
    now: SystemTime,
    zone: FixedOffset,
) -> bool {
    let Some(start) = day_start(ats, now, zone) else {
        return false;
    };
    start > last_tick
}

/// Whether a short allowance window has not started yet.
pub fn short_window_idle(
    used: f64,
    resets_at: Option<SystemTime>,
    span: Option<Duration>,
    now: SystemTime,
) -> bool {
    if used != 0.0 {
        return false;
    }
    let span = span.unwrap_or_else(|| Duration::from_secs(5 * 3600));
    if span >= DAY {
        return false;
    }
    match resets_at {
        None => true,
        Some(reset) => reset
            .duration_since(now)
            .ok()
            .is_some_and(|left| left >= span.saturating_sub(WINDOW_SLACK)),
    }
}

fn zoned(time: SystemTime, zone: FixedOffset) -> DateTime<FixedOffset> {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    zone.timestamp_opt(secs as i64, 0)
        .single()
        .unwrap_or_else(|| zone.timestamp_opt(0, 0).single().expect("epoch"))
}

fn local_datetime(date: NaiveDate, hour: u32, minute: u32) -> Option<NaiveDateTime> {
    NaiveTime::from_hms_opt(hour, minute, 0).map(|time| date.and_time(time))
}

fn naive_to_system(value: NaiveDateTime, zone: FixedOffset) -> Option<SystemTime> {
    zone.from_local_datetime(&value)
        .single()
        .map(|dt| UNIX_EPOCH + Duration::from_secs(dt.timestamp().unsigned_abs()))
}

#[cfg(test)]
mod tests {
    use super::{DAILY_SLACK, daily_due, day_start_passed, had_start, held_for_day, start_key};
    use chrono::{FixedOffset, TimeZone};
    use std::time::{Duration, UNIX_EPOCH};

    fn east8() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).expect("offset")
    }

    fn east8_at(day: u32, hour: u32, minute: u32) -> std::time::SystemTime {
        let base = chrono::NaiveDate::from_ymd_opt(2026, 9, day).expect("date");
        let time = chrono::NaiveTime::from_hms_opt(hour, minute, 0).expect("time");
        let dt = base.and_time(time);
        let zoned = east8().from_local_datetime(&dt).single().expect("zoned");
        UNIX_EPOCH + Duration::from_secs(zoned.timestamp().unsigned_abs())
    }

    fn ats(one: &str) -> Vec<String> {
        vec![one.to_owned()]
    }

    #[test]
    fn a3a_1_daily_due_on_the_minute() {
        let zone = east8();
        let now = east8_at(28, 6, 0);
        let key = daily_due(&ats("06:00"), "", false, now, zone).expect("due");
        assert_eq!(key, start_key(now, zone));
    }

    #[test]
    fn a3a_2_daily_due_within_slack_after_wake() {
        let zone = east8();
        let now = east8_at(28, 6, 30);
        assert!(daily_due(&ats("06:00"), "", false, now, zone).is_some());
        let late = east8_at(28, 7, 1);
        assert!(daily_due(&ats("06:00"), "", false, late, zone).is_none());
        assert!(late.duration_since(east8_at(28, 6, 0)).expect("order") > DAILY_SLACK);
    }

    #[test]
    fn a3a_4_skips_when_already_started_today() {
        let zone = east8();
        let now = east8_at(28, 6, 5);
        let key = start_key(east8_at(28, 6, 0), zone);
        assert!(daily_due(&ats("06:00"), &key, false, now, zone).is_none());
        assert!(had_start(&key, &key));
    }

    #[test]
    fn a3a_3_daily_skips_when_window_running() {
        let zone = east8();
        let now = east8_at(28, 6, 0);
        assert!(daily_due(&ats("06:00"), "", true, now, zone).is_none());
    }

    #[test]
    fn a3a_5_had_start_accepts_legacy_day_only_key() {
        let zone = east8();
        let key = start_key(east8_at(28, 6, 0), zone);
        assert!(had_start("2026-09-28", &key));
    }

    #[test]
    fn a3a_13_held_for_day_waits_before_long_span_reset() {
        let zone = east8();
        assert!(held_for_day(
            &ats("06:00"),
            Duration::from_secs(5 * 3600),
            east8_at(28, 2, 0),
            zone
        ));
    }

    #[test]
    fn a3a_14_day_start_passed_after_sleep() {
        let zone = east8();
        let last = east8_at(27, 23, 0);
        let now = east8_at(28, 7, 30);
        assert!(day_start_passed(&ats("06:00"), last, now, zone));
    }
}
