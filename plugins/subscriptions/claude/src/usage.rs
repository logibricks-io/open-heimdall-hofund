//! Claude Code `/usage` output parsed into quota windows.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{PluginError, QuotaWindow};
use regex::{Regex, RegexSet};

use crate::cli;
use crate::usage_reset::reset_time;

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

fn usage_sign_in_denied() -> &'static RegexSet {
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
        ])
        .expect("sign-in denied patterns")
    })
}

fn usage_limit_denied() -> &'static RegexSet {
    static RE: OnceLock<RegexSet> = OnceLock::new();
    RE.get_or_init(|| {
        RegexSet::new([r"(?i)(session|usage) limit", r"(?i)hit your limit"])
            .expect("limit denied patterns")
    })
}

fn usage_unknown() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?m)^(?:Claude Code: )?(?:Claude Code's /usage is temporarily unavailable|/usage couldn't load the plan's usage)",
        )
        .expect("unknown usage")
    })
}

fn overages_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)using your overages").expect("overages"))
}

/// Runs `claude -p /usage` in `dir` and returns stdout text.
pub(crate) async fn run_usage_command(
    host: &hofund_plugin_api::HostContext,
    command: &crate::launcher::ClaudeCommand,
    dir: &std::path::Path,
    extra_env: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<String, PluginError> {
    let request = command.spawn_request(&cli::USAGE_ARGS, dir, extra_env)?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let mut stdout = child.stdout()?;
    let text = hofund_plugin_sdk::timeout(cli::USAGE_WAIT, cli::collect(&mut stdout)).await;
    drop(child);
    text.ok_or_else(|| PluginError::timeout("Claude Code did not finish reading usage in time"))
}

/// Parses `/usage` text into quota windows.
pub(crate) fn parse(text: &str, now: SystemTime) -> Result<Vec<QuotaWindow>, PluginError> {
    let text = cli::strip_ansi(text).trim().to_owned();
    for line in text.lines() {
        let line = line.trim();
        if usage_limit_denied().is_match(line) {
            return Err(PluginError::quota(format!(
                "Claude Code's /usage told no allowance: {}",
                clip_line(line)
            )));
        }
        if usage_sign_in_denied().is_match(line) {
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
        let used_pct = used.floor().clamp(0.0, 100.0) as u64;
        let (name, model_scope) = if kind == "session" {
            ("5 hours".to_owned(), None)
        } else {
            let mut scope_label = scope.unwrap_or("all models");
            let lower = scope_label.to_ascii_lowercase();
            if let Some(stripped) = lower.strip_suffix(" only") {
                let end = stripped.len();
                scope_label = scope_label[..end].trim();
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
        if text
            .lines()
            .any(|line| usage_unknown().is_match(line.trim()))
        {
            return Err(PluginError::transport(
                "Claude Code's /usage is temporarily unavailable",
            ));
        }
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

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginErrorKind;
    use hofund_plugin_sdk::time::parse_rfc3339;

    use super::*;

    fn utc_at(text: &str) -> SystemTime {
        parse_rfc3339(text).expect("utc")
    }

    fn shanghai_at(text: &str) -> SystemTime {
        parse_rfc3339(text).expect("shanghai")
    }

    #[test]
    fn r5_1_parse_usage_golden() {
        let now = utc_at("2026-10-01T04:00:00Z");
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
        assert_eq!(
            windows[0].resets_at(),
            Some(shanghai_at("2026-10-01T15:30:00+08:00"))
        );
        assert_eq!(windows[1].name(), "7 days");
        assert_eq!(
            windows[1].resets_at(),
            Some(shanghai_at("2026-10-03T14:00:00+08:00"))
        );
        assert_eq!(windows[2].name(), "7 days · Opus");
        assert_eq!(windows[2].scope_model(), Some("opus"));
        assert_eq!(windows[2].used(), 12);
        assert_eq!(
            windows[2].resets_at(),
            Some(shanghai_at("2026-01-02T14:00:00+08:00"))
        );
        assert_eq!(windows[3].name(), "7 days · Fable");
        assert_eq!(windows[3].scope_model(), Some("fable"));
        assert_eq!(windows[3].resets_at(), None);
    }

    #[test]
    fn r5_2_signed_out_usage_is_auth_error() {
        let now = utc_at("2026-10-01T04:00:00Z");
        let error = parse("Error: not logged in", now).expect_err("denied");
        assert_eq!(error.kind(), PluginErrorKind::Auth);
    }

    #[test]
    fn r5_16_limit_line_is_quota_not_auth() {
        let now = utc_at("2026-10-01T04:00:00Z");
        let error = parse("You've hit your limit · resets 3:30am (UTC)", now).expect_err("limit");
        assert_eq!(error.kind(), PluginErrorKind::Quota);
    }

    #[test]
    fn r5_17_usage_unknown_is_transport() {
        let now = utc_at("2026-10-01T04:00:00Z");
        let error = parse("/usage couldn't load the plan's usage", now).expect_err("unknown");
        assert_eq!(error.kind(), PluginErrorKind::Transport);
    }

    #[test]
    fn r5_3_scope_model_normalizes_labels() {
        assert_eq!(scope_to_model("Fable"), "fable");
        assert_eq!(scope_to_model("Fable 5.1"), "fable-5-1");
    }

    #[test]
    fn r5_4_overages_adds_extra_usage_window() {
        let now = utc_at("2026-11-01T13:00:00Z");
        let text = "You are currently using your overages to power your Claude Code usage. We will automatically switch you back to your subscription rate limits when they reset

Current session: 100% used · resets 3pm (UTC)
Current week (all models): 40% used · resets Nov 6 at 11am (UTC)
";
        let windows = parse(text, now).expect("windows");
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].resets_at(), Some(utc_at("2026-11-01T15:00:00Z")));
        assert_eq!(windows[1].resets_at(), Some(utc_at("2026-11-06T11:00:00Z")));
        assert_eq!(windows[2].name(), "Extra usage");
        assert_eq!(windows[2].used(), 100);
        assert_eq!(windows[2].resets_at(), Some(utc_at("2026-11-01T15:00:00Z")));
    }

    #[test]
    fn r5_36_unknown_line_with_session_still_returns_windows() {
        let now = utc_at("2026-10-01T04:00:00Z");
        let text = "/usage couldn't load the plan's usage
Current session: 5% used · resets 3pm (UTC)
";
        let windows = parse(text, now).expect("windows");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].name(), "5 hours");
    }

    #[test]
    fn r6_6_usage_fractional_percent_uses_floor_not_round() {
        let now = utc_at("2026-10-01T04:00:00Z");
        let text = "Current session: 99.5% used · resets 3pm (UTC)
";
        let windows = parse(text, now).expect("windows");
        assert_eq!(windows[0].used(), 99);
        assert!(windows[0].used() < 100);
    }

    #[test]
    fn r5_37_scope_only_suffix_is_case_insensitive() {
        let now = utc_at("2026-10-01T04:00:00Z");
        let text = "Current week (Sonnet ONLY): 10% used
";
        let windows = parse(text, now).expect("windows");
        assert_eq!(windows[0].scope_model(), Some("sonnet"));
    }
}
