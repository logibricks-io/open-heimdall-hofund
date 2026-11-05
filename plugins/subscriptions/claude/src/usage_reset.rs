//! Reset time parsing for Claude Code `/usage` lines (reference `claudeResetTime`).

use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use hofund_plugin_sdk::time::zone::{
    chrono_span, local_wall_instant, pick_time_only, pick_yearless, resolve_iana, utc_datetime,
};
use regex::Regex;

/// How long a reset time may read ahead of its window (clock slack).
pub(super) const RESET_SLACK: Duration = Duration::from_secs(2 * 60 * 60);

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
    let loc = resolve_iana(zone_name);
    let ref_dt = utc_datetime(now)?.with_timezone(&loc);
    let slack = chrono_span(span + RESET_SLACK);
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
            return local_wall_instant(&loc, year, month, day, hour, minute);
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

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_sdk::time::zone::{
        TimeZone, chrono_tz::America::Los_Angeles, chrono_tz::Asia::Shanghai, chrono_tz::Tz,
        set_test_host_local,
    };
    use std::time::{Duration, UNIX_EPOCH};

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> SystemTime {
        let dt = hofund_plugin_sdk::time::zone::chrono::Utc
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
        set_test_host_local(Some("America/Los_Angeles"));
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
        set_test_host_local(Some("America/Los_Angeles"));
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
            set_test_host_local(None);
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
