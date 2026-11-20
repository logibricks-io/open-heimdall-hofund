//! Claude Code `/usage` output parsed into quota windows.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{PluginError, PluginErrorKind, QuotaWindow};
use regex::{Regex, RegexSet};
use serde::Deserialize;
use serde_json::Value;

use crate::cli;
use crate::usage_reset::reset_time;

/// Seven-day window span for reset parsing.
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Five-hour session window span.
const SESSION: Duration = Duration::from_secs(5 * 60 * 60);

/// Line added when a usage report's `limits` array is empty.
pub(crate) const NO_METERS_LINE: &str = "Usage report: no meters on this plan";

/// Window name for a plan that has no 5-hour or weekly meters.
pub(crate) const NO_METERS_WINDOW: &str = "5-hour and weekly limits";

/// Display text for [`NO_METERS_WINDOW`].
pub(crate) const NO_METERS_DISPLAY: &str = "None on this plan";

/// Sentence Claude Code prints when `/usage` has no window lines.
const SUBSCRIPTION_LINE: &str =
    "You are currently using your subscription to power your Claude Code usage";

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

/// Whether a `/usage` error line is an account refusal, not a quota stand-in.
pub(crate) fn line_denies_account(message: &str) -> bool {
    usage_sign_in_denied().is_match(message) || usage_limit_denied().is_match(message)
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

/// Parses `/usage` text, and a usage-based seat whose text drew nothing, into windows.
pub(crate) fn parse_for_dir(
    text: &str,
    now: SystemTime,
    dir: &Path,
) -> Result<Vec<QuotaWindow>, PluginError> {
    match parse(text, now) {
        Err(err)
            if err.kind() == PluginErrorKind::Transport
                && subscription_only(text)
                && usage_based_seat(dir) =>
        {
            Ok(vec![no_limits_window()?])
        }
        other => other,
    }
}

/// Whether `.claude.json` in `dir` names a seat Claude Code holds no rate limits for.
#[must_use]
pub(crate) fn usage_based_seat(dir: &Path) -> bool {
    let Some(text) = crate::creds::read_capped(&dir.join(crate::creds::PROFILE_FILE)) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    let Some(account) = value.get("oauthAccount") else {
        return false;
    };
    let seat = account
        .get("seatTier")
        .and_then(Value::as_str)
        .unwrap_or("");
    let billing = account
        .get("billingType")
        .and_then(Value::as_str)
        .unwrap_or("");
    seat == "enterprise_usage_based" || billing == "usage_based"
}

/// Joins a JSON `/usage` result with report rows when the text drew no window.
#[must_use]
pub(crate) fn attach_report(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return raw.to_owned();
    };
    match value {
        Value::Object(object) => object
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or(raw)
            .to_owned(),
        Value::Array(items) => {
            let Some((result, is_error)) = items.iter().rev().find_map(|item| {
                let object = item.as_object()?;
                if object.get("type").and_then(Value::as_str) != Some("result") {
                    return None;
                }
                let result = object.get("result").and_then(Value::as_str)?;
                let is_error = object
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Some((result.to_owned(), is_error))
            }) else {
                return raw.to_owned();
            };
            if is_error {
                return result;
            }
            let extra = report_lines(&items, &result);
            format!("{result}{extra}")
        }
        _ => raw.to_owned(),
    }
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
        if text.lines().any(|line| line.trim() == NO_METERS_LINE) {
            let mut aside = vec![no_limits_window()?];
            if let Some(credits) = credits_window(&text) {
                aside.push(credits);
            }
            return Ok(aside);
        }
        if text
            .lines()
            .any(|line| usage_unknown().is_match(line.trim()))
        {
            return Err(PluginError::transport(
                "Claude Code's /usage is temporarily unavailable",
            ));
        }
        if subscription_only(&text) {
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
    if let Some(credits) = credits_window(&text) {
        out.push(credits);
    }
    Ok(out)
}

fn subscription_only(text: &str) -> bool {
    let text = cli::strip_ansi(text);
    let text = text.trim();
    text.is_empty()
        || text
            .lines()
            .any(|line| line.trim().trim_end_matches('.') == SUBSCRIPTION_LINE)
}

fn no_limits_window() -> Result<QuotaWindow, PluginError> {
    Ok(QuotaWindow::new(NO_METERS_WINDOW, 0, None, None)?
        .with_display(NO_METERS_DISPLAY)
        .with_blocking(false))
}

fn credits_window(text: &str) -> Option<QuotaWindow> {
    for line in text.lines() {
        let Some(caps) = extra_usage_line().captures(line.trim()) else {
            continue;
        };
        let spent = caps.get(1)?.as_str().parse::<f64>().ok()?;
        let limit = caps.get(2)?.as_str().parse::<f64>().ok()?;
        if !spent.is_finite() || !limit.is_finite() || limit <= 0.0 {
            continue;
        }
        let currency = caps.get(3)?.as_str().to_ascii_uppercase();
        let used_pct = ((spent / limit) * 100.0).clamp(0.0, 100.0).floor() as u64;
        let window = QuotaWindow::new("Usage credits", used_pct, Some(100), None)
            .ok()?
            .with_blocking(false)
            .with_provider_count(spent, limit, currency);
        return Some(window);
    }
    None
}

fn extra_usage_line() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^Extra usage: ([0-9.]+) / ([0-9.]+) ([A-Za-z]{3}) spent$")
            .expect("extra usage line")
    })
}

#[derive(Deserialize)]
struct LimitRow {
    kind: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<RowScope>,
}

#[derive(Deserialize)]
struct RowScope {
    model: Option<RowModel>,
}

#[derive(Deserialize)]
struct RowModel {
    display_name: Option<String>,
}

fn report_lines(items: &[Value], text: &str) -> String {
    for item in items {
        let Some(limits) = item
            .get("usage_report")
            .and_then(|report| report.get("rate_limits"))
        else {
            continue;
        };
        if !limits.is_object() {
            continue;
        }
        let mut add = String::new();
        if let Some(raw_rows) = limits.get("limits").filter(|rows| !rows.is_null())
            && let Ok(rows) = serde_json::from_value::<Vec<LimitRow>>(raw_rows.clone())
            && !text_has_window(text)
        {
            if rows.is_empty() {
                add.push('\n');
                add.push_str(NO_METERS_LINE);
            } else {
                for row in &rows {
                    if let Some(line) = row_line(row) {
                        add.push('\n');
                        add.push_str(&line);
                    }
                }
            }
        }
        add.push_str(&extra_suffix(limits));
        return add;
    }
    String::new()
}

fn text_has_window(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line.starts_with("Current session:") || line.starts_with("Current week")
    })
}

fn row_line(row: &LimitRow) -> Option<String> {
    let percent = row.percent.filter(|value| value.is_finite())?;
    let head = match row.kind.as_deref() {
        Some("session") => "Current session".to_owned(),
        Some("weekly_all") => "Current week (all models)".to_owned(),
        Some("weekly_scoped") => {
            let name = row
                .scope
                .as_ref()
                .and_then(|scope| scope.model.as_ref())
                .and_then(|model| model.display_name.as_deref())
                .map(str::trim)
                .filter(|name| !name.is_empty() && !name.contains(['(', ')']))?;
            format!("Current week ({name})")
        }
        _ => return None,
    };
    let mut line = format!("{head}: {}% used", percent_text(percent));
    if let Some(reset) = row
        .resets_at
        .as_deref()
        .map(str::trim)
        .filter(|reset| !reset.is_empty())
    {
        line.push_str(" · resets ");
        line.push_str(reset);
    }
    Some(line)
}

fn percent_text(value: f64) -> String {
    let text = value.to_string();
    if text.contains(['e', 'E']) {
        let trimmed = format!("{value:.6}");
        trimmed
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    } else {
        text
    }
}

fn extra_suffix(limits: &Value) -> String {
    let Some(extra) = limits.get("extra_usage").filter(|extra| !extra.is_null()) else {
        return String::new();
    };
    let on = extra
        .get("is_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(limit) = extra.get("monthly_limit").and_then(Value::as_f64) else {
        return String::new();
    };
    if !on || !limit.is_finite() || limit <= 0.0 {
        return String::new();
    }
    let used = extra
        .get("used_credits")
        .and_then(Value::as_f64)
        .filter(|used| used.is_finite())
        .unwrap_or(0.0);
    let currency = extra
        .get("currency")
        .and_then(Value::as_str)
        .filter(|currency| !currency.is_empty())
        .unwrap_or("USD");
    format!(
        "\nExtra usage: {:.2} / {:.2} {} spent",
        used / 100.0,
        limit / 100.0,
        currency.to_ascii_uppercase()
    )
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
    fn d238_no_meters_line_is_an_aside_not_a_failed_read() {
        let now = utc_at("2026-11-19T13:00:00Z");
        let text = "You are currently using your subscription to power your Claude Code usage
Usage report: no meters on this plan
";
        let windows = parse(text, now).expect("aside");
        assert_eq!(windows[0].name(), "5-hour and weekly limits");
        assert_eq!(windows[0].display(), "None on this plan");
        assert!(!windows[0].blocking());
    }

    #[test]
    fn d238_report_rows_fill_a_text_that_drew_no_window() {
        let now = utc_at("2026-11-19T13:00:00Z");
        let text = attach_report(include_str!("fixtures/usage/enterprise_rows.json"));
        assert!(text.contains("Current session: 37.5% used · resets 2026-11-19T15:00:00.000Z"));
        assert!(text.contains("Current week (all models): 8% used · resets 2026-11-26T13:00:00Z"));
        assert!(text.contains("Current week (Fable): 4% used"));
        assert!(!text.contains("Claude Design"));
        assert!(!text.contains("new_meter"));
        assert!(!text.contains("50%"));
        let windows = parse(&text, now).expect("rows");
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 37);
        assert_eq!(windows[0].resets_at(), Some(utc_at("2026-11-19T15:00:00Z")));
        assert_eq!(windows[1].name(), "7 days");
        assert_eq!(windows[1].used(), 8);
        assert_eq!(windows[1].resets_at(), Some(utc_at("2026-11-26T13:00:00Z")));
        assert_eq!(windows[2].name(), "7 days · Fable");
        assert_eq!(windows[2].scope_model(), Some("fable"));
        assert_eq!(windows[2].used(), 4);
        assert!(windows[2].resets_at().is_none());
    }

    #[test]
    fn d238_text_that_drew_a_window_ignores_report_rows() {
        let text = attach_report(include_str!("fixtures/usage/text_drew_window.json"));
        assert_eq!(text, "Current session: 13% used");
    }

    #[test]
    fn d238_empty_limits_with_credits_is_an_aside_beside_usage_credits() {
        let now = utc_at("2026-11-19T13:00:00Z");
        let text = attach_report(include_str!("fixtures/usage/no_meters_credits.json"));
        assert!(text.contains(NO_METERS_LINE));
        assert!(text.contains("Extra usage: 12.00 / 500.00 USD spent"));
        let windows = parse(&text, now).expect("aside");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), NO_METERS_WINDOW);
        assert_eq!(windows[0].display(), NO_METERS_DISPLAY);
        assert!(!windows[0].blocking());
        assert_eq!(windows[1].name(), "Usage credits");
        assert!(!windows[1].blocking());
        assert_eq!(windows[1].used(), 2);
        let amount = windows[1].provider_amount().expect("spent");
        let limit = windows[1].provider_limit().expect("cap");
        assert!((amount - 12.0).abs() < 0.001);
        assert!((limit - 500.0).abs() < 0.001);
        assert_eq!(windows[1].unit(), "USD");
    }

    #[test]
    fn d238_bad_row_keeps_the_extra_line_and_stays_unavailable() {
        let now = utc_at("2026-11-19T13:00:00Z");
        let text = attach_report(include_str!("fixtures/usage/bad_row.json"));
        assert!(text.contains("Extra usage: 1.00 / 500.00 USD spent"));
        assert!(!text.contains("Current session:"));
        let error = parse(&text, now).expect_err("unavailable");
        assert_eq!(error.kind(), PluginErrorKind::Transport);
    }

    #[test]
    fn d238_null_limits_and_null_rate_limits_add_nothing() {
        let now = utc_at("2026-11-19T13:00:00Z");
        let subscription =
            "You are currently using your subscription to power your Claude Code usage";
        for fixture in [
            include_str!("fixtures/usage/limits_null.json"),
            include_str!("fixtures/usage/rate_limits_null.json"),
            include_str!("fixtures/usage/no_meters.json"),
        ] {
            let text = attach_report(fixture);
            if fixture.contains("\"limits\":[]") {
                let windows = parse(&text, now).expect("empty limits");
                assert_eq!(windows[0].display(), NO_METERS_DISPLAY);
            } else {
                assert_eq!(text, subscription);
                let error = parse(&text, now).expect_err("unavailable");
                assert_eq!(error.kind(), PluginErrorKind::Transport);
            }
        }
    }

    #[test]
    fn d238_usage_based_seat_is_an_aside_and_other_seats_stay_unavailable() {
        let now = utc_at("2026-11-19T13:00:00Z");
        let text = "You are currently using your subscription to power your Claude Code usage";
        let cases = [
            (
                r#"{"oauthAccount":{"seatTier":"enterprise_usage_based","emailAddress":"a@example.com"}}"#,
                true,
            ),
            (r#"{"oauthAccount":{"billingType":"usage_based"}}"#, true),
            (
                r#"{"oauthAccount":{"seatTier":"enterprise_standard"}}"#,
                false,
            ),
            (
                r#"{"oauthAccount":{"emailAddress":"a@example.com"}}"#,
                false,
            ),
            ("", false),
        ];
        for (account, none) in cases {
            let dir = tempfile::tempdir().expect("dir");
            if !account.is_empty() {
                std::fs::write(dir.path().join(crate::creds::PROFILE_FILE), account)
                    .expect("profile");
            }
            let parsed = parse_for_dir(text, now, dir.path());
            if none {
                let windows = parsed.expect("aside");
                assert_eq!(windows.len(), 1);
                assert_eq!(windows[0].display(), NO_METERS_DISPLAY);
                assert!(!windows[0].blocking());
            } else {
                let error = parsed.expect_err("unavailable");
                assert_eq!(error.kind(), PluginErrorKind::Transport);
            }
        }
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join(crate::creds::PROFILE_FILE),
            r#"{"oauthAccount":{"seatTier":"enterprise_usage_based"}}"#,
        )
        .expect("profile");
        let failed = parse_for_dir("/usage couldn't load the plan's usage", now, dir.path());
        assert_eq!(failed.expect_err("load").kind(), PluginErrorKind::Transport);
    }

    #[test]
    fn d238_rfc3339_nano_reset_is_the_session_window() {
        let now = utc_at("2026-11-19T13:00:00Z");
        let text = "Current session: 37.5% used · resets 2026-11-19T15:00:00.000Z
";
        let windows = parse(text, now).expect("session");
        assert_eq!(windows[0].name(), "5 hours");
        assert_eq!(windows[0].used(), 37);
        assert_eq!(windows[0].resets_at(), Some(utc_at("2026-11-19T15:00:00Z")));
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
