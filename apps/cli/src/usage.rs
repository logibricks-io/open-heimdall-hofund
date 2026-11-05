//! `hofund usage` argument parsing and rendering helpers.

use hofund_control_client::hofund_control_proto::{
    UsageBreakdownBy, UsagePeriod, UsageQuery, UsageResponse,
};
use hofund_usage::{ReportWindow, parse_days, parse_last, parse_since};

/// Parsed `hofund usage` invocation.
pub(crate) enum UsageCommand {
    Report(UsageReportOptions),
}

/// Output format for a usage report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageOutputFormat {
    Table,
    Csv,
    Json,
}

/// Fully parsed report options for the control usage API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UsageReportOptions {
    pub window: ReportWindow,
    pub period: UsagePeriod,
    pub group_by: Option<UsageBreakdownBy>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub since_text: Option<String>,
    pub days_text: Option<String>,
    pub last_text: Option<String>,
    pub format: UsageOutputFormat,
}

pub(crate) const USAGE_HELP: &str = "usage: hofund usage [today|7d|30d|all] [--days <n>] [--last <dur>] [--since <time>] [--group-by agent|model|provider|provider-key|gateway-key|account|session|day] [--by <same>] [--account <name>] [--model <id>] [--csv] [--json] [--config PATH]

Period presets count from local midnight (default: last 30 days / 30d). Rolling --days and --last end at now.

--group-by dimensions: agent (client agent id), model (requested model), provider (upstream member), provider-key (API key fingerprint), gateway-key (caller key), account (subscription account), session (client session id), day (local calendar day).

--json emits UsageResponse with stable fields: period, window_label, currency, totals (requests, input_tokens, output_tokens, cache_read_tokens, cache_write_5m_tokens, cache_write_1h_tokens, reasoning_tokens, cost, unpriced, mean_ttft_ms, mean_tokens_per_second), rows (created_at_ms, model, provider, input_tokens, output_tokens, duration_ms, status, caller_key_id, caller_key_name, caller_issuer_id, cache_* , reasoning_*, priced, cost, cost_usd, outcome, client_agent, ttft_ms, tokens_per_second, account, session_id, provider_key_id, served_model), rows_truncated, breakdown_by, breakdown (id, label, requests, input_tokens, output_tokens, cache_*, reasoning_tokens, cost, unpriced, mean_ttft_ms, mean_tokens_per_second).";

/// Parses flags after the `usage` verb.
///
/// # Errors
///
/// Returns a usage message when a flag is unknown or malformed.
pub(crate) fn parse(args: &[&str]) -> Result<UsageCommand, String> {
    let mut format = UsageOutputFormat::Table;
    let mut positional_period = None;
    let mut since = None::<String>;
    let mut days = None::<String>;
    let mut last = None::<String>;
    let mut group_by = None;
    let mut account = None;
    let mut model = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match *arg {
            "--csv" => format = UsageOutputFormat::Csv,
            "--json" => format = UsageOutputFormat::Json,
            "--by" | "--group-by" | "--group_by" => {
                let Some(value) = iter.next() else {
                    return Err(format!("{USAGE_HELP}\n{arg} needs a dimension"));
                };
                group_by = Some(parse_group_by(value)?);
            }
            "--days" => {
                let Some(value) = iter.next() else {
                    return Err(format!("{USAGE_HELP}\n--days needs a number of days"));
                };
                days = Some((*value).to_owned());
            }
            "--last" => {
                let Some(value) = iter.next() else {
                    return Err(format!("{USAGE_HELP}\n--last needs a duration"));
                };
                last = Some((*value).to_owned());
            }
            "--since" => {
                let Some(value) = iter.next() else {
                    return Err(format!("{USAGE_HELP}\n--since needs a timestamp or date"));
                };
                since = Some((*value).to_owned());
            }
            "--account" => {
                let Some(value) = iter.next() else {
                    return Err(format!(
                        "{USAGE_HELP}\n--account needs a subscription account name"
                    ));
                };
                account = Some((*value).to_owned());
            }
            "--model" | "-m" => {
                let Some(value) = iter.next() else {
                    return Err(format!("{USAGE_HELP}\n--model needs a model id"));
                };
                model = Some((*value).to_owned());
            }
            value if !value.starts_with('-') => {
                if positional_period.is_some() {
                    return Err(format!("{USAGE_HELP}\nunexpected argument {value:?}"));
                }
                positional_period = Some(parse_positional_period(value)?);
            }
            other => return Err(format!("{USAGE_HELP}\nunknown flag {other:?}")),
        }
    }
    if days.is_some() && last.is_some() {
        return Err(format!("{USAGE_HELP}\ntake --days or --last, not both"));
    }
    if positional_period.is_some() && (days.is_some() || last.is_some() || since.is_some()) {
        return Err(format!(
            "{USAGE_HELP}\ntake --days/--last/--since or a period preset, not both"
        ));
    }
    let (window, period) = if let Some(ref value) = days {
        let duration = parse_days(value)?;
        (ReportWindow::Rolling(duration), UsagePeriod::All)
    } else if let Some(ref value) = last {
        let duration = parse_last(value)?;
        (ReportWindow::Rolling(duration), UsagePeriod::All)
    } else if let Some(ref value) = since {
        let unix = parse_since(value)?;
        (ReportWindow::Since(unix), UsagePeriod::All)
    } else {
        let period = positional_period.unwrap_or(UsagePeriod::Month);
        let window = match period {
            UsagePeriod::Day => ReportWindow::Today,
            UsagePeriod::Week => ReportWindow::Week,
            UsagePeriod::Month => ReportWindow::Month,
            UsagePeriod::All => ReportWindow::All,
            UsagePeriod::Unknown => {
                return Err(format!("{USAGE_HELP}\nunknown period preset"));
            }
            _ => {
                return Err(format!("{USAGE_HELP}\nunknown period preset"));
            }
        };
        (window, period)
    };
    Ok(UsageCommand::Report(UsageReportOptions {
        window,
        period,
        group_by,
        account,
        model,
        since_text: since,
        days_text: days,
        last_text: last,
        format,
    }))
}

pub(crate) fn options_to_query(options: &UsageReportOptions) -> UsageQuery {
    let mut query = UsageQuery::new(options.period);
    query.by = options.group_by;
    query.account = options.account.clone();
    query.model = options.model.clone();
    query.since = options.since_text.clone();
    query.days = options.days_text.clone();
    query.last = options.last_text.clone();
    query.unlimited = matches!(
        options.format,
        UsageOutputFormat::Csv | UsageOutputFormat::Json
    );
    query
}

fn parse_positional_period(value: &str) -> Result<UsagePeriod, String> {
    match value.to_ascii_lowercase().as_str() {
        "today" | "day" => Ok(UsagePeriod::Day),
        "7d" | "week" => Ok(UsagePeriod::Week),
        "30d" | "month" => Ok(UsagePeriod::Month),
        "all" => Ok(UsagePeriod::All),
        other => Err(format!("{USAGE_HELP}\nunknown period {other:?}")),
    }
}

fn parse_group_by(value: &str) -> Result<UsageBreakdownBy, String> {
    UsageBreakdownBy::parse(value).ok_or_else(|| {
        format!("{USAGE_HELP}\nunknown breakdown {value:?}; expected agent, model, provider, provider-key, gateway-key, account, session, or day")
    })
}

/// Stable JSON field names for `hofund usage --json`.
pub(crate) fn usage_json(response: &UsageResponse) -> Result<String, String> {
    serde_json::to_string(response).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{UsageOutputFormat, UsagePeriod, options_to_query, parse, parse_group_by};
    use hofund_control_client::hofund_control_proto::UsageBreakdownBy;

    #[test]
    fn u1c_13_cli_usage_parses_by_flag() {
        let parsed = parse(&["--by", "provider-key"]).expect("parse");
        match parsed {
            super::UsageCommand::Report(options) => {
                assert_eq!(options.group_by, Some(UsageBreakdownBy::ProviderKey));
            }
        }
        assert!(parse(&["--by"]).is_err());
        assert!(parse(&["--by", "nope"]).is_err());
        assert_eq!(
            parse_group_by("gateway-key").expect("gateway-key"),
            UsageBreakdownBy::GatewayKey
        );
    }

    #[test]
    fn r10_usage_parses_periods_conflicts_and_filters() {
        let week = parse(&["7d"]).expect("7d");
        match week {
            super::UsageCommand::Report(options) => {
                assert_eq!(options.period, UsagePeriod::Week);
            }
        }
        assert!(parse(&["--days", "7", "today"]).is_err());
        assert!(parse(&["--days", "1", "--last", "1h"]).is_err());
        let filtered = parse(&["--model", "demo", "--account", "me@x.test"]).expect("filters");
        match filtered {
            super::UsageCommand::Report(options) => {
                assert_eq!(options.model.as_deref(), Some("demo"));
                assert_eq!(options.account.as_deref(), Some("me@x.test"));
            }
        }
        let csv = parse(&["--csv", "today"]).expect("csv");
        match csv {
            super::UsageCommand::Report(options) => {
                assert_eq!(options.format, UsageOutputFormat::Csv)
            }
        }
    }

    #[test]
    fn r10_usage_default_period_is_month() {
        let super::UsageCommand::Report(options) = parse(&[]).expect("default");
        assert_eq!(options.period, UsagePeriod::Month);
        let query = options_to_query(&options);
        assert_eq!(query.period, UsagePeriod::Month);
    }

    #[test]
    fn r10_usage_group_by_aliases_and_since() {
        let super::UsageCommand::Report(options) =
            parse(&["--group-by", "day", "--since", "2026-03-01"]).expect("parse");
        assert_eq!(options.group_by, Some(UsageBreakdownBy::Day));
        let query = options_to_query(&options);
        assert_eq!(query.since.as_deref(), Some("2026-03-01"));
        assert_eq!(query.by, Some(UsageBreakdownBy::Day));
    }
}
