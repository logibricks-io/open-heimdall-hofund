use std::time::{Duration, SystemTime};

use hofund_plugin_api::{HostContext, PlanCard, PlanQuery, PlanWindow, PluginError};
use hofund_plugin_sdk::json::{Value, json};

use crate::{
    add_months, fetch, hex, hmac, json_body, number, percent_encode, sha256, shape_of, time_value,
    utc_date, unix_seconds, vendor_error, x_date,
};

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;
const VOLC_HOST: &str = "open.volcengineapi.com";
const TENCENT_HOST: &str = "tokenhub.tencentcloudapi.com";

pub(crate) async fn read_volcengine(
    query: &PlanQuery,
    host: &HostContext,
    endpoint: &str,
    action: &str,
    plan: &str,
    region: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    if query.access_key_id().is_empty() || query.secret_access_key().is_empty() {
        return Err(PluginError::auth(format!(
            "this {plan} needs the account's AccessKey ID and Secret in the provider's options (access_key_id and secret_access_key)"
        )));
    }
    let body = volc_call(query, host, endpoint, action, region).await?;
    let value = json_body(&body)?;
    let result = value.get("Result").cloned().unwrap_or(Value::Null);
    let (mut windows, tier) = if action == "GetAFPUsage" {
        read_afp(&result)
    } else {
        (read_coding(&result), String::new())
    };
    let plan = if tier.is_empty() {
        plan.to_owned()
    } else {
        format!("{plan} {tier}")
    };
    if windows.is_empty() {
        return Err(PluginError::decode(format!(
            "this Volcengine account has no {plan}, or Ark answered {action} in a form the plan reader doesn't know: {}",
            shape_of(&body)
        )));
    }
    Ok(vec![PlanCard::new(plan).with_windows(std::mem::take(&mut windows))])
}

async fn volc_call(
    query: &PlanQuery,
    host: &HostContext,
    endpoint: &str,
    action: &str,
    region: &str,
) -> Result<Vec<u8>, PluginError> {
    let payload = b"{}";
    let query_text = format!(
        "Action={}&Version=2024-01-01",
        percent_encode(action)
    );
    let content_sha = sha256(payload);
    let x_date = x_date(query.now());
    let date = &x_date[..8];
    let signed = "content-type;host;x-content-sha256;x-date";
    let canonical = format!(
        "POST\n/\n{query_text}\ncontent-type:application/json\nhost:{VOLC_HOST}\nx-content-sha256:{content_sha}\nx-date:{x_date}\n\n{signed}\n{content_sha}"
    );
    let scope = format!("{date}/{region}/ark/request");
    let to_sign = format!(
        "HMAC-SHA256\n{x_date}\n{scope}\n{}",
        sha256(canonical.as_bytes())
    );
    let mut key = hmac(query.secret_access_key().as_bytes(), date).to_vec();
    for part in [region, "ark", "request"] {
        key = hmac(&key, part).to_vec();
    }
    let signature = hex(&hmac(&key, &to_sign));
    let authorization = format!(
        "HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        query.access_key_id()
    );
    let url = format!("{}?{query_text}", endpoint.trim_end_matches('/'));
    let call = hofund_plugin_sdk::Call::post(url)
        .header("content-type", "application/json")
        .header("x-date", x_date)
        .header("x-content-sha256", content_sha)
        .sensitive_header("authorization", authorization)
        .header("accept", "application/json")
        .json(payload);
    let (status, body) = fetch(host, call).await?;
    let value = hofund_plugin_sdk::json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    if let Some(error) = value.pointer("/ResponseMetadata/Error")
        && (error.get("Code").and_then(Value::as_str).is_some_and(|code| !code.is_empty())
            || error.get("Message").and_then(Value::as_str).is_some_and(|message| !message.is_empty()))
    {
        let code = error.get("Code").and_then(Value::as_str).unwrap_or("");
        let message = error.get("Message").and_then(Value::as_str).unwrap_or("");
        let hint = if volc_refuses_key(code) || matches!(status, 401 | 403) {
            "; check the AccessKey ID and Secret in the provider's editor, and that the access key may query Ark (IAM)"
        } else {
            ""
        };
        return Err(if matches!(status, 401 | 403) {
            PluginError::auth(format!("Volcengine {action}: {code} {message}{hint}"))
        } else {
            PluginError::status_error(status, format!("Volcengine {action}: {code} {message}{hint}"))
        });
    }
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    Ok(body)
}

fn volc_refuses_key(code: &str) -> bool {
    let code = code.to_ascii_lowercase();
    [
        "accesskey",
        "signature",
        "accessdenied",
        "unauthorized",
        "forbidden",
        "credential",
        "authfailure",
    ]
    .iter()
    .any(|needle| code.contains(needle))
}

fn read_coding(result: &Value) -> Vec<PlanWindow> {
    let mut out = Vec::new();
    for row in result
        .get("QuotaUsage")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let Some((name, seconds)) = volc_window(row.get("Level").and_then(Value::as_str)) else {
            continue;
        };
        let Some(percent) = number(row.get("Percent")) else {
            continue;
        };
        out.push(
            PlanWindow::new(name, percent)
                .with_span(Duration::from_secs(seconds))
                .with_optional_reset(time_value(row.get("ResetTimestamp"))),
        );
    }
    out
}

fn read_afp(result: &Value) -> (Vec<PlanWindow>, String) {
    let mut out = Vec::new();
    for (field, level) in [("AFPFiveHour", "session"), ("AFPWeekly", "weekly"), ("AFPMonthly", "monthly")] {
        let Some(bucket) = result.get(field) else {
            continue;
        };
        let Some((name, seconds)) = volc_window(Some(level)) else {
            continue;
        };
        let mut window = if let Some(percent) = number(bucket.get("Percent")) {
            PlanWindow::new(name, percent).with_span(Duration::from_secs(seconds))
        } else {
            let Some(used) = number(bucket.get("Used")) else {
                continue;
            };
            let Some(limit) = number(bucket.get("Quota")).or_else(|| number(bucket.get("Total"))) else {
                continue;
            };
            if limit <= 0.0 {
                continue;
            }
            PlanWindow::new(name, used / limit * 100.0)
                .with_span(Duration::from_secs(seconds))
                .with_count(used, limit, "credits")
        };
        window = window.with_optional_reset(
            time_value(bucket.get("ResetTime")).or_else(|| time_value(bucket.get("ResetTimestamp"))),
        );
        out.push(window);
    }
    let tier = result
        .get("Tier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    (out, tier)
}

fn volc_window(level: Option<&str>) -> Option<(&'static str, u64)> {
    match level?.trim().to_ascii_lowercase().as_str() {
        "session" => Some(("5 hours", 5 * HOUR)),
        "weekly" => Some(("Weekly", 7 * DAY)),
        "monthly" => Some(("Month", 30 * DAY)),
        _ => None,
    }
}

pub(crate) async fn read_tencent(
    query: &PlanQuery,
    host: &HostContext,
    endpoint: &str,
    region: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    if query.access_key_id().is_empty() || query.secret_access_key().is_empty() {
        return Err(PluginError::auth(
            "this Token Plan Enterprise needs the account's SecretId and SecretKey in the provider's options (access_key_id and secret_access_key)",
        ));
    }
    let body = tencent_call(
        query,
        host,
        endpoint,
        region,
        "DescribeTokenPlanList",
        json!({"Offset": 0, "Limit": 100}),
    )
    .await?;
    let value = json_body(&body)?;
    let Some(response) = value.get("Response") else {
        return Err(PluginError::decode(format!(
            "Tencent Cloud DescribeTokenPlanList: a reply the plan reader can't read: {}",
            shape_of(&body)
        )));
    };
    let plans = response
        .get("TokenPlanSet")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|plan| plan.get("TeamId").and_then(Value::as_str).is_some_and(|id| !id.is_empty()))
        .cloned()
        .collect::<Vec<_>>();
    if plans.is_empty() {
        return Ok(vec![PlanCard::new("Token Plan Enterprise").with_error(format!(
            "this Tencent Cloud account has no Token Plan Enterprise in {region} that its API key can see (a sub-user sees only the plans it made)"
        ))]);
    }
    let several = plans.len() > 1;
    let mut cards = Vec::new();
    for listed in plans {
        let team_id = listed
            .get("TeamId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut plan = listed.clone();
        let mut told = body.clone();
        if let Ok(detail_body) = tencent_call(
            query,
            host,
            endpoint,
            region,
            "DescribeTokenPlan",
            json!({"TeamId": team_id}),
        )
        .await
            && let Ok(detail) = json_body(&detail_body)
            && let Some(detail) = detail.get("Response")
        {
            told = detail_body;
            for field in ["PackageInfo", "TokenSummary"] {
                if let Some(value) = detail.get(field) {
                    plan[field] = value.clone();
                }
            }
            for field in ["Status", "StopReason", "ProductType"] {
                if let Some(value) = detail.get(field).filter(|value| !value.as_str().is_some_and(str::is_empty)) {
                    plan[field] = value.clone();
                }
            }
        }
        let user = if several {
            plan.get("Name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(&team_id)
                .to_owned()
        } else {
            String::new()
        };
        let mut label = "Token Plan Enterprise".to_owned();
        if let Some(status) = plan.get("Status").and_then(Value::as_str)
            && !status.eq_ignore_ascii_case("enable")
        {
            label.push_str(" · ");
            label.push_str(status);
            if let Some(reason) = plan.get("StopReason").and_then(Value::as_str)
                && !reason.trim().is_empty()
                && !reason.eq_ignore_ascii_case("NORMAL")
                && !reason.eq_ignore_ascii_case("none")
            {
                label.push_str(", ");
                label.push_str(reason.trim());
            }
        }
        let mut card = PlanCard::new(label).with_user(user);
        if let Some(window) = tencent_window(&plan, query.now()) {
            card = card.with_windows(vec![window]);
        } else {
            card = card.with_error(format!(
                "Tencent Cloud told no quota for this Token Plan Enterprise: {}",
                shape_of(&told)
            ));
        }
        cards.push(card);
    }
    Ok(cards)
}

async fn tencent_call(
    query: &PlanQuery,
    host: &HostContext,
    endpoint: &str,
    region: &str,
    action: &str,
    params: Value,
) -> Result<Vec<u8>, PluginError> {
    let payload = hofund_plugin_sdk::json::to_vec(&params)
        .map_err(|error| PluginError::internal(error.to_string()))?;
    let timestamp = unix_seconds(query.now());
    let date = utc_date(query.now());
    let signed = "content-type;host;x-tc-action";
    let canonical = format!(
        "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:{TENCENT_HOST}\nx-tc-action:{}\n\n{signed}\n{}",
        action.to_ascii_lowercase(),
        sha256(&payload)
    );
    let scope = format!("{date}/tokenhub/tc3_request");
    let to_sign = format!(
        "TC3-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
        sha256(canonical.as_bytes())
    );
    let secret = format!("TC3{}", query.secret_access_key());
    let mut key = hmac(secret.as_bytes(), &date).to_vec();
    key = hmac(&key, "tokenhub").to_vec();
    key = hmac(&key, "tc3_request").to_vec();
    let signature = hex(&hmac(&key, &to_sign));
    let authorization = format!(
        "TC3-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        query.access_key_id()
    );
    let call = hofund_plugin_sdk::Call::post(endpoint)
        .header("content-type", "application/json; charset=utf-8")
        .header("x-tc-action", action)
        .header("x-tc-version", "2026-03-22")
        .header("x-tc-region", region)
        .header("x-tc-timestamp", timestamp.to_string())
        .sensitive_header("authorization", authorization)
        .json(payload);
    let (status, body) = fetch(host, call).await?;
    let value = hofund_plugin_sdk::json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    if let Some(error) = value.pointer("/Response/Error") {
        let code = error.get("Code").and_then(Value::as_str).unwrap_or("");
        let message = error.get("Message").and_then(Value::as_str).unwrap_or("");
        if !code.is_empty() || !message.is_empty() {
            let lower = code.to_ascii_lowercase();
            let hint = if lower.starts_with("authfailure") || lower.contains("unauthorized") {
                "; check the SecretId and SecretKey in the provider's editor (Tencent Cloud console, Access management, API keys), and that the key may read TokenHub (CAM)"
            } else {
                ""
            };
            return Err(if matches!(status, 401 | 403) || !hint.is_empty() {
                PluginError::auth(format!("Tencent Cloud {action}: {code} {message}{hint}"))
            } else {
                PluginError::status_error(status, format!("Tencent Cloud {action}: {code} {message}{hint}"))
            });
        }
    }
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    Ok(body)
}

fn tencent_window(plan: &Value, now: SystemTime) -> Option<PlanWindow> {
    let package = plan.get("PackageInfo")?;
    let mut limit = number(package.get("CycleQuota")).unwrap_or(0.0);
    if limit <= 0.0 {
        limit = number(package.get("TotalQuota"))?;
    }
    let used = number(package.get("TotalUsed"))?;
    if limit <= 0.0 {
        return None;
    }
    let unit = if plan
        .get("ProductType")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("enterprise-auto"))
    {
        "tokens"
    } else {
        "credits"
    };
    let mut window = PlanWindow::new("Month", (used / limit * 100.0).clamp(0.0, 100.0))
        .with_span(Duration::from_secs(30 * DAY))
        .with_count(used, limit, unit);
    let summary = plan.get("TokenSummary");
    let start = summary.and_then(|summary| time_value(summary.get("CycleStartTime")));
    let mut reset = summary.and_then(|summary| time_value(summary.get("CycleEndTime")));
    if let (Some(start), Some(end)) = (start, reset)
        && end > start
    {
        window = window.with_span(end.duration_since(start).unwrap_or_default());
    }
    if reset.is_none() {
        let start = time_value(package.get("StartTime"));
        let cycle = number(package.get("CurrentCycle")).unwrap_or(1.0).max(1.0) as i64;
        let monthly = package
            .get("CycleUnit")
            .and_then(Value::as_str)
            .is_none_or(|unit| unit.eq_ignore_ascii_case("month"));
        if let Some(start) = start.filter(|_| monthly) {
            let mut count = cycle;
            let mut end = add_months(start, count)?;
            while end <= now && count < 1200 {
                count += 1;
                end = add_months(start, count)?;
            }
            if let Some(expiry) = time_value(package.get("ExpireTime"))
                && expiry < end
            {
                end = expiry;
            }
            reset = Some(end);
        }
    }
    window = window.with_optional_reset(reset);
    Some(window)
}

#[cfg(test)]
pub(crate) fn volc_signature(
    access: &str,
    secret: &str,
    region: &str,
    action: &str,
    now: SystemTime,
) -> String {
    let query_text = format!("Action={}&Version=2024-01-01", percent_encode(action));
    let payload = b"{}";
    let content_sha = sha256(payload);
    let x_date = x_date(now);
    let date = &x_date[..8];
    let signed = "content-type;host;x-content-sha256;x-date";
    let canonical = format!(
        "POST\n/\n{query_text}\ncontent-type:application/json\nhost:{VOLC_HOST}\nx-content-sha256:{content_sha}\nx-date:{x_date}\n\n{signed}\n{content_sha}"
    );
    let scope = format!("{date}/{region}/ark/request");
    let to_sign = format!("HMAC-SHA256\n{x_date}\n{scope}\n{}", sha256(canonical.as_bytes()));
    let mut key = hmac(secret.as_bytes(), date).to_vec();
    for part in [region, "ark", "request"] {
        key = hmac(&key, part).to_vec();
    }
    format!(
        "HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={}",
        hex(&hmac(&key, &to_sign))
    )
}

#[cfg(test)]
pub(crate) fn tc3_signature(
    id: &str,
    secret: &str,
    service: &str,
    host: &str,
    action: &str,
    payload: &[u8],
    now: SystemTime,
) -> String {
    let timestamp = unix_seconds(now);
    let date = utc_date(now);
    let signed = "content-type;host;x-tc-action";
    let canonical = format!(
        "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:{host}\nx-tc-action:{}\n\n{signed}\n{}",
        action.to_ascii_lowercase(),
        sha256(payload)
    );
    let scope = format!("{date}/{service}/tc3_request");
    let to_sign = format!("TC3-HMAC-SHA256\n{timestamp}\n{scope}\n{}", sha256(canonical.as_bytes()));
    let secret = format!("TC3{secret}");
    let mut key = hmac(secret.as_bytes(), &date).to_vec();
    key = hmac(&key, service).to_vec();
    key = hmac(&key, "tc3_request").to_vec();
    format!(
        "TC3-HMAC-SHA256 Credential={id}/{scope}, SignedHeaders={signed}, Signature={}",
        hex(&hmac(&key, &to_sign))
    )
}
