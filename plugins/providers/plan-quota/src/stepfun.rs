use std::time::{Duration, SystemTime};

use hofund_plugin_api::{HostContext, PlanCard, PlanQuery, PlanWindow, PluginError, SecretString};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::unverified_claims;

use crate::{Reader, fetch, json_body, number, time_value, vendor_error};

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;
const RATE_PATH: &str = "/api/step.openapi.devcenter.Dashboard/QueryStepPlanRateLimit";
const STATUS_PATH: &str = "/api/step.openapi.devcenter.Dashboard/GetStepPlanStatus";
const REFRESH_PATH: &str = "/passport/proto.api.passport.v1.PassportService/RefreshToken";

#[derive(Clone)]
pub(crate) struct Session {
    access: String,
    refresh: String,
    web_id: String,
    account: String,
}

pub(crate) async fn read(
    reader: &Reader,
    query: &PlanQuery,
    host: &HostContext,
    platform: &str,
    app: &str,
) -> Result<Vec<PlanCard>, PluginError> {
    ensure_session(reader, query)?;
    let rate = step_ask(reader, query, host, platform, app, RATE_PATH).await?;
    let value = json_body(&rate)?;
    let windows = read_rate(&value);
    let mut plan = String::new();
    let mut until = None;
    let mut renew = String::new();
    if let Ok(status) = step_ask(reader, query, host, platform, app, STATUS_PATH).await
        && let Ok(value) = json_body(&status)
        && let Some(subscription) = value.get("subscription")
    {
        plan = subscription
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        until = time_value(subscription.get("expired_at"));
        renew = match subscription.get("auto_renew") {
            Some(Value::Bool(true)) => "auto".to_owned(),
            Some(Value::Bool(false)) => "off".to_owned(),
            Some(Value::String(value)) => match value.parse::<bool>() {
                Ok(true) => "auto".to_owned(),
                Ok(false) => "off".to_owned(),
                _ => String::new(),
            },
            _ => String::new(),
        };
    }
    if windows.is_empty() && plan.is_empty() {
        return Err(PluginError::decode("this StepFun account has no Step Plan"));
    }
    let mut card = PlanCard::new(if plan.is_empty() {
        "Step Plan".to_owned()
    } else {
        plan
    })
    .with_windows(windows);
    if let Some(at) = until {
        card = card.with_term(at, renew);
    }
    Ok(vec![card])
}

fn ensure_session(reader: &Reader, query: &PlanQuery) -> Result<(), PluginError> {
    let mut sessions = reader
        .step_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sessions.contains_key(query.api_key()) {
        return Ok(());
    }
    let session = parse_session(query.session_token())?;
    sessions.insert(query.api_key().to_owned(), session);
    Ok(())
}

fn parse_session(text: &str) -> Result<Session, PluginError> {
    let text = text.trim().trim_start_matches("Oasis-Token=");
    let (access, refresh, supplied_web) = if text.starts_with('{') {
        let value: Value = hofund_plugin_sdk::json::from_str(text)
            .map_err(|_| PluginError::config("the platform session is not valid JSON"))?;
        (
            value
                .get("access")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            value
                .get("refresh")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            value
                .get("webId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        )
    } else {
        let (access, refresh) = text.split_once("...").ok_or_else(|| {
            PluginError::auth("this Step Plan needs a platform session in session_token")
        })?;
        (access.to_owned(), refresh.to_owned(), String::new())
    };
    let access_claims = claims(&access)?;
    if access_claims.get("activated").and_then(Value::as_bool) != Some(true) {
        return Err(PluginError::auth("not signed in to StepFun yet"));
    }
    let account = claim_id(access_claims.get("oasis_id"))
        .ok_or_else(|| PluginError::auth("the platform session has no account"))?;
    let refresh_claims = claims(&refresh)?;
    let web_id = claim_id(refresh_claims.get("device_id")).unwrap_or(supplied_web);
    if web_id.is_empty() {
        return Err(PluginError::auth("the sign-in gave no session to keep"));
    }
    Ok(Session {
        access,
        refresh,
        web_id,
        account,
    })
}

fn claims(token: &str) -> Result<hofund_plugin_sdk::json::Map<String, Value>, PluginError> {
    unverified_claims(&SecretString::from(token.to_owned()))
        .map_err(|error| PluginError::auth(format!("the platform session is unreadable: {error}")))
}

fn claim_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn expires(token: &str) -> Option<SystemTime> {
    let claims = claims(token).ok()?;
    time_value(claims.get("exp"))
}

async fn current_session(
    reader: &Reader,
    query: &PlanQuery,
    host: &HostContext,
    platform: &str,
    app: &str,
    force: bool,
) -> Result<Session, PluginError> {
    let session = reader
        .step_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(query.api_key())
        .cloned()
        .ok_or_else(|| {
            PluginError::auth("signed out of StepFun: sign in again in the provider's settings")
        })?;
    if !force
        && expires(&session.access).is_some_and(|expires| {
            expires.duration_since(query.now()).unwrap_or_default() > Duration::from_secs(300)
        })
    {
        return Ok(session);
    }
    let (status, body) = step_call(host, platform, app, REFRESH_PATH, &session).await?;
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    let value = json_body(&body)?;
    let access = value
        .pointer("/accessToken/raw")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let access_claims = claims(&access)?;
    if access_claims.get("activated").and_then(Value::as_bool) != Some(true)
        || claim_id(access_claims.get("oasis_id")).as_deref() != Some(&session.account)
    {
        return Err(PluginError::auth(
            "signed out of StepFun: sign in again in the provider's settings",
        ));
    }
    let mut renewed = session.clone();
    renewed.access = access;
    if let Some(refresh) = value
        .pointer("/refreshToken/raw")
        .and_then(Value::as_str)
        .filter(|refresh| !refresh.is_empty())
    {
        renewed.refresh = refresh.to_owned();
    }
    let mut sessions = reader
        .step_sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for current in sessions.values_mut() {
        if current.refresh == session.refresh && current.account == session.account {
            *current = renewed.clone();
        }
    }
    Ok(renewed)
}

async fn step_ask(
    reader: &Reader,
    query: &PlanQuery,
    host: &HostContext,
    platform: &str,
    app: &str,
    path: &str,
) -> Result<Vec<u8>, PluginError> {
    let session = current_session(reader, query, host, platform, app, false).await?;
    let (status, body) = step_call(host, platform, app, path, &session).await?;
    if status != 401 {
        if status >= 300 {
            return Err(vendor_error(status, &body));
        }
        return Ok(body);
    }
    let session = current_session(reader, query, host, platform, app, true).await?;
    let (status, body) = step_call(host, platform, app, path, &session).await?;
    if status >= 300 {
        return Err(vendor_error(status, &body));
    }
    Ok(body)
}

async fn step_call(
    host: &HostContext,
    platform: &str,
    app: &str,
    path: &str,
    session: &Session,
) -> Result<(u16, Vec<u8>), PluginError> {
    let cookie = format!("Oasis-Token={}...{}", session.access, session.refresh);
    let call = hofund_plugin_sdk::Call::post(format!("{platform}{path}"))
        .sensitive_header("cookie", cookie)
        .header("content-type", "application/json")
        .header("connect-protocol-version", "1")
        .header("oasis-appid", app)
        .header("oasis-platform", "web")
        .header("oasis-webid", &session.web_id)
        .json(b"{}");
    fetch(host, call).await
}

fn read_rate(value: &Value) -> Vec<PlanWindow> {
    let mut out = Vec::new();
    for (name, seconds, left, reset, resetless) in [
        (
            "5 hours",
            5 * HOUR,
            "five_hour_usage_left_rate",
            "five_hour_usage_reset_time",
            false,
        ),
        (
            "7 days",
            7 * DAY,
            "weekly_usage_left_rate",
            "weekly_usage_reset_time",
            false,
        ),
    ] {
        if let Some(window) = rate_window(value, name, seconds, left, reset, resetless) {
            out.push(window);
        }
    }
    if let Some(credit) = value.get("plan_credit_rate_limit") {
        let resetless = credit
            .get("credit_buckets")
            .and_then(Value::as_array)
            .is_some_and(|buckets| !buckets.is_empty());
        if let Some(window) = rate_window(
            credit,
            "Credits",
            0,
            "subscription_credit_left_rate",
            "subscription_credit_reset_time",
            resetless,
        ) {
            out.push(window);
        }
    }
    out
}

fn rate_window(
    value: &Value,
    name: &str,
    seconds: u64,
    left_field: &str,
    reset_field: &str,
    resetless: bool,
) -> Option<PlanWindow> {
    let reset = time_value(value.get(reset_field));
    if reset.is_none() && !resetless {
        return None;
    }
    let mut left = number(value.get(left_field))?;
    if left > 1.0 {
        left /= 100.0;
    }
    let left = left.clamp(0.0, 1.0);
    let mut window = PlanWindow::new(name, (1.0 - left) * 100.0);
    if seconds > 0 {
        window = window.with_span(Duration::from_secs(seconds));
    }
    Some(window.with_optional_reset(reset))
}
