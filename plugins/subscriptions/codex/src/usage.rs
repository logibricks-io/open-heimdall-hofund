//! The account's usage windows from the ChatGPT usage page, `/wham/usage`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{HostContext, PluginError, QuotaWindow};
use hofund_plugin_sdk::json::{self, Value};

use crate::Endpoints;
use crate::limits::{self, BASIS};
use crate::tokens::Access;
use crate::upstream::account_call;

/// The usage page beside the Codex backend: `{base}/wham/usage`, where the
/// base drops the trailing `/codex`.
pub(crate) fn url(endpoints: &Endpoints) -> String {
    let base = endpoints.base.trim_end_matches('/');
    let base = base.strip_suffix("/codex").unwrap_or(base);
    format!("{base}/wham/usage")
}

/// Reads the account's usage windows.
pub(crate) async fn read(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &Access,
) -> Result<Vec<QuotaWindow>, PluginError> {
    let reply = account_call(url(endpoints), access)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200 => windows(reply.body(), host.clock().now()),
        401 | 403 => Err(PluginError::auth(
            "ChatGPT refused to read the account's usage",
        )),
        status => Err(PluginError::status_error(
            status,
            "ChatGPT did not read the account's usage",
        )),
    }
}

/// The windows a usage page tells, in basis points: the primary window,
/// then the secondary one. When the account goes on past its allowance on
/// credits, a spent window does not stop it.
pub(crate) fn windows(body: &[u8], now: SystemTime) -> Result<Vec<QuotaWindow>, PluginError> {
    let usage: Value = json::from_slice(body)
        .map_err(|_| PluginError::decode("the ChatGPT usage page is not JSON"))?;
    let credits = usage.get("credits").filter(|credits| credits.is_object());
    let on_credits = credits.is_some_and(|credits| {
        let flag = |name: &str| credits.get(name).and_then(Value::as_bool) == Some(true);
        let balance = match credits.get("balance") {
            Some(Value::String(text)) => Some(text.clone()),
            Some(Value::Number(number)) => Some(number.to_string()),
            _ => None,
        };
        limits::goes_on_credits(flag("has_credits"), flag("unlimited"), balance.as_deref())
    });
    let mut out = Vec::new();
    for which in ["primary_window", "secondary_window"] {
        let Some(window) = usage
            .pointer(&format!("/rate_limit/{which}"))
            .filter(|window| window.is_object())
        else {
            continue;
        };
        let used = window
            .get("used_percent")
            .and_then(Value::as_f64)
            .map_or(0, limits::basis_points);
        let seconds = window
            .get("limit_window_seconds")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let resets_at = match window.get("reset_at").and_then(Value::as_i64) {
            Some(at) if at > 0 => Some(UNIX_EPOCH + Duration::from_secs(at.unsigned_abs())),
            _ => window
                .get("reset_after_seconds")
                .and_then(Value::as_i64)
                .filter(|after| *after > 0)
                .map(|after| now + Duration::from_secs(after.unsigned_abs())),
        };
        out.push(
            QuotaWindow::new(window_name(seconds), used, Some(BASIS), resets_at)?
                .with_blocking(!on_credits),
        );
    }
    Ok(out)
}

/// A window named by how long it runs: whole days, else whole hours, else
/// `Allowance`.
pub(crate) fn window_name(seconds: i64) -> String {
    const DAY: i64 = 24 * 60 * 60;
    const HOUR: i64 = 60 * 60;
    let (count, unit) = if seconds > 0 && seconds % DAY == 0 {
        (seconds / DAY, "day")
    } else if seconds > 0 && seconds % HOUR == 0 {
        (seconds / HOUR, "hour")
    } else {
        return "Allowance".to_owned();
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural}")
}
