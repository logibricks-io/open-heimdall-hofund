//! Claude Code `/usage` output parsed into quota windows.

use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use hofund_plugin_api::{PluginError, QuotaWindow};
use regex::{Regex, RegexSet};

use crate::cli;

/// How long a reset time may read ahead of its window (clock slack).
const RESET_SLACK: Duration = Duration::from_secs(2 * 60 * 60);

/// Seven-day window span for reset parsing.
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Five-hour session window span.
const SESSION: Duration = Duration::from_secs(5 * 60 * 60);

/// Why setup-token accounts have no subscription usage windows.
pub(crate) const SETUP_TOKEN_QUOTA: &str = "unknown: a setup-token can't read the allowance (Claude Code's /usage needs a full sign-in); it shows once a reply tells it";

fn usage_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^Current (session|week(?: \(([^)]+)\))?):\s*([0-9.]+)% used(?:\s*·\s*resets (.+))?$",
        )
        .expect("usage line pattern")
    })
}

fn usage_denied() -> &'static RegexSet {
    static RE: OnceLock<RegexSet> = OnceLock::new();
    RE.get_or_init(|| {
        RegexSet::new([
            r"(?i)\b(401|403)\b",
            r"(?i)not (logged|signed) in",
            r"(?i)signed out",
            r"(?i)sign-in (has )?expired",
            r"(?i)unauthorized",
            r"(?i)forbidden",
            r"(?i)authentication (failed|required)",
            r"(?i)invalid (access )?token",
            r"(?i)(session|usage) limit",
            r"(?i)hit your limit",
        ])
        .expect("denied patterns")
    })
}

fn overages_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)using your overages").expect("overages"))
}

/// Runs `claude -p /usage` in the account config directory and parses windows.
pub(crate) async fn read(
    host: &hofund_plugin_api::HostContext,
    program: &std::path::Path,
    dir: &std::path::Path,
    now: SystemTime,
    blob: &hofund_plugin_sdk::oauth::SecretObject,
) -> Result<Vec<QuotaWindow>, PluginError> {
    let request = cli::request_with_extra(
        program,
        &cli::USAGE_ARGS,
        dir,
        &crate::account::run_env(blob),
    )?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let mut stdout = child.stdout()?;
    let text = hofund_plugin_sdk::timeout(cli::USAGE_WAIT, cli::collect(&mut stdout)).await;
    drop(child);
    let Some(text) = text else {
        return Err(PluginError::timeout(
            "Claude Code did not finish reading usage in time",
        ));
    };
    parse(&text, now)
}

/// Parses `/usage` text into quota windows.
pub(crate) fn parse(text: &str, now: SystemTime) -> Result<Vec<QuotaWindow>, PluginError> {
    let text = cli::strip_ansi(text).trim().to_owned();
    for line in text.lines() {
        let line = line.trim();
        if usage_denied().is_match(line) {
            return Err(PluginError::auth(format!(
                "Claude Code's /usage told no allowance: {}",
                clip_line(line)
            )));
        }
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(caps) = usage_line().captures(line) else {
            continue;
        };
        let kind = caps.get(1).expect("kind").as_str();
        let scope = caps.get(2).map(|m| m.as_str().trim());
        let used = caps
            .get(3)
            .expect("used")
            .as_str()
            .parse::<f64>()
            .unwrap_or(0.0);
        let used_pct = used.round().clamp(0.0, 100.0) as u64;
        let (name, model_scope) = if kind == "session" {
            ("5 hours".to_owned(), None)
        } else {
            let mut scope_label = scope.unwrap_or("all models");
            if let Some(stripped) = scope_label
                .strip_suffix(" only")
                .or_else(|| scope_label.strip_suffix(" Only"))
            {
                scope_label = stripped.trim();
            }
            if scope_label.eq_ignore_ascii_case("all models") {
                ("7 days".to_owned(), None)
            } else {
                (
                    format!("7 days · {scope_label}"),
                    Some(scope_to_model(scope_label)),
                )
            }
        };
        if out.iter().any(|w: &QuotaWindow| w.name() == name) {
            continue;
        }
        let span = if kind == "session" { SESSION } else { WEEK };
        let reset = caps.get(4).map(|m| m.as_str());
        let mut window = QuotaWindow::new(name, used_pct, Some(100), reset_time(reset, now, span))?;
        if let Some(model) = model_scope.filter(|m| !m.is_empty()) {
            window = window.with_scope_model(model);
        }
        out.push(window);
    }
    if text
        .lines()
        .any(|line| overages_line().is_match(line.trim()))
    {
        out.push(overages_window(&out)?);
    }
    if out.is_empty() {
        if text.is_empty()
            || text.lines().any(|line| {
                line.trim().trim_end_matches('.')
                    == "You are currently using your subscription to power your Claude Code usage"
            })
        {
            return Err(PluginError::transport(
                "Claude Code's /usage is temporarily unavailable",
            ));
        }
        let first = text.lines().next().unwrap_or("");
        return Err(PluginError::transport(format!(
            "Claude Code's /usage showed no usage figures: {}",
            clip_line(first)
        )));
    }
    Ok(out)
}

fn overages_window(windows: &[QuotaWindow]) -> Result<QuotaWindow, PluginError> {
    let mut resets = None;
    for window in windows {
        if window.used() >= 100
            && let Some(at) = window.resets_at()
            && resets.is_none_or(|r| at > r)
        {
            resets = Some(at);
        }
    }
    QuotaWindow::new("Extra usage", 100, Some(100), resets)
}

/// Maps a scoped window label to the model id prefix Claude counts.
pub(crate) fn scope_to_model(name: &str) -> String {
    name.to_ascii_lowercase().replace([' ', '.'], "-")
}

fn clip_line(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > 120 {
        format!("{}…", chars[..120].iter().collect::<String>())
    } else {
        s.to_owned()
    }
}

fn reset_time(reset: Option<&str>, now: SystemTime, span: Duration) -> Option<SystemTime> {
    let reset = reset.map(str::trim).filter(|s| !s.is_empty())?;
    let (zone_name, rest) = split_zone(reset);
    let loc = zone_name
        .and_then(|name| chrono_tz::Tz::from_str(name).ok())
        .unwrap_or(chrono_tz::UTC);
    let now_dt = datetime_from_system(now)?.with_timezone(&loc);
    let slack = chrono::Duration::from_std(span + RESET_SLACK)
        .unwrap_or_else(|_| chrono::Duration::hours(2));
    let latest = now_dt + slack;
    let rest = rest
        .replace("AM", "am")
        .replace("PM", "pm")
        .replace("Am", "am")
        .replace("Pm", "pm");
    for layout in [
        "Jan 2 at 3:04pm",
        "Jan 2 at 3pm",
        "Jan 2, 2006 at 3:04pm",
        "Jan 2, 2006 at 3pm",
        "Jan 2, 3:04pm",
        "Jan 2, 3pm",
        "Jan 2, 2006, 3:04pm",
        "Jan 2, 2006, 3pm",
    ] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(&rest, layout) {
            if parsed.year() != 0 {
                return system_from_datetime(parsed.and_utc());
            }
            let ref_year = now_dt.year();
            for year in (ref_year - 1)..=(ref_year + 1) {
                let Some(candidate) = loc
                    .with_ymd_and_hms(
                        year,
                        parsed.month(),
                        parsed.day(),
                        parsed.hour(),
                        parsed.minute(),
                        0,
                    )
                    .single()
                else {
                    continue;
                };
                if candidate >= now_dt - chrono::Duration::days(1) && candidate <= latest {
                    return system_from_datetime(candidate);
                }
            }
            let Some(past) = loc
                .with_ymd_and_hms(
                    ref_year,
                    parsed.month(),
                    parsed.day(),
                    parsed.hour(),
                    parsed.minute(),
                    0,
                )
                .single()
            else {
                continue;
            };
            if past < now_dt {
                return system_from_datetime(past);
            }
        }
    }
    for layout in ["3:04pm", "3pm"] {
        let Ok(parsed) = NaiveTime::parse_from_str(&rest, layout) else {
            continue;
        };
        let ref_date = now_dt.date_naive();
        for day_offset in 0..=2i64 {
            let day = ref_date + chrono::Duration::days(day_offset);
            let Some(candidate) = loc.from_local_datetime(&day.and_time(parsed)).single() else {
                continue;
            };
            if candidate.time() == parsed && candidate >= now_dt && candidate <= latest {
                return system_from_datetime(candidate);
            }
        }
        for day_offset in 0..=2i64 {
            let day = ref_date - chrono::Duration::days(day_offset);
            let Some(candidate) = loc.from_local_datetime(&day.and_time(parsed)).single() else {
                continue;
            };
            if candidate.time() == parsed && candidate < now_dt {
                return system_from_datetime(candidate);
            }
        }
    }
    None
}

fn split_zone(s: &str) -> (Option<&str>, String) {
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

fn datetime_from_system(now: SystemTime) -> Option<chrono::DateTime<Utc>> {
    let dur = now.duration_since(UNIX_EPOCH).ok()?;
    Utc.timestamp_opt(dur.as_secs() as i64, dur.subsec_nanos())
        .single()
}

fn system_from_datetime<Tz: TimeZone>(dt: chrono::DateTime<Tz>) -> Option<SystemTime> {
    let secs = dt.timestamp();
    if secs < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> SystemTime {
        let dt = chrono::Utc
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("time");
        UNIX_EPOCH + Duration::from_secs(dt.timestamp() as u64)
    }

    #[test]
    fn r5_1_parse_usage_golden() {
        let now = utc(2026, 10, 1, 4, 0);
        let text = "You are currently using your subscription to power your Claude Code usage

Current session: 13% used · resets Oct 1 at 3:30pm (Asia/Shanghai)
Current week (all models): 4% used · resets Oct 3 at 2pm (Asia/Shanghai)
Current week (Opus): 12.5% used · resets Jan 2 at 2pm (Asia/Shanghai)
Current week (Fable): 0% used

What's contributing to your limits usage?
  96% of your usage came from sessions active for 8+ hours";
        let windows = parse(text, now).expect("windows");
        assert_eq!(windows.len(), 4);
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 13);
        assert_eq!(windows[1].name(), "7 days");
        assert_eq!(windows[2].name(), "7 days · Opus");
        assert_eq!(windows[2].scope_model(), Some("opus"));
        assert_eq!(windows[3].name(), "7 days · Fable");
        assert_eq!(windows[3].scope_model(), Some("fable"));
    }

    #[test]
    fn r5_2_signed_out_usage_is_auth_error() {
        let now = utc(2026, 10, 1, 4, 0);
        let error = parse("Error: not logged in", now).expect_err("denied");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Auth);
    }

    #[test]
    fn r5_3_scope_model_normalizes_labels() {
        assert_eq!(scope_to_model("Fable"), "fable");
        assert_eq!(scope_to_model("Fable 5.1"), "fable-5-1");
    }

    #[test]
    fn r5_4_overages_adds_extra_usage_window() {
        let now = utc(2026, 11, 1, 13, 0);
        let text = "You are currently using your overages to power your Claude Code usage. We will automatically switch you back to your subscription rate limits when they reset

Current session: 100% used · resets 3pm (UTC)
Current week (all models): 40% used · resets Nov 6 at 11am (UTC)
";
        let windows = parse(text, now).expect("windows");
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[2].name(), "Extra usage");
        assert_eq!(windows[2].used(), 100);
    }
}
