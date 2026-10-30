//! Time stamps as vendors write them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Reads `YYYY-MM-DD`, or RFC 3339 `YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM)`.
/// A date alone is midnight UTC.
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let date = text.get(..10)?;
    let days = days_from_civil(field(date, 0, 4)?, field(date, 5, 7)?, field(date, 8, 10)?)?;
    if date.as_bytes()[4] != b'-' || date.as_bytes()[7] != b'-' {
        return None;
    }
    let mut seconds = days.checked_mul(86_400)?;
    let rest = &text[10..];
    if !rest.is_empty() {
        let time = rest.strip_prefix(['T', 't', ' '])?;
        let clock = time.get(..8)?;
        if clock.as_bytes()[2] != b':' || clock.as_bytes()[5] != b':' {
            return None;
        }
        let (hour, minute, second) = (
            field(clock, 0, 2)?,
            field(clock, 3, 5)?,
            field(clock, 6, 8)?,
        );
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        seconds += i64::from(hour * 3600 + minute * 60 + second);
        let mut zone = &time[8..];
        if let Some(fraction) = zone.strip_prefix('.') {
            let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return None;
            }
            zone = &fraction[digits..];
        }
        seconds -= offset(zone)?;
    }
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// `instant` as RFC 3339 UTC with nanoseconds, `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`.
/// A time before 1970 is written as the epoch.
#[must_use]
pub fn format_rfc3339(instant: SystemTime) -> String {
    let since = instant.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX / 2);
    let (year, month, day) = civil_from_days(days);
    let time = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}Z",
        time / 3_600,
        (time % 3_600) / 60,
        time % 60,
        since.subsec_nanos()
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// The zone offset in seconds east of UTC.
fn offset(zone: &str) -> Option<i64> {
    if zone == "Z" || zone == "z" {
        return Some(0);
    }
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &zone[1..];
    if rest.len() != 5 || rest.as_bytes()[2] != b':' {
        return None;
    }
    let (hours, minutes) = (field(rest, 0, 2)?, field(rest, 3, 5)?);
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * i64::from(hours * 3600 + minutes * 60))
}

fn field(text: &str, from: usize, to: usize) -> Option<u32> {
    let part = text.get(from..to)?;
    if !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: u32, month: u32, day: u32) -> Option<i64> {
    let leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let index = usize::try_from(month.checked_sub(1)?).ok()?;
    if day == 0 || day > *month_days.get(index)? {
        return None;
    }
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{format_rfc3339, parse_rfc3339};

    #[test]
    fn times_format_as_rfc3339_and_read_back() {
        let cases = [
            (0, 0, "1970-01-01T00:00:00.000000000Z"),
            (951_868_800, 0, "2000-03-01T00:00:00.000000000Z"),
            (1_709_209_815, 5, "2024-02-29T12:30:15.000000005Z"),
            (1_793_491_199, 999_999_999, "2026-10-31T23:59:59.999999999Z"),
            (4_107_542_400, 120_000_000, "2100-03-01T00:00:00.120000000Z"),
        ];
        for (seconds, nanos, text) in cases {
            let instant = UNIX_EPOCH + Duration::new(seconds, nanos);
            assert_eq!(format_rfc3339(instant), text);
            assert_eq!(parse_rfc3339(text), at(seconds), "{text}");
        }
        let before = UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(format_rfc3339(before), "1970-01-01T00:00:00.000000000Z");
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "compared with what the parser returns"
    )]
    fn at(seconds: u64) -> Option<SystemTime> {
        Some(UNIX_EPOCH + Duration::from_secs(seconds))
    }

    #[test]
    fn reset_times_parse() {
        let parse = parse_rfc3339;
        assert_eq!(parse("2026-11-01"), at(1_793_491_200));
        assert_eq!(parse("2026-11-01T00:00:00Z"), at(1_793_491_200));
        assert_eq!(parse("2026-11-01T00:00:00.000Z"), at(1_793_491_200));
        assert_eq!(parse("2024-02-29T12:30:15Z"), at(1_709_209_815));
        assert_eq!(parse("2024-02-29t12:30:15z"), at(1_709_209_815));
        assert_eq!(parse("2024-02-29T20:30:15+08:00"), at(1_709_209_815));
        assert_eq!(parse("2024-02-29T04:00:15-08:30"), at(1_709_209_815));
        assert_eq!(parse("2000-03-01"), at(951_868_800));
        assert_eq!(parse("1970-01-01"), at(0));
        for bad in [
            "",
            "2026-11-0",
            "2026/11/01",
            "2026-13-01",
            "2026-00-01",
            "2026-02-29",
            "2026-11-00",
            "2026-11-31",
            "1969-12-31",
            "2026-11-01X",
            "2026-11-01T24:00:00Z",
            "2026-11-01T00:60:00Z",
            "2026-11-01T00:00:61Z",
            "2026-11-01T00-00-00Z",
            "2026-11-01T00:00:00",
            "2026-11-01T00:00:00.Z",
            "2026-11-01T00:00:00+0800",
            "2026-11-01T00:00:00+24:00",
            "2026-11-01T00:00:00+08:60",
            "2026-11-01T00:00:00*08:00",
            "+026-11-01",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(parse("2100-03-01"), at(4_107_542_400));
        assert_eq!(parse("2000-02-29"), at(951_782_400));
        assert_eq!(parse("2100-02-29"), None);
    }
}
