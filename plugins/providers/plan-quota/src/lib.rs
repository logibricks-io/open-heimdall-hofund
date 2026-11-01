//! Plan allowance readers for providers whose keys or accounts publish windows.

#![deny(missing_docs)]

mod command_code;
mod control_plans;
mod key_plans;
mod stepfun;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{HostContext, PlanCard, PlanQuery, PlanQuota, PlanSupport, PluginError};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::time::parse_rfc3339;
use hofund_plugin_sdk::{Call, Contribution};

/// Registers the plugin's data-only contribution.
#[must_use]
pub fn contribute() -> Contribution {
    Contribution::new()
}

/// Builds the stateful plan reader shared by the registry.
#[must_use]
pub fn reader() -> Arc<dyn PlanQuota> {
    Arc::new(Reader::default())
}

#[derive(Default)]
struct Reader {
    command_plans: Mutex<HashMap<String, command_code::CachedPlan>>,
    step_sessions: Mutex<HashMap<String, stepfun::Session>>,
}

#[derive(Clone, Debug)]
enum Source {
    Zhipu {
        url: String,
        root: String,
        certain: bool,
    },
    Kimi(String),
    OpenCode(String),
    CommandCode {
        credits: String,
        subscriptions: String,
    },
    MiniMax(String),
    Ollama(String),
    StepFun {
        platform: String,
        app: String,
    },
    Volcengine {
        endpoint: String,
        action: String,
        plan: String,
        region: String,
    },
    Tencent {
        endpoint: String,
        region: String,
    },
}

impl Source {
    const fn support(&self) -> PlanSupport {
        match self {
            Self::Zhipu { certain, .. } => {
                if *certain {
                    PlanSupport::Certain
                } else {
                    PlanSupport::Possible
                }
            }
            Self::Kimi(_) | Self::OpenCode(_) => PlanSupport::Certain,
            Self::CommandCode { .. } | Self::MiniMax(_) | Self::Ollama(_) => PlanSupport::Possible,
            Self::StepFun { .. } | Self::Volcengine { .. } | Self::Tencent { .. } => {
                PlanSupport::Certain
            }
        }
    }
}

#[hofund_plugin_sdk::async_trait]
impl PlanQuota for Reader {
    fn support(&self, query: &PlanQuery) -> PlanSupport {
        source(query).map_or(PlanSupport::Unsupported, |source| source.support())
    }

    async fn read(
        &self,
        query: &PlanQuery,
        host: &HostContext,
    ) -> Result<Vec<PlanCard>, PluginError> {
        let Some(source) = source(query) else {
            return Ok(Vec::new());
        };
        read_source(self, query, host, source).await
    }
}

async fn read_source(
    reader: &Reader,
    query: &PlanQuery,
    host: &HostContext,
    source: Source,
) -> Result<Vec<PlanCard>, PluginError> {
    match source {
        Source::Zhipu { url, root, .. } => key_plans::read_zhipu(query, host, &url, &root).await,
        Source::Kimi(url) => key_plans::read_kimi(query, host, &url).await,
        Source::OpenCode(url) => key_plans::read_open_code(query, host, &url).await,
        Source::CommandCode {
            credits,
            subscriptions,
        } => command_code::read(reader, query, host, &credits, &subscriptions).await,
        Source::MiniMax(url) => key_plans::read_mini_max(query, host, &url).await,
        Source::Ollama(url) => key_plans::read_ollama(query, host, &url).await,
        Source::StepFun { platform, app } => {
            stepfun::read(reader, query, host, &platform, &app).await
        }
        Source::Volcengine {
            endpoint,
            action,
            plan,
            region,
        } => control_plans::read_volcengine(query, host, &endpoint, &action, &plan, &region).await,
        Source::Tencent { endpoint, region } => {
            control_plans::read_tencent(query, host, &endpoint, &region).await
        }
    }
}

fn source(query: &PlanQuery) -> Option<Source> {
    for base in query.bases() {
        let url = hofund_plugin_sdk::url::Url::parse(base).ok()?;
        let host = url.host_str()?.to_ascii_lowercase();
        let path = url.path();
        let coding = path.contains("/api/coding/");
        match host.as_str() {
            "open.bigmodel.cn" | "api.z.ai" => {
                let default_root = format!("{}://{}", url.scheme(), host);
                let root = if query.plan_url().is_empty() {
                    default_root
                } else {
                    origin(query.plan_url())
                };
                let default_url = format!("{root}/api/monitor/usage/quota/limit");
                return Some(Source::Zhipu {
                    url: override_url(query, default_url),
                    root,
                    certain: coding,
                });
            }
            "api.kimi.com" | "api.kimi.ai" if path.contains("/coding") => {
                let root = if path.trim_end_matches('/').ends_with("/coding") {
                    format!("{}://{host}{}/v1", url.scheme(), path.trim_end_matches('/'))
                } else {
                    format!("{}://{host}{}", url.scheme(), path.trim_end_matches('/'))
                };
                return Some(Source::Kimi(override_url(query, format!("{root}/usages"))));
            }
            "opencode.ai" if open_code_go(path) => {
                return Some(Source::OpenCode(override_url(
                    query,
                    "https://opencode.ai/zen/go/v1/usage".to_owned(),
                )));
            }
            "api.commandcode.ai" => {
                let root = if query.plan_url().is_empty() {
                    format!("{}://{host}", url.scheme())
                } else {
                    origin(query.plan_url())
                };
                return Some(Source::CommandCode {
                    credits: override_url(query, format!("{root}/alpha/billing/credits")),
                    subscriptions: format!("{root}/alpha/billing/subscriptions"),
                });
            }
            "api.minimaxi.com" | "api.minimax.cn" | "api.minimax.io" => {
                return Some(Source::MiniMax(override_url(
                    query,
                    format!("{}://{host}/v1/token_plan/remains", url.scheme()),
                )));
            }
            "ollama.com" => {
                return Some(Source::Ollama(override_url(
                    query,
                    format!("{}://{host}/api/balance", url.scheme()),
                )));
            }
            "api.stepfun.ai" | "api.stepfun.com" => {
                let (platform, app) = if host.ends_with(".ai") {
                    ("https://platform.stepfun.ai", "20700")
                } else {
                    ("https://platform.stepfun.com", "10300")
                };
                let platform = if query.plan_url().is_empty() {
                    platform.to_owned()
                } else {
                    origin(query.plan_url())
                };
                return Some(Source::StepFun {
                    platform,
                    app: app.to_owned(),
                });
            }
            _ if host.starts_with("ark.") && host.ends_with(".volces.com") => {
                let (action, plan) = if path.starts_with("/api/coding/") {
                    ("GetCodingPlanUsage", "Coding Plan")
                } else if path.starts_with("/api/plan/") {
                    ("GetAFPUsage", "Agent Plan")
                } else {
                    continue;
                };
                let region = host
                    .split('.')
                    .nth(1)
                    .filter(|region| !region.is_empty())
                    .unwrap_or("cn-beijing")
                    .to_owned();
                return Some(Source::Volcengine {
                    endpoint: override_url(query, "https://open.volcengineapi.com/".to_owned()),
                    action: action.to_owned(),
                    plan: plan.to_owned(),
                    region,
                });
            }
            "tokenhub.tencentmaas.com" | "tokenhub-intl.tencentmaas.com"
                if path.starts_with("/plan/") =>
            {
                let region = if host.starts_with("tokenhub-intl") {
                    "ap-singapore"
                } else {
                    "ap-guangzhou"
                };
                return Some(Source::Tencent {
                    endpoint: override_url(
                        query,
                        "https://tokenhub.tencentcloudapi.com/".to_owned(),
                    ),
                    region: region.to_owned(),
                });
            }
            _ => {}
        }
    }
    None
}

fn override_url(query: &PlanQuery, default: String) -> String {
    if query.plan_url().is_empty() {
        default
    } else {
        query.plan_url().to_owned()
    }
}

fn origin(url: &str) -> String {
    hofund_plugin_sdk::url::Url::parse(url)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?.to_owned();
            let port = url
                .port()
                .map_or_else(String::new, |port| format!(":{port}"));
            Some(format!("{}://{host}{port}", url.scheme()))
        })
        .unwrap_or_else(|| url.trim_end_matches('/').to_owned())
}

fn open_code_go(path: &str) -> bool {
    path.contains("/zen/go") || path.starts_with("/inference/go/")
}

async fn fetch(host: &HostContext, call: Call) -> Result<(u16, Vec<u8>), PluginError> {
    let reply = call.fetch(host.http()).await?;
    if reply.body().len() > 1 << 20 {
        return Err(PluginError::decode("plan reply exceeds 1 MiB"));
    }
    Ok((reply.status(), reply.into_body()))
}

fn json_body(body: &[u8]) -> Result<Value, PluginError> {
    hofund_plugin_sdk::json::from_slice(body)
        .map_err(|error| PluginError::decode(format!("plan reply is not JSON: {error}")))
}

fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn epoch(value: f64) -> Option<SystemTime> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let seconds = if value > 1e11 { value / 1000.0 } else { value };
    if seconds > u64::MAX as f64 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs_f64(seconds))
}

fn time_value(value: Option<&Value>) -> Option<SystemTime> {
    let value = value?;
    if let Some(text) = value.as_str()
        && let Some(at) = parse_rfc3339(text)
    {
        return Some(at);
    }
    epoch(number(Some(value))?)
}

fn span_name(seconds: u64) -> String {
    const HOUR: u64 = 3600;
    const DAY: u64 = 24 * HOUR;
    match seconds {
        value if value == 30 * DAY => "Month".to_owned(),
        value if value > DAY && value % DAY == 0 => format!("{} days", value / DAY),
        value if value >= HOUR && value % HOUR == 0 => format!("{} hours", value / HOUR),
        value if value > 0 => format!("{} minutes", value / 60),
        _ => "Allowance".to_owned(),
    }
}

fn vendor_error(status: u16, body: &[u8]) -> PluginError {
    let text = vendor_said(status, body);
    if status == 401 || status == 403 {
        PluginError::auth(text)
    } else if status == 429 {
        PluginError::quota_status(status, text)
    } else {
        PluginError::status_error(status, text)
    }
}

fn vendor_said(status: u16, body: &[u8]) -> String {
    let status = status_text(status);
    let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(body) else {
        let text = String::from_utf8_lossy(body);
        let text = text.trim();
        return if text.is_empty() {
            format!("{status}: refused with no details")
        } else {
            format!("{status}: refused with non-JSON details")
        };
    };
    let message = value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("msg").and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if message.is_empty() {
        status
    } else {
        let cut = message.chars().take(200).collect::<String>();
        if message.chars().count() > 200 {
            format!("{status}: {cut}…")
        } else {
            format!("{status}: {message}")
        }
    }
}

fn status_text(status: u16) -> String {
    let reason = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => return format!("HTTP {status}"),
    };
    format!("{status} {reason}")
}

fn shape_of(body: &[u8]) -> String {
    let Ok(Value::Object(top)) = hofund_plugin_sdk::json::from_slice::<Value>(body) else {
        return if body.is_empty() {
            "an empty reply".to_owned()
        } else {
            "a reply that isn't a JSON object".to_owned()
        };
    };
    let mut names = Vec::new();
    for (key, value) in top {
        if let Value::Object(inner) = value
            && !inner.is_empty()
        {
            names.extend(inner.keys().map(|nested| format!("{key}.{nested}")));
        } else {
            names.push(key);
        }
    }
    names.sort();
    if names.is_empty() {
        return "an empty object".to_owned();
    }
    let shown = names
        .iter()
        .take(12)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > 12 {
        format!("fields {shown}, …")
    } else {
        format!("fields {shown}")
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex(&hofund_plugin_sdk::digest::sha256(bytes))
}

fn hmac(key: &[u8], text: &str) -> [u8; 32] {
    hofund_plugin_sdk::digest::hmac_sha256(key, text.as_bytes())
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn unix_seconds(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH).map_or_else(
        |error| -(error.duration().as_secs() as i64),
        |duration| duration.as_secs() as i64,
    )
}

fn utc_datetime(at: SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let seconds = unix_seconds(at);
    let days = seconds.div_euclid(86_400);
    let daytime = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (daytime / 3600) as u32,
        (daytime / 60 % 60) as u32,
        (daytime % 60) as u32,
    )
}

fn x_date(at: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = utc_datetime(at);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

fn utc_date(at: SystemTime) -> String {
    let (year, month, day, ..) = utc_datetime(at);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 }.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = month as i64;
    let mp = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn add_months(at: SystemTime, months: i64) -> Option<SystemTime> {
    let (year, month, day, hour, minute, second) = utc_datetime(at);
    let index = i64::from(month) - 1 + months;
    let year = year + index.div_euclid(12);
    let month = index.rem_euclid(12) + 1;
    let month_days =
        days_from_civil(year, month as u32 + 1, 1) - days_from_civil(year, month as u32, 1);
    let day = day.min(month_days as u32);
    let seconds =
        days_from_civil(year, month as u32, day) + i64::from(hour * 3600 + minute * 60 + second);
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

#[cfg(test)]
mod tests;
