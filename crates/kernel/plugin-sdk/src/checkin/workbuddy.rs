//! `WorkBuddy` (China) daily check-in against copilot.tencent.com billing meters.

use crate::json;
use crate::transport::Call;
use hofund_plugin_api::{CheckinOutcome, CheckinResult, PluginError};

use super::{CheckinTransport, expired_sign_in, result_failed};

const CHECKIN_ALREADY: i64 = 1001;
const CHECKIN_INELIGIBLE: i64 = 1002;
const CHECKIN_ENDED: i64 = 1003;

/// Which `WorkBuddy` API host an account uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkBuddySite {
    /// Chinese build (`copilot.tencent.com`).
    China,
    /// International build (`www.workbuddy.ai`); check-in is usually off.
    International,
}

impl WorkBuddySite {
    fn api_base(self) -> &'static str {
        match self {
            Self::China => "https://copilot.tencent.com",
            Self::International => "https://www.workbuddy.ai",
        }
    }

    fn user_agent(self) -> &'static str {
        match self {
            Self::China | Self::International => "WorkBuddy/5.5.6",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct Envelope<T> {
    code: serde_json::Number,
    msg: String,
    data: Option<T>,
}

#[derive(Debug, serde::Deserialize)]
struct Status {
    active: bool,
    #[serde(rename = "today_checked_in")]
    today_checked_in: bool,
    #[serde(rename = "streak_days", deserialize_with = "flex_float")]
    streak_days: f64,
    #[serde(rename = "today_credit", default, deserialize_with = "flex_float")]
    today_credit: f64,
}

#[derive(Debug, serde::Deserialize)]
struct Claim {
    #[serde(deserialize_with = "flex_float")]
    credit: f64,
    #[serde(rename = "streak_days", deserialize_with = "flex_float")]
    streak_days: f64,
    #[serde(default)]
    message: String,
}

fn flex_float<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("invalid number")),
        serde_json::Value::String(text) => text
            .parse()
            .map_err(|_| serde::de::Error::custom("invalid numeric string")),
        serde_json::Value::Null => Ok(0.0),
        _ => Err(serde::de::Error::custom("expected number or string")),
    }
}

/// Runs `WorkBuddy` check-in: activity status, then daily claim when due.
///
/// # Errors
///
/// Returns [`PluginError`] only when the transport fails before a vendor body is read.
pub async fn run<T: CheckinTransport>(
    transport: &T,
    site: WorkBuddySite,
) -> Result<CheckinResult, PluginError> {
    let status = match activity_status(transport, site).await {
        Ok(status) => status,
        Err(error) => return Ok(handle_status_error(transport, site, error).await),
    };
    if !status.active {
        return Ok(CheckinResult::new(CheckinOutcome::Inactive, ""));
    }
    if status.today_checked_in {
        return Ok(CheckinResult::new(CheckinOutcome::Done, "")
            .with_rewards(status.today_credit, streak_days_i32(status.streak_days)));
    }
    match daily_claim(transport, site).await {
        Ok(claim) => Ok(CheckinResult::new(CheckinOutcome::Claimed, claim.message)
            .with_rewards(claim.credit, streak_days_i32(claim.streak_days))),
        Err(error) => Ok(map_claim_error(error)),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn streak_days_i32(value: f64) -> i32 {
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

async fn activity_status<T: CheckinTransport>(
    transport: &T,
    site: WorkBuddySite,
) -> Result<Status, String> {
    let url = format!(
        "{}/v2/billing/meter/checkin-activity-status",
        site.api_base()
    );
    post_json(transport, site, &url).await
}

async fn daily_claim<T: CheckinTransport>(
    transport: &T,
    site: WorkBuddySite,
) -> Result<Claim, String> {
    let url = format!("{}/v2/billing/meter/daily-checkin", site.api_base());
    post_json(transport, site, &url).await
}

async fn post_json<T: CheckinTransport, D: for<'de> serde::Deserialize<'de>>(
    transport: &T,
    site: WorkBuddySite,
    url: &str,
) -> Result<D, String> {
    let call = Call::post(url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", site.user_agent())
        .bytes(b"{}");
    let reply = transport
        .send(call)
        .await
        .map_err(|error| error.to_string())?;
    if expired_sign_in(&reply) {
        return Err("unauthorized".to_owned());
    }
    let body = std::str::from_utf8(reply.body()).unwrap_or("");
    if !(200..300).contains(&reply.status()) {
        return Err(format!("HTTP {}", reply.status()));
    }
    let env: Envelope<D> = json::from_str(body).map_err(|error| error.to_string())?;
    let code = env.code.as_i64().unwrap_or(-1);
    if code != 0 {
        return Err(format!("code {code}: {}", env.msg));
    }
    env.data.ok_or_else(|| "empty data".to_owned())
}

async fn handle_status_error<T: CheckinTransport>(
    transport: &T,
    site: WorkBuddySite,
    error: String,
) -> CheckinResult {
    if error != "unauthorized" {
        return result_failed(error);
    }
    let summary_url = format!(
        "{}/billing/meter/get-user-resource-summary",
        site.api_base()
    );
    if post_json::<T, serde_json::Value>(transport, site, &summary_url)
        .await
        .is_ok()
    {
        return CheckinResult::new(
            CheckinOutcome::Ineligible,
            "WorkBuddy answered the check-in 401 while this account's credits read fine: check in in the WorkBuddy app",
        );
    }
    CheckinResult::new(
        CheckinOutcome::Failed,
        "WorkBuddy refused this account's sign-in (401); sign in again",
    )
}

fn map_claim_error(error: String) -> CheckinResult {
    if let Some(code) = error.strip_prefix("code ")
        && let Some((num, msg)) = code.split_once(':')
        && let Ok(code) = num.trim().parse::<i64>()
    {
        let detail = msg.trim().to_owned();
        return match code {
            CHECKIN_ALREADY => CheckinResult::new(CheckinOutcome::Done, detail),
            CHECKIN_INELIGIBLE => CheckinResult::new(CheckinOutcome::Ineligible, detail),
            CHECKIN_ENDED => CheckinResult::new(CheckinOutcome::Inactive, detail),
            _ => result_failed(error),
        };
    }
    if error == "unauthorized" {
        return CheckinResult::new(
            CheckinOutcome::Failed,
            "WorkBuddy refused this account's sign-in (401); sign in again",
        );
    }
    result_failed(error)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hofund_plugin_api::{CheckinOutcome, PluginError};

    use super::{WorkBuddySite, run};
    use crate::checkin::CheckinTransport;
    use crate::transport::{Call, Reply};

    struct Mock {
        seen: Arc<Mutex<Vec<String>>>,
        responses: Vec<Reply>,
    }

    impl CheckinTransport for Mock {
        async fn send(&self, call: Call) -> Result<Reply, PluginError> {
            tokio::task::yield_now().await;
            let (uri, _) = call.uri_and_body();
            self.seen.lock().expect("lock").push(uri.to_owned());
            let index = self.seen.lock().expect("lock").len() - 1;
            Ok(self
                .responses
                .get(index)
                .cloned()
                .unwrap_or_else(|| Reply::from_parts(500, b"missing mock response")))
        }
    }

    #[tokio::test]
    async fn a3b_10_workbuddy_checkin_claims_when_active() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let transport = Mock {
            seen: Arc::clone(&seen),
            responses: vec![
                Reply::from_parts(
                    200,
                    br#"{"code":0,"msg":"ok","data":{"active":true,"today_checked_in":false,"streak_days":"3","today_credit":"0"}}"#,
                ),
                Reply::from_parts(
                    200,
                    br#"{"code":0,"msg":"ok","data":{"credit":"100","streak_days":"4","message":"ok"}}"#,
                ),
            ],
        };
        let result = run(&transport, WorkBuddySite::China).await.expect("run");
        assert_eq!(result.outcome(), CheckinOutcome::Claimed);
        assert!((result.credit() - 100.0).abs() < f64::EPSILON);
        let urls = seen.lock().expect("lock");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].contains("checkin-activity-status"));
        assert!(urls[1].contains("daily-checkin"));
    }
}
