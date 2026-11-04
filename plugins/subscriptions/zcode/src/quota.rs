//! GLM coding plan resets from business API lists.

use hofund_plugin_api::{HostContext, PluginError, QuotaWindow};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::time::parse_rfc3339;

use crate::Endpoints;
use crate::api;
use crate::session::Stored;
use crate::team;

pub(crate) async fn read(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
) -> Result<Vec<QuotaWindow>, PluginError> {
    let biz = endpoints.biz_root(&stored.auth.base);
    let device_path = std::sync::Mutex::new(None);
    if stored.auth.team() {
        let token = stored
            .auth
            .team_token
            .as_ref()
            .ok_or_else(|| PluginError::auth("the team seat has no business sign-in token"))?;
        let headers = team::team_headers(&stored.auth.base, &stored.auth.org, &stored.auth.project);
        let url = format!("{biz}/api/biz/customer-package-reset/list?targetType=TEAM");
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        let data = api::get_envelope(&api::EnvelopeRequest::get(
            host,
            endpoints,
            &url,
            Some(token),
            &header_refs,
            &device_path,
        ))
        .await?;
        return windows_from_resets(&data);
    }
    let auth = stored
        .auth
        .biz_auth_bearer()
        .ok_or_else(|| PluginError::auth("the stored Z Code sign-in has no plan key for quota"))?;
    let url = format!("{biz}/api/biz/customer-package-reset/list?targetType=PERSONAL");
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &url,
        Some(&hofund_plugin_api::SecretString::from(auth)),
        &[],
        &device_path,
    ))
    .await?;
    windows_from_resets(&data)
}

pub(crate) fn windows_from_resets(data: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    let five = data
        .get("fiveHourResets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let week = data
        .get("weekResets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    count_available(&five, "five hours", &mut out)?;
    count_available(&week, "weekly", &mut out)?;
    Ok(out)
}

fn count_available(
    rows: &[Value],
    label: &str,
    out: &mut Vec<QuotaWindow>,
) -> Result<(), PluginError> {
    let mut count = 0_u64;
    let mut nearest: Option<std::time::SystemTime> = None;
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        if !object
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        count += 1;
        if let Some(until) = parse_time(object.get("expireTime")) {
            nearest = Some(match nearest {
                Some(current) if current < until => current,
                _ => until,
            });
        }
    }
    if count == 0 {
        return Ok(());
    }
    let used = 0_u64;
    let limit = count;
    let window = QuotaWindow::new(label, used, Some(limit), nearest)?;
    out.push(window.with_blocking(true));
    Ok(())
}

fn parse_time(value: Option<&Value>) -> Option<std::time::SystemTime> {
    match value {
        Some(Value::String(text)) => parse_vendor_time(text),
        Some(Value::Number(number)) => {
            let secs = number.as_f64()?;
            if secs > 1e12 {
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(secs as u64))
            } else {
                std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs as u64))
            }
        }
        _ => None,
    }
}

fn parse_vendor_time(text: &str) -> Option<std::time::SystemTime> {
    let normalized = text.trim().replace(' ', "T");
    parse_rfc3339(&format!("{normalized}+08:00")).or_else(|| parse_rfc3339(&normalized))
}
