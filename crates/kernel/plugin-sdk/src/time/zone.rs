//! IANA time zones and local wall-clock instants for subscription reset lines.

use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, LocalResult, NaiveDate, Offset, TimeZone, Timelike};
use chrono_tz::Tz;

/// Opaque IANA zone handle for subscription reset parsing.
#[derive(Clone, Copy, Debug)]
pub struct Zone(Tz);

impl Zone {
    /// Parses an IANA name, or [`host_local`](Self::host_local) when `name` is missing or invalid.
    #[must_use]
    pub fn resolve(name: Option<&str>) -> Self {
        resolve_iana(name)
    }

    /// The host's current IANA zone, or UTC when it cannot be read.
    #[must_use]
    pub fn host_local() -> Self {
        host_local()
    }
}

/// Parses an IANA identifier, or [`host_local`] when `name` is missing or invalid.
#[must_use]
pub fn resolve_iana(name: Option<&str>) -> Zone {
    if let Some(name) = name
        && let Ok(tz) = Tz::from_str(name)
    {
        return Zone(tz);
    }
    host_local()
}

/// The host's current IANA zone, or UTC when it cannot be read.
#[must_use]
pub fn host_local() -> Zone {
    #[cfg(feature = "test-host-local")]
    if let Some(name) = test_host_local::read()
        && let Ok(tz) = Tz::from_str(&name)
    {
        return Zone(tz);
    }
    let tz = iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| Tz::from_str(&name).ok())
        .unwrap_or(chrono_tz::UTC);
    Zone(tz)
}

/// One local wall-clock reading on a calendar date with an explicit year.
#[must_use]
pub fn local_wall_instant(
    zone: &Zone,
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<SystemTime> {
    let loc = zone.0;
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    let dt = match loc.from_local_datetime(&naive) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(earlier, later) => parse_in_location_ambiguous(loc, earlier, later),
        LocalResult::None => gap_fallback(loc, year, month, day, hour, minute)?,
    };
    system_from_datetime(&dt)
}

/// Picks a yearless month/day reset instant within one year of `now`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn pick_yearless_wall(
    zone: &Zone,
    now: SystemTime,
    window: Duration,
    slack: Duration,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<SystemTime> {
    let loc = &zone.0;
    let ref_dt = utc_datetime(now)?.with_timezone(loc);
    let latest = ref_dt + chrono_span(window + slack);
    pick_yearless(month, day, hour, minute, ref_dt, latest, zone.0)
}

/// Picks a time-only reset instant within a few days of `now`.
#[must_use]
pub fn pick_time_only_wall(
    zone: &Zone,
    now: SystemTime,
    window: Duration,
    slack: Duration,
    hour: u32,
    minute: u32,
) -> Option<SystemTime> {
    let loc = &zone.0;
    let ref_dt = utc_datetime(now)?.with_timezone(loc);
    let latest = ref_dt + chrono_span(window + slack);
    pick_time_only(hour, minute, ref_dt, latest, zone.0)
}

fn utc_datetime(now: SystemTime) -> Option<chrono::DateTime<chrono::Utc>> {
    let dur = now.duration_since(UNIX_EPOCH).ok()?;
    match chrono::Utc.timestamp_opt(dur.as_secs().cast_signed(), dur.subsec_nanos()) {
        LocalResult::Single(dt) => Some(dt),
        _ => None,
    }
}

fn system_from_datetime<Tz: TimeZone>(dt: &chrono::DateTime<Tz>) -> Option<SystemTime> {
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs.cast_unsigned()))
}

fn chrono_span(span: Duration) -> chrono::Duration {
    chrono::Duration::from_std(span).unwrap_or_else(|_| chrono::Duration::hours(2))
}

/// West of UTC keeps the first ambiguous reading; east of UTC keeps the second.
fn parse_in_location_ambiguous(
    _loc: Tz,
    earlier: chrono::DateTime<Tz>,
    later: chrono::DateTime<Tz>,
) -> chrono::DateTime<Tz> {
    if earlier.offset().fix().local_minus_utc() <= 0 {
        earlier
    } else {
        later
    }
}

fn wall_clock_readings(
    loc: Tz,
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
fn next_reading_wall(
    loc: Tz,
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
    loc: Tz,
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

fn pick_yearless(
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    ref_dt: chrono::DateTime<Tz>,
    latest: chrono::DateTime<Tz>,
    loc: Tz,
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

fn pick_time_only(
    hour: u32,
    minute: u32,
    ref_dt: chrono::DateTime<Tz>,
    latest: chrono::DateTime<Tz>,
    loc: Tz,
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

#[cfg(feature = "test-host-local")]
mod test_host_local {
    use std::cell::RefCell;

    thread_local! {
        static OVERRIDE: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(super) fn read() -> Option<String> {
        OVERRIDE.with(|slot| slot.borrow().clone())
    }

    pub(super) fn set(value: Option<String>) {
        OVERRIDE.with(|slot| *slot.borrow_mut() = value);
    }
}

/// Thread-local host zone override for unit tests in downstream crates.
#[cfg(feature = "test-host-local")]
#[doc(hidden)]
pub struct HostLocalGuard {
    previous: Option<String>,
}

/// Sets the host local zone for the current thread until the guard is dropped.
#[cfg(feature = "test-host-local")]
#[doc(hidden)]
#[must_use]
pub fn override_host_local(iana: &str) -> HostLocalGuard {
    let previous = test_host_local::read();
    test_host_local::set(Some(iana.to_owned()));
    HostLocalGuard { previous }
}

#[cfg(feature = "test-host-local")]
impl Drop for HostLocalGuard {
    fn drop(&mut self) {
        test_host_local::set(self.previous.take());
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::{Duration, UNIX_EPOCH};

    use chrono::{LocalResult, NaiveDate, TimeZone};
    use chrono_tz::Tz;

    use super::{local_wall_instant, pick_time_only_wall, pick_yearless_wall, resolve_iana};

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> std::time::SystemTime {
        let dt = chrono::Utc
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("time");
        UNIX_EPOCH + Duration::from_secs(dt.timestamp().cast_unsigned())
    }

    fn in_zone(name: &str, y: i32, m: u32, d: u32, h: u32, min: u32) -> std::time::SystemTime {
        let tz = Tz::from_str(name).expect("zone");
        let dt = tz
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("time");
        UNIX_EPOCH + Duration::from_secs(dt.timestamp().cast_unsigned())
    }

    fn week() -> Duration {
        Duration::from_hours(168)
    }

    #[test]
    fn explicit_year_ambiguous_new_york_picks_first_reading_west_of_utc() {
        let zone = resolve_iana(Some("America/New_York"));
        let got = local_wall_instant(&zone, 2026, 11, 1, 1, 30).expect("instant");
        let loc = chrono_tz::America::New_York;
        let naive = NaiveDate::from_ymd_opt(2026, 11, 1)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let LocalResult::Ambiguous(earlier, _) = loc.from_local_datetime(&naive) else {
            panic!("expected ambiguous fall-back reading");
        };
        let want = UNIX_EPOCH + Duration::from_secs(earlier.timestamp().cast_unsigned());
        assert_eq!(got, want);
    }

    #[test]
    fn explicit_year_ambiguous_berlin_picks_second_reading_east_of_utc() {
        let zone = resolve_iana(Some("Europe/Berlin"));
        let got = local_wall_instant(&zone, 2026, 10, 25, 2, 30).expect("instant");
        let loc = chrono_tz::Europe::Berlin;
        let naive = NaiveDate::from_ymd_opt(2026, 10, 25)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        let LocalResult::Ambiguous(_, later) = loc.from_local_datetime(&naive) else {
            panic!("expected ambiguous fall-back reading");
        };
        let want = UNIX_EPOCH + Duration::from_secs(later.timestamp().cast_unsigned());
        assert_eq!(got, want);
    }

    fn slack() -> Duration {
        Duration::from_hours(2)
    }

    #[test]
    fn yearless_fall_back_new_york_at_one_thirty() {
        let zone = resolve_iana(Some("America/New_York"));
        let now = utc(2026, 11, 1, 6, 20);
        let got = pick_yearless_wall(&zone, now, week(), slack(), 11, 1, 1, 30);
        assert_eq!(got, Some(utc(2026, 11, 1, 6, 30)));
    }

    #[test]
    fn spring_gap_new_york_two_thirty_is_next_day() {
        let zone = resolve_iana(Some("America/New_York"));
        let now = utc(2026, 3, 8, 22, 0);
        let got = pick_time_only_wall(&zone, now, week(), slack(), 2, 30);
        assert_eq!(got, Some(in_zone("America/New_York", 2026, 3, 9, 2, 30)));
    }

    #[test]
    fn spring_gap_berlin_two_thirty_is_next_day() {
        let zone = resolve_iana(Some("Europe/Berlin"));
        let now = utc(2026, 3, 29, 23, 0);
        let got = pick_time_only_wall(&zone, now, week(), slack(), 2, 30);
        assert_eq!(got, Some(in_zone("Europe/Berlin", 2026, 3, 30, 2, 30)));
    }

    #[test]
    fn santiago_midnight_skip_twelve_thirty_is_day_after_tomorrow() {
        let zone = resolve_iana(Some("America/Santiago"));
        let now = in_zone("America/Santiago", 2026, 9, 5, 23, 31);
        let got = pick_time_only_wall(&zone, now, week(), slack(), 0, 30);
        assert_eq!(got, Some(in_zone("America/Santiago", 2026, 9, 7, 0, 30)));
    }
}
