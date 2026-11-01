use std::time::{Duration, SystemTime};

use hofund_plugin_api::{
    CredentialStyle, HostContext, PlanCard, PlanQuery, PlanWindow, PluginError, SecretString,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::new_state;

use crate::{
    fetch, finite, json_body, number, shape_of, span_name, time_value, vendor_error, vendor_said,
};

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

pub(crate) async fn read_zhipu(
    query: &PlanQuery,
    host: &HostContext,
    url: &str,
    root: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    let personal = zhipu_call(query, host, url, false).await;
    let mut failure = None;
    let (mut plan, mut windows) = match personal {
        Ok(reading) => reading,
        Err(error) if error.kind() == hofund_plugin_api::PluginErrorKind::Auth => {
            return Err(error);
        }
        Err(error) => {
            failure = Some(error);
            (String::new(), Vec::new())
        }
    };
    if windows.is_empty() {
        let mut roots = vec![root.to_owned()];
        if root.ends_with("open.bigmodel.cn") {
            roots.push("https://bigmodel.cn".to_owned());
        }
        for team_root in roots {
            let team_url = format!("{team_root}/api/monitor/usage/quota/limit?type=2");
            if let Ok((team_plan, team_windows)) = zhipu_call(query, host, &team_url, true).await
                && !team_windows.is_empty()
            {
                plan = team_plan;
                windows = team_windows;
                break;
            }
        }
        if windows.is_empty() {
            if let Some(error) = failure {
                return Err(error);
            }
            return Ok(vec![PlanCard::new(plan)]);
        }
        return Ok(vec![PlanCard::new(plan).with_windows(windows)]);
    }
    let mut card = PlanCard::new(plan).with_windows(windows);
    let term_url = format!("{root}/api/biz/subscription/list");
    if let Ok(reply) = fetch(host, zhipu_request(query, &term_url, false)?).await
        && reply.0 < 300
        && let Some((until, renew)) = zhipu_term(&reply.1)
    {
        card = card.with_term(until, renew);
    }
    Ok(vec![card])
}

fn zhipu_request(query: &PlanQuery, url: &str, team: bool) -> Result<Call, PluginError> {
    let mut call = Call::get(url)
        .sensitive_header("authorization", query.api_key())
        .header("accept", "application/json")
        .header("accept-language", "en-US,en");
    if team && !query.organization().is_empty() && !query.project().is_empty() {
        call = call
            .header("bigmodel-organization", query.organization())
            .header("bigmodel-project", query.project())
            .header("set-language", "en");
    }
    Ok(call)
}

async fn zhipu_call(
    query: &PlanQuery,
    host: &HostContext,
    url: &str,
    team: bool,
) -> Result<(String, Vec<PlanWindow>), PluginError> {
    let (status, body) = fetch(host, zhipu_request(query, url, team)?).await?;
    if status == 401 {
        let vendor = if url.contains("api.z.ai") {
            "Z.ai"
        } else {
            "Zhipu"
        };
        let keys = if vendor == "Z.ai" {
            "https://z.ai/manage-apikey/apikey-list"
        } else {
            "https://open.bigmodel.cn/usercenter/proj-mgmt/apikeys"
        };
        return Err(PluginError::auth(format!(
            "{vendor} didn't take this key ({}): it has expired, was deleted, or is mistyped; paste a current one from {keys} in the provider's settings",
            vendor_said(status, &body)
        )));
    }
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    read_zhipu_body(&body, team)
}

fn read_zhipu_body(body: &[u8], team: bool) -> Result<(String, Vec<PlanWindow>), PluginError> {
    let value = json_body(body)?;
    let code = value.get("code").and_then(Value::as_i64);
    let message = value.get("msg").and_then(Value::as_str).unwrap_or("");
    if code.is_some_and(|code| (1000..=1005).contains(&code)) {
        return Err(PluginError::auth(message));
    }
    if code.is_some_and(|code| code != 1113 && (1300..=1399).contains(&code)) {
        return Err(PluginError::quota_status(
            429,
            format!("429 rate limited ({}): {message}", code.unwrap_or(0)),
        ));
    }
    if value
        .get("success")
        .and_then(Value::as_bool)
        .is_some_and(|success| !success)
        || value.get("data").is_none()
    {
        return Err(PluginError::decode(if message.is_empty() {
            "no plan in the reply"
        } else {
            message
        }));
    }
    let data = &value["data"];
    let mut windows = Vec::new();
    for limit in data
        .get("limits")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let used = finite(number(limit.get("percentage")).unwrap_or(0.0)).unwrap_or(0.0);
        let mut window = PlanWindow::new("Allowance", used)
            .with_optional_reset(time_value(limit.get("nextResetTime")));
        let kind = limit.get("type").and_then(Value::as_str).unwrap_or("");
        let unit = limit.get("unit").and_then(Value::as_i64).unwrap_or(0);
        let number = limit.get("number").and_then(Value::as_i64).unwrap_or(0);
        if kind.eq_ignore_ascii_case("TIME_LIMIT") {
            window = PlanWindow::new("MCP · Month", used)
                .with_blocking(false)
                .with_optional_reset(time_value(limit.get("nextResetTime")));
        } else if unit == 3 {
            let hours = if number <= 0 { 5 } else { number.max(1) as u64 };
            window = PlanWindow::new(format!("{hours} hours"), used)
                .with_span(Duration::from_secs(hours * HOUR))
                .with_optional_reset(time_value(limit.get("nextResetTime")));
        } else if unit == 6 {
            window = PlanWindow::new("7 days", used)
                .with_span(Duration::from_secs(7 * DAY))
                .with_optional_reset(time_value(limit.get("nextResetTime")));
        }
        windows.push(window);
    }
    let plan = if team {
        "GLM Coding Team".to_owned()
    } else {
        data.get("level")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    Ok((plan, windows))
}

fn zhipu_term(body: &[u8]) -> Option<(SystemTime, String)> {
    let rows = json_body(body).ok()?;
    let data = rows.get("data")?.as_array()?;
    for row in data {
        if !row
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status.eq_ignore_ascii_case("VALID"))
        {
            continue;
        }
        let auto = row
            .get("autoRenew")
            .is_some_and(|value| value.as_bool() == Some(true) || value.as_i64() == Some(1));
        if let Some(until) = zhipu_time(row.get("nextRenewTime").and_then(Value::as_str)) {
            return Some((until, if auto { "auto" } else { "off" }.to_owned()));
        }
        if !auto && let Some(valid) = row.get("valid").and_then(Value::as_str) {
            let dates = valid.match_indices(|character: char| character.is_ascii_digit());
            let mut found = None;
            for (start, _) in dates {
                let candidate = valid
                    .get(start..)
                    .and_then(|rest| rest.get(..19.min(rest.len())));
                if let Some(at) = candidate.and_then(|text| zhipu_time(Some(text))) {
                    found = Some(at);
                }
            }
            if let Some(until) = found {
                return Some((until, "off".to_owned()));
            }
        }
    }
    None
}

fn zhipu_time(text: Option<&str>) -> Option<SystemTime> {
    let text = text?.trim();
    for candidate in [text.to_owned(), text.replace(' ', "T")] {
        for suffix in ["+08:00", "Z"] {
            if let Some(at) =
                hofund_plugin_sdk::time::parse_rfc3339(&format!("{candidate}{suffix}"))
            {
                return Some(at);
            }
        }
    }
    None
}

pub(crate) async fn read_kimi(
    query: &PlanQuery,
    host: &HostContext,
    url: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    let secret = SecretString::from(query.api_key().to_owned());
    let call = Call::get(url)
        .credential(CredentialStyle::Bearer, &secret)?
        .header("accept", "application/json")
        .header("accept-language", "en-US,en");
    let (status, body) = fetch(host, call).await?;
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    let windows = read_kimi_body(&body)?;
    Ok(vec![PlanCard::new("").with_windows(windows)])
}

fn read_kimi_body(body: &[u8]) -> Result<Vec<PlanWindow>, PluginError> {
    let value = json_body(body)?;
    let mut out = Vec::new();
    for row in value
        .get("limits")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let window = row.get("window").cloned().unwrap_or_else(|| json!({}));
        let detail = row.get("detail").unwrap_or(&window);
        let duration = number(window.get("duration")).unwrap_or(0.0).max(0.0) as u64;
        let unit = window.get("timeUnit").and_then(Value::as_str).unwrap_or("");
        let seconds = if unit.contains("MINUTE") {
            duration.saturating_mul(60)
        } else if unit.contains("HOUR") {
            duration.saturating_mul(HOUR)
        } else if unit.contains("DAY") {
            duration.saturating_mul(DAY)
        } else if unit.contains("WEEK") {
            duration.saturating_mul(7 * DAY)
        } else if unit.contains("MONTH") {
            duration.saturating_mul(30 * DAY)
        } else {
            0
        };
        if let Some(window) = counted_window(detail, &span_name(seconds), seconds) {
            out.push(window);
        }
    }
    if let Some(usage) = value.get("usage")
        && let Some(window) = counted_window(usage, "7 days", 7 * DAY)
    {
        out.push(window);
    }
    let mut shares = Vec::new();
    if let Some(usages) = value.get("usages").and_then(Value::as_object) {
        for (snake, camel, name, seconds) in [
            ("limit_5h", "limit5h", "5 hours", 5 * HOUR),
            ("limit_7d", "limit7d", "7 days", 7 * DAY),
            ("limit_month_total", "limitMonthTotal", "Month", 30 * DAY),
            ("limit_month_code", "limitMonthCode", "Month", 30 * DAY),
        ] {
            let Some(row) = usages.get(snake).or_else(|| usages.get(camel)) else {
                continue;
            };
            let Some(ratio) =
                number(row.get("used_ratio").or_else(|| row.get("usedRatio"))).and_then(finite)
            else {
                continue;
            };
            if shares
                .iter()
                .any(|window: &PlanWindow| window.span() == Some(Duration::from_secs(seconds)))
            {
                continue;
            }
            shares.push(
                PlanWindow::new(name, (ratio * 100.0).clamp(0.0, 100.0))
                    .with_span(Duration::from_secs(seconds))
                    .with_optional_reset(reset_of(row)),
            );
        }
    }
    if !shares.is_empty() {
        out.retain(|window| {
            !shares
                .iter()
                .any(|share| share.span().is_some() && share.span() == window.span())
        });
        out.extend(shares);
        out.sort_by_key(|window| window.span().unwrap_or(Duration::MAX));
    }
    if out.is_empty() {
        return Err(PluginError::decode("no usage in the reply"));
    }
    Ok(out)
}

fn reset_of(row: &Value) -> Option<SystemTime> {
    ["resetTime", "resetAt", "reset_at", "reset_time"]
        .iter()
        .find_map(|key| time_value(row.get(*key)))
}

fn counted_window(row: &Value, name: &str, seconds: u64) -> Option<PlanWindow> {
    let limit = number(row.get("limit")).and_then(finite)?;
    if limit <= 0.0 {
        return None;
    }
    let used = number(row.get("used"))
        .or_else(|| number(row.get("remaining")).map(|remaining| limit - remaining))?;
    Some(
        PlanWindow::new(name, (used / limit * 100.0).clamp(0.0, 100.0))
            .with_span(Duration::from_secs(seconds))
            .with_optional_reset(reset_of(row)),
    )
}

pub(crate) async fn read_open_code(
    query: &PlanQuery,
    host: &HostContext,
    url: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    let secret = SecretString::from(query.api_key().to_owned());
    let session = new_state().map_err(|error| PluginError::internal(error.to_string()))?;
    let call = Call::get(url)
        .credential(CredentialStyle::Bearer, &secret)?
        .header("accept", "application/json")
        .header("accept-language", "en-US,en")
        .header("x-opencode-session", session);
    let (status, body) = fetch(host, call).await?;
    match status {
        403 => {
            return Err(PluginError::auth(
                "this key has no OpenCode Go subscription: Go belongs to the member who subscribed, in that workspace, so use a key that member made there (opencode.ai, the workspace's API Keys)",
            ));
        }
        401 => {
            return Err(PluginError::auth(format!(
                "OpenCode didn't take this key ({}): it was deleted, or is mistyped",
                vendor_said(status, &body)
            )));
        }
        300.. => return Err(vendor_error(status, &body)),
        _ => {}
    }
    let value = json_body(&body)?;
    let usage = value.get("usage").and_then(Value::as_object);
    let mut windows = Vec::new();
    for (key, name, seconds) in [
        ("rolling", "5 hours", 5 * HOUR),
        ("weekly", "7 days", 7 * DAY),
        ("monthly", "Month", 0),
    ] {
        let Some(row) = usage.and_then(|usage| usage.get(key)) else {
            continue;
        };
        let Some(percent) = number(row.get("percent")).and_then(finite) else {
            continue;
        };
        let mut window = PlanWindow::new(name, percent);
        if seconds > 0 {
            window = window.with_span(Duration::from_secs(seconds));
        }
        if percent > 0.0 {
            window = window.with_optional_reset(time_value(row.get("resetsAt")));
        }
        windows.push(window);
    }
    if windows.is_empty() {
        return Err(PluginError::decode(format!(
            "OpenCode Go's reply has no rolling, weekly or monthly window: {}",
            shape_of(&body)
        )));
    }
    Ok(vec![PlanCard::new("").with_windows(windows)])
}

pub(crate) async fn read_mini_max(
    query: &PlanQuery,
    host: &HostContext,
    url: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    let secret = SecretString::from(query.api_key().to_owned());
    let call = Call::get(url)
        .credential(CredentialStyle::Bearer, &secret)?
        .header("accept", "application/json")
        .header("accept-language", "en-US,en");
    let (status, body) = fetch(host, call).await?;
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    let windows = read_mini_max_body(&body)?;
    Ok(vec![PlanCard::new("").with_windows(windows)])
}

fn read_mini_max_body(body: &[u8]) -> Result<Vec<PlanWindow>, PluginError> {
    let value = json_body(body)?;
    let base = value
        .get("base_resp")
        .ok_or_else(|| PluginError::decode("no plan in the reply"))?;
    let code = base.get("status_code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let message = base
            .get("status_msg")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("MiniMax said {code}"));
        return Err(PluginError::auth(message));
    }
    let mut out = Vec::new();
    for bucket in value
        .get("model_remains")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let name = bucket
            .get("model_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let zero = |field| number(bucket.get(field)) == Some(0.0);
        let interval_status = bucket
            .get("current_interval_status")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let weekly_status = bucket
            .get("current_weekly_status")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if name.is_empty()
            || interval_status == 3
                && weekly_status == 3
                && zero("current_interval_total_count")
                && zero("current_weekly_total_count")
        {
            continue;
        }
        let general = name.eq_ignore_ascii_case("general");
        for weekly in [false, true] {
            let (left_field, status, start_field, end_field, total_field, count_field) = if weekly {
                (
                    "current_weekly_remaining_percent",
                    weekly_status,
                    "weekly_start_time",
                    "weekly_end_time",
                    "current_weekly_total_count",
                    "current_weekly_usage_count",
                )
            } else {
                (
                    "current_interval_remaining_percent",
                    interval_status,
                    "start_time",
                    "end_time",
                    "current_interval_total_count",
                    "current_interval_usage_count",
                )
            };
            let left = number(bucket.get(left_field));
            if status == 3 || left.is_none() && status != 2 {
                continue;
            }
            let used = if status == 2 {
                100.0
            } else {
                (100.0 - left.unwrap_or(100.0)).clamp(0.0, 100.0)
            };
            let start = time_value(bucket.get(start_field));
            let end = time_value(bucket.get(end_field));
            let seconds = match (start, end) {
                (Some(start), Some(end)) if end > start => {
                    end.duration_since(start).unwrap_or_default().as_secs()
                }
                _ if weekly => 7 * DAY,
                _ => 0,
            };
            let window_name = if weekly {
                "7 days".to_owned()
            } else {
                span_name(seconds)
            };
            let window_name = if general {
                window_name
            } else {
                let mut chars = name.chars();
                let first = chars.next().unwrap_or_default().to_uppercase();
                format!("{first}{} · {window_name}", chars.as_str())
            };
            let mut window = PlanWindow::new(window_name, used)
                .with_blocking(general)
                .with_optional_reset(end);
            if seconds > 0 {
                window = window.with_span(Duration::from_secs(seconds));
            }
            if let Some((amount, limit)) = mini_max_count(
                number(bucket.get(count_field)),
                number(bucket.get(total_field)),
                left,
            ) {
                let amount = if status == 2 { limit } else { amount };
                let unit = if name.eq_ignore_ascii_case("video") {
                    "videos"
                } else {
                    "items"
                };
                window = window.with_count(amount, limit, unit);
            }
            out.push(window);
        }
    }
    Ok(out)
}

fn mini_max_count(count: Option<f64>, total: Option<f64>, left: Option<f64>) -> Option<(f64, f64)> {
    let (count, total, left) = (count?, total?, left?);
    if total <= 0.0 || count < 0.0 || count > total {
        return None;
    }
    let as_used = ((total - count) / total * 100.0 - left).abs();
    let as_left = (count / total * 100.0 - left).abs();
    if as_used.min(as_left) > 1.0 {
        return None;
    }
    if as_used < as_left {
        Some((count, total))
    } else {
        Some((total - count, total))
    }
}

pub(crate) async fn read_ollama(
    query: &PlanQuery,
    host: &HostContext,
    url: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    let secret = SecretString::from(query.api_key().to_owned());
    let call = Call::get(url)
        .credential(CredentialStyle::Bearer, &secret)?
        .header("accept", "application/json")
        .header("accept-language", "en-US,en");
    let (status, body) = fetch(host, call).await?;
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    let value = json_body(&body)?;
    let Some(included) = value.get("included") else {
        return Ok(Vec::new());
    };
    let allowance = number(included.get("allowance_usd"));
    let left = number(included.get("balance_usd"));
    let (Some(allowance), Some(left)) = (allowance, left) else {
        return Ok(Vec::new());
    };
    if allowance <= 0.0 {
        return Ok(Vec::new());
    }
    let used = (allowance - left).clamp(0.0, allowance);
    let mut window = PlanWindow::new("Allowance", (used / allowance * 100.0).clamp(0.0, 100.0))
        .with_count(used, allowance, "USD");
    if let Some(period) = included.get("period") {
        let from = time_value(period.get("from"));
        let until = time_value(period.get("until"));
        if let (Some(from), Some(until)) = (from, until)
            && until > from
        {
            let span = until.duration_since(from).unwrap_or_default();
            if (27..=32).contains(&(span.as_secs() / DAY)) {
                window = PlanWindow::new("Month", window.used()).with_count(used, allowance, "USD");
            }
            window = window.with_span(span).with_resets_at(until);
        } else {
            window = window.with_optional_reset(until);
        }
    }
    let bought = value
        .get("purchased")
        .and_then(|purchased| number(purchased.get("balance_usd")))
        .unwrap_or(0.0);
    if bought > 0.0 {
        window = window.with_blocking(false);
    }
    Ok(vec![PlanCard::new("").with_windows(vec![window])])
}

#[cfg(test)]
pub(crate) fn parse_zhipu(body: &[u8]) -> Result<(String, Vec<PlanWindow>), PluginError> {
    read_zhipu_body(body, false)
}

#[cfg(test)]
pub(crate) fn parse_kimi(body: &[u8]) -> Result<Vec<PlanWindow>, PluginError> {
    read_kimi_body(body)
}

#[cfg(test)]
pub(crate) fn parse_mini_max(body: &[u8]) -> Result<Vec<PlanWindow>, PluginError> {
    read_mini_max_body(body)
}

#[cfg(test)]
pub(crate) fn parse_zhipu_term(body: &[u8]) -> Option<(SystemTime, String)> {
    zhipu_term(body)
}
