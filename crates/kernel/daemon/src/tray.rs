//! Status text for the menu-bar tray, read without contacting upstreams.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, TimeZone, Utc};
use hofund_control_proto::{Quota, QuotaQuery, RouteQuery};
use hofund_gateway::{QuotaSource, Running};
use hofund_usage::{Ledger, Totals};

/// What the menu-bar tray shows beside the gateway state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayStatus {
    /// The newest route, or that no requests have arrived.
    pub route: String,
    /// Requests and tokens recorded since local midnight.
    pub today: String,
    /// One line per quota card, in report order.
    pub quotas: Vec<String>,
}

/// Builds tray status from retained daemon state and the usage ledger.
///
/// The handles are taken from `running` before the returned future starts,
/// so the future is `Send + 'static` and never borrows the server, which is
/// not `Sync`.
pub fn tray_status(
    running: &Running,
    config_path: &Path,
) -> impl Future<Output = TrayStatus> + Send + 'static {
    let route = running
        .route_trace_book()
        .and_then(|book| {
            book.list(&RouteQuery {
                limit: 1,
                ..RouteQuery::default()
            })
            .decisions
            .into_iter()
            .next()
        })
        .map_or_else(
            || "Route: no requests yet".to_owned(),
            |decision| {
                route_line(
                    &decision.asked,
                    &decision.served_provider,
                    &decision.served_member,
                    decision.done,
                    decision.status,
                )
            },
        );
    let source = running.quota_source();
    let config_path = config_path.to_path_buf();
    async move { tray_report(route, source, &config_path).await }
}

async fn tray_report(
    route: String,
    source: Option<Arc<dyn QuotaSource>>,
    config_path: &Path,
) -> TrayStatus {
    let today = today(config_path).await;
    let quotas = match source {
        Some(source) => {
            match tokio::time::timeout(Duration::from_secs(2), source.report(QuotaQuery::kept()))
                .await
            {
                Ok(report) => {
                    let now = Utc::now();
                    report
                        .data
                        .iter()
                        .filter_map(|quota| quota_line(quota, now))
                        .collect()
                }
                Err(_) => Vec::new(),
            }
        }
        None => Vec::new(),
    };
    TrayStatus {
        route,
        today,
        quotas,
    }
}

async fn today(config_path: &Path) -> String {
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0);
    let since = midnight.and_then(|midnight| {
        Local
            .from_local_datetime(&midnight)
            .earliest()
            .map(|midnight| midnight.timestamp())
    });
    let Some(since) = since else {
        return "Today: unavailable".to_owned();
    };
    let path = config_path.to_path_buf();
    let totals = tokio::task::spawn_blocking(move || {
        let config = hofund_config::load_path(&path).ok()?;
        let ledger = Ledger::open(&crate::beside(&path, &config.usage_file)).ok()?;
        ledger
            .summary(Some(since), 0)
            .ok()
            .map(|(_, totals)| totals)
    })
    .await;
    match totals {
        Ok(Some(totals)) => today_line(totals),
        Ok(None) | Err(_) => "Today: unavailable".to_owned(),
    }
}

fn route_line(
    asked: &str,
    served_provider: &str,
    served_member: &str,
    done: bool,
    status: Option<u16>,
) -> String {
    if served_provider.is_empty() && !done {
        return format!("Route: {asked} → routing");
    }
    if served_provider.is_empty() {
        let status =
            status.map_or_else(|| "unknown status".to_owned(), |status| status.to_string());
        return format!("Route: {asked} → not served ({status})");
    }
    format!("Route: {asked} → {served_provider}/{served_member}")
}

fn today_line(totals: Totals) -> String {
    if totals.requests == 0 {
        return "Today: no requests yet".to_owned();
    }
    let noun = if totals.requests == 1 {
        "request"
    } else {
        "requests"
    };
    let tokens = totals.input_tokens.saturating_add(totals.output_tokens);
    format!(
        "Today: {} {noun}, {} tokens",
        totals.requests,
        compact_tokens(tokens)
    )
}

fn compact_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

fn quota_line(quota: &Quota, now: DateTime<Utc>) -> Option<String> {
    let mut label = if quota.alias.is_empty() {
        quota.name.clone()
    } else {
        quota.alias.clone()
    };
    if !quota.user.is_empty() {
        label.push_str(&format!(" ({})", quota.user));
    }
    if !quota.error.is_empty() {
        return Some(format!("{label}: {}", quota.error));
    }
    let windows: Vec<_> = quota
        .windows
        .iter()
        .filter(|window| !window.text && !window.unlimited && window.balance.is_none())
        .take(2)
        .collect();
    if windows.is_empty() {
        return (!quota.balance.is_empty()).then(|| format!("{label}: {}", quota.balance));
    }
    let percentages = windows
        .iter()
        .map(|window| format!("{}%", window.used.clamp(0.0, 100.0).round() as u64))
        .collect::<Vec<_>>()
        .join(" · ");
    let mut line = format!("{label}: {percentages}");
    let reset = windows
        .iter()
        .filter_map(|window| window.resets_at.as_ref())
        .filter_map(|time| DateTime::parse_from_rfc3339(time.as_str()).ok())
        .map(|time| time.with_timezone(&Utc))
        .filter(|time| *time > now)
        .min();
    if let Some(reset) = reset {
        line.push_str(&format!(
            " · resets in {}",
            until((reset - now).to_std().unwrap_or_default())
        ));
    }
    Some(line)
}

fn until(wait: Duration) -> String {
    let minutes = wait.as_secs().saturating_add(30) / 60;
    let days = minutes / (24 * 60);
    let hours = minutes / 60 % 24;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {}m", minutes % 60)
    } else {
        format!("{}m", minutes.max(1))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeDelta, Utc};
    use hofund_control_proto::{Quota, QuotaSpan, QuotaTime};
    use hofund_usage::Totals;

    use super::{compact_tokens, quota_line, route_line, today_line, until};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
            .expect("now")
            .with_timezone(&Utc)
    }

    fn span(used: f64) -> QuotaSpan {
        QuotaSpan {
            name: "window".to_owned(),
            used,
            remaining: 100.0 - used,
            ..QuotaSpan::default()
        }
    }

    fn reset(span: &mut QuotaSpan, at: &str) {
        span.resets_at = Some(QuotaTime::new(at));
    }

    #[test]
    fn route_text_names_the_serving_provider_and_member() {
        assert_eq!(
            route_line("demo", "provider", "member", true, Some(200)),
            "Route: demo → provider/member"
        );
        assert_eq!(
            route_line("demo", "", "", true, Some(404)),
            "Route: demo → not served (404)"
        );
        assert_eq!(
            route_line("demo", "", "", true, None),
            "Route: demo → not served (unknown status)"
        );
        assert_eq!(
            route_line("demo", "", "", false, None),
            "Route: demo → routing"
        );
        assert_eq!(
            route_line("demo", "provider", "member", false, None),
            "Route: demo → provider/member"
        );
    }

    #[test]
    fn today_text_counts_requests_and_compact_tokens() {
        assert_eq!(today_line(Totals::default()), "Today: no requests yet");
        assert_eq!(
            today_line(Totals {
                requests: 1,
                input_tokens: 12_000,
                output_tokens: 300,
                ..Totals::default()
            }),
            "Today: 1 request, 12.3k tokens"
        );
        assert_eq!(
            today_line(Totals {
                requests: 2,
                input_tokens: 4_000_000,
                output_tokens: 500_000,
                ..Totals::default()
            }),
            "Today: 2 requests, 4.5M tokens"
        );
    }

    #[test]
    fn token_counts_use_compact_suffixes() {
        for (tokens, text) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1.0k"),
            (12_300, "12.3k"),
            (999_999, "1000.0k"),
            (1_000_000, "1.0M"),
            (4_500_000, "4.5M"),
        ] {
            assert_eq!(compact_tokens(tokens), text);
        }
    }

    #[test]
    fn quota_error_uses_the_alias_and_user_label() {
        let quota = Quota {
            name: "Plan".to_owned(),
            alias: "Work".to_owned(),
            user: "user@example.test".to_owned(),
            error: "read failed".to_owned(),
            ..Quota::default()
        };
        assert_eq!(
            quota_line(&quota, now()).as_deref(),
            Some("Work (user@example.test): read failed")
        );
    }

    #[test]
    fn quota_shows_two_windows_and_the_soonest_reset() {
        let mut first = span(42.4);
        reset(&mut first, "2026-11-10T15:00:00Z");
        let mut second = span(17.5);
        reset(&mut second, "2026-11-10T14:05:00Z");
        let quota = Quota {
            name: "Plan".to_owned(),
            windows: vec![first, second],
            ..Quota::default()
        };
        assert_eq!(
            quota_line(&quota, now()).as_deref(),
            Some("Plan: 42% · 18% · resets in 2h 5m")
        );
    }

    #[test]
    fn quota_truncates_to_two_windows_in_report_order() {
        let mut first = span(1.0);
        reset(&mut first, "2026-11-10T15:00:00Z");
        let second = span(2.0);
        let mut third = span(3.0);
        reset(&mut third, "2026-11-10T12:07:00Z");
        let quota = Quota {
            name: "Plan".to_owned(),
            windows: vec![first, second, third],
            ..Quota::default()
        };
        assert_eq!(
            quota_line(&quota, now()).as_deref(),
            Some("Plan: 1% · 2% · resets in 3h 0m"),
            "the third window and its earlier reset are not shown"
        );
    }

    #[test]
    fn quota_clamps_percentages_and_ignores_uncounted_windows() {
        let mut text = span(50.0);
        text.text = true;
        let mut unlimited = span(50.0);
        unlimited.unlimited = true;
        let mut balance = span(50.0);
        balance.balance = Some(10.0);
        let quota = Quota {
            name: "Plan".to_owned(),
            windows: vec![text, unlimited, balance, span(-5.0), span(105.4)],
            ..Quota::default()
        };
        assert_eq!(
            quota_line(&quota, now()).as_deref(),
            Some("Plan: 0% · 100%")
        );
    }

    #[test]
    fn quota_falls_back_to_balance_or_nothing() {
        let quota = Quota {
            name: "Plan".to_owned(),
            balance: "12 credits".to_owned(),
            ..Quota::default()
        };
        assert_eq!(
            quota_line(&quota, now()).as_deref(),
            Some("Plan: 12 credits")
        );
        assert_eq!(quota_line(&Quota::default(), now()), None);
    }

    #[test]
    fn quota_uses_the_name_when_alias_is_empty() {
        let quota = Quota {
            name: "Plan".to_owned(),
            user: "user@example.test".to_owned(),
            windows: vec![span(7.0)],
            ..Quota::default()
        };
        assert_eq!(
            quota_line(&quota, now()).as_deref(),
            Some("Plan (user@example.test): 7%")
        );
    }

    #[test]
    fn quota_omits_bad_or_past_reset_times() {
        let mut bad = span(7.0);
        reset(&mut bad, "not a time");
        let mut past = span(8.0);
        reset(&mut past, "2026-11-10T11:00:00Z");
        let quota = Quota {
            name: "Plan".to_owned(),
            windows: vec![bad, past],
            ..Quota::default()
        };
        assert_eq!(quota_line(&quota, now()).as_deref(), Some("Plan: 7% · 8%"));
    }

    #[test]
    fn quota_omits_a_reset_that_is_due_now() {
        let mut due = span(9.0);
        reset(&mut due, "2026-11-10T12:00:00Z");
        let quota = Quota {
            name: "Plan".to_owned(),
            windows: vec![due],
            ..Quota::default()
        };
        assert_eq!(quota_line(&quota, now()).as_deref(), Some("Plan: 9%"));
    }

    #[test]
    fn waits_use_the_largest_two_units() {
        for (seconds, text) in [
            (3 * 86_400 + 4 * 3_600, "3d 4h"),
            (2 * 3_600 + 10 * 60, "2h 10m"),
            (7 * 60, "7m"),
            (1, "1m"),
            (89, "1m"),
            (90, "2m"),
        ] {
            assert_eq!(until(Duration::from_secs(seconds)), text);
        }
        assert_eq!(
            until(TimeDelta::seconds(-1).to_std().unwrap_or_default()),
            "1m"
        );
    }
}
