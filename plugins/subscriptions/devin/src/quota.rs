//! Account quota from GetUserStatus and free-model exclusions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{
    AccountId, BodyStream, HostContext, PluginError, PluginErrorKind, QuotaWindow,
};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, time};

use crate::catalog::{self, os_name};
use crate::connect::CLI_VERSION;
use crate::creds::Stored;
use crate::models;

const STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const BODY_CAP: usize = 1024 * 1024;
const PERCENT: u64 = 100;

pub(crate) async fn read(
    host: &HostContext,
    account: &AccountId,
) -> Result<Vec<QuotaWindow>, PluginError> {
    let stored = Stored::load(host, account).await?;
    let body = json!({
        "metadata": {
            "ideName": "devin-cli",
            "ideVersion": CLI_VERSION,
            "extensionName": "devin-cli",
            "extensionVersion": CLI_VERSION,
            "apiKey": secret_text(&stored.credential.key),
            "locale": "en",
            "os": os_name(),
        }
    });
    let response = Call::post(format!("{}{STATUS_PATH}", catalog::origin(&stored)))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .read_timeout(Duration::from_secs(15))
        .bytes(body.to_string().into_bytes())
        .open_raw(host.http(), 1)
        .await
        .map_err(rpc_failure)?;
    let status = response.status().as_u16();
    let bytes = read_body(response.into_body()).await?;
    if rejected(status, &bytes) {
        return Err(PluginError::auth(
            "Devin's sign-in has expired — sign in again",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(PluginError::status_error(status, "GetUserStatus failed"));
    }
    let reply: Value = if bytes.is_empty() {
        Value::Null
    } else {
        hofund_plugin_sdk::json::from_slice(&bytes)
            .map_err(|_| PluginError::decode("GetUserStatus returned invalid JSON"))?
    };
    let mut windows = quota_windows(&reply)?;
    if !windows.is_empty() {
        let catalog = catalog::families_for(host, account, &stored).await;
        let free = models::free_ids(&catalog.families, &catalog.free)?;
        if !free.is_empty() {
            for window in &mut windows {
                *window = window.clone().with_excluded_models(free.clone());
            }
        }
    }
    if let Some(acu) = acu_window(&reply)? {
        windows.push(acu);
    }
    Ok(windows)
}

async fn read_body(mut stream: BodyStream) -> Result<Vec<u8>, PluginError> {
    let mut bytes = Vec::new();
    while bytes.len() < BODY_CAP {
        let Some(chunk) = stream.next_chunk().await else {
            break;
        };
        let chunk = chunk.map_err(rpc_failure)?;
        let remaining = BODY_CAP - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(bytes)
}

fn quota_windows(reply: &Value) -> Result<Vec<QuotaWindow>, PluginError> {
    let status = reply
        .get("userStatus")
        .and_then(|value| value.get("planStatus"))
        .unwrap_or(&Value::Null);
    let info = status.get("planInfo").unwrap_or(&Value::Null);
    let quota =
        info.get("billingStrategy").and_then(Value::as_str) == Some("BILLING_STRATEGY_QUOTA");
    let mut windows = Vec::new();
    for (name, left, reset, hidden) in [
        (
            "1 day",
            "dailyQuotaRemainingPercent",
            "dailyQuotaResetAtUnix",
            "hideDailyQuota",
        ),
        (
            "7 days",
            "weeklyQuotaRemainingPercent",
            "weeklyQuotaResetAtUnix",
            "hideWeeklyQuota",
        ),
    ] {
        if info.get(hidden).and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let reset = reset_at(status.get(reset));
        let left = status.get(left);
        if left.is_none_or(Value::is_null) && !(quota && reset.is_some()) {
            continue;
        }
        let used = PERCENT.saturating_sub(percent(left));
        windows.push(QuotaWindow::new(name, used, Some(PERCENT), reset)?);
    }
    Ok(windows)
}

fn acu_window(reply: &Value) -> Result<Option<QuotaWindow>, PluginError> {
    let status = reply
        .get("userStatus")
        .and_then(|value| value.get("planStatus"))
        .unwrap_or(&Value::Null);
    let limit = number(status.get("acuLimit"));
    if limit <= 0.0 {
        return Ok(None);
    }
    let used = (100.0 * number(status.get("acuConsumed")) / limit).clamp(0.0, 100.0);
    let reset = status
        .get("planEnd")
        .and_then(Value::as_str)
        .and_then(time::parse_rfc3339);
    Ok(Some(
        QuotaWindow::new("ACUs", rounded_percent(used), Some(PERCENT), reset)?.with_blocking(false),
    ))
}

fn number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(value)) => value.as_f64().unwrap_or(0.0),
        Some(Value::String(value)) => value.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn percent(value: Option<&Value>) -> u64 {
    rounded_percent(number(value).clamp(0.0, 100.0))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "callers clamp percentages to 0..=100"
)]
fn rounded_percent(value: f64) -> u64 {
    value.round() as u64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "negative, non-finite, and out-of-range timestamps become no reset"
)]
fn reset_at(value: Option<&Value>) -> Option<SystemTime> {
    let seconds = number(value);
    if !seconds.is_finite() || seconds <= 0.0 || seconds > u64::MAX as f64 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

fn rejected(status: u16, body: &[u8]) -> bool {
    if status == 401 {
        return true;
    }
    let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(body) else {
        return false;
    };
    let object = value
        .get("error")
        .filter(|error| error.is_object())
        .unwrap_or(&value);
    object.get("code").and_then(Value::as_str) == Some("unauthenticated")
}

fn rpc_failure(error: PluginError) -> PluginError {
    if matches!(
        error.kind(),
        PluginErrorKind::Timeout
            | PluginErrorKind::Connect
            | PluginErrorKind::Transport
            | PluginErrorKind::Interrupted
    ) {
        return PluginError::io(format!("GetUserStatus failed: {}", error.message()));
    }
    error
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use hofund_plugin_api::{BodyStream, PluginError, PluginErrorKind};
    use hofund_plugin_sdk::json::json;

    use super::{BODY_CAP, acu_window, quota_windows, read_body, reset_at, rpc_failure};

    #[test]
    fn status_body_cap_is_one_mib() {
        assert_eq!(BODY_CAP, 1024 * 1024);
    }

    #[test]
    fn body_cap_subtracts_bytes_from_an_earlier_chunk() {
        let stream = BodyStream::unfold(0_u8, |step| async move {
            match step {
                0 => Some((Ok(vec![b'a'; 512]), 1)),
                1 => Some((Ok(vec![b'b'; BODY_CAP]), 2)),
                _ => None,
            }
        });
        let bytes = hofund_kit_subscription::block_on(read_body(stream)).expect("body");
        assert_eq!(bytes.len(), BODY_CAP);
    }

    #[test]
    fn omitted_zero_shares_are_exhausted_only_for_quota_billing() {
        let reply = json!({"userStatus": {"planStatus": {
            "planInfo": {"billingStrategy": "BILLING_STRATEGY_QUOTA"},
            "dailyQuotaRemainingPercent": 100,
            "dailyQuotaResetAtUnix": "1790841600",
            "weeklyQuotaResetAtUnix": "1791100800"
        }}});
        let windows = quota_windows(&reply).expect("windows");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name(), "1 day");
        assert_eq!(windows[0].used(), 0);
        assert_eq!(
            windows[0].resets_at(),
            Some(UNIX_EPOCH + Duration::from_secs(1_790_841_600))
        );
        assert_eq!(windows[1].name(), "7 days");
        assert_eq!(windows[1].used(), 100);
        assert_eq!(
            windows[1].resets_at(),
            Some(UNIX_EPOCH + Duration::from_secs(1_791_100_800))
        );

        let mut credits = reply.clone();
        credits["userStatus"]["planStatus"]["planInfo"]["billingStrategy"] =
            json!("BILLING_STRATEGY_CREDITS");
        let windows = quota_windows(&credits).expect("windows");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].name(), "1 day");
    }

    #[test]
    fn hidden_and_resetless_missing_shares_are_not_windows() {
        let reply = json!({"userStatus": {"planStatus": {
            "planInfo": {
                "billingStrategy": "BILLING_STRATEGY_QUOTA",
                "hideWeeklyQuota": true
            },
            "dailyQuotaResetAtUnix": "1790841600",
            "weeklyQuotaResetAtUnix": "1791100800"
        }}});
        let mut windows = quota_windows(&reply).expect("windows");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].name(), "1 day");
        windows[0] = windows[0]
            .clone()
            .with_excluded_models(vec!["swe-2".to_owned()]);
        assert_eq!(windows[0].excluded_models(), ["swe-2"]);
        assert!(reset_at(Some(&json!(0))).is_none());
    }

    #[test]
    fn acu_limit_reports_an_informational_percentage_window() {
        let reply = json!({"userStatus": {"planStatus": {
            "acuConsumed": 12.5,
            "acuLimit": 50,
            "planEnd": "2026-10-26T21:45:55Z"
        }}});
        let window = acu_window(&reply).expect("parse").expect("ACUs");
        assert_eq!(window.name(), "ACUs");
        assert_eq!(window.used(), 25);
        assert_eq!(window.limit(), Some(100));
        assert!(!window.blocking());
        assert!(window.resets_at().is_some());
        assert!(acu_window(&json!({})).expect("none").is_none());
    }

    #[test]
    fn transport_shaped_rpc_failures_become_io_errors() {
        for error in [
            PluginError::timeout("slow"),
            PluginError::connect("offline"),
            PluginError::transport("reset"),
            PluginError::interrupted("body reset"),
        ] {
            assert_eq!(rpc_failure(error).kind(), PluginErrorKind::Io);
        }
        let status = PluginError::status_error(503, "unavailable");
        assert_eq!(rpc_failure(status).kind(), PluginErrorKind::Status);
    }
}
