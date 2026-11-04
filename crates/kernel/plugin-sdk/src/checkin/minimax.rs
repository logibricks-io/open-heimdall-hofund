//! `MiniMax` Code daily check-in panel requests.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::json;
use crate::transport::Call;
use hofund_plugin_api::{CheckinOutcome, CheckinResult, PluginError};

use super::{CheckinTransport, expired_sign_in, result_failed};

const SIGN_SECRET: &str = "I*7Cf%WZ#S&%1RlZJ&C2";

const DAY_UPCOMING: i64 = 1;
const DAY_CLAIMABLE: i64 = 2;
const DAY_CLAIMED: i64 = 3;
const DAY_DISABLED: i64 = 4;

/// Which `MiniMax` Code site an account belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MiniMaxSite {
    /// `agent.minimax.cn`
    China,
    /// `agent.minimax.io`
    Global,
}

impl MiniMaxSite {
    fn base_url(self) -> &'static str {
        match self {
            Self::China => "https://agent.minimax.cn/minimax-cloud/api/v1/signin",
            Self::Global => "https://agent.minimax.io/minimax-cloud/api/v1/signin",
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct Base {
    #[serde(rename = "status_code")]
    code: i64,
    #[serde(rename = "status_msg")]
    message: String,
}

#[derive(Debug, serde::Deserialize)]
#[allow(clippy::struct_field_names)]
struct Day {
    day_no: i32,
    points: f64,
    status: i64,
    is_today: bool,
}

#[derive(Debug, serde::Deserialize)]
struct StatusBody {
    data: StatusData,
    base_resp: Option<Base>,
}

#[derive(Debug, serde::Deserialize)]
struct StatusData {
    days: Vec<Day>,
}

#[derive(Debug, serde::Deserialize)]
struct ClaimBody {
    data: ClaimData,
    base_resp: Option<Base>,
}

#[derive(Debug, serde::Deserialize)]
struct ClaimData {
    #[serde(rename = "claim_result")]
    result: i64,
    day_no: i32,
    points: f64,
}

/// Runs `MiniMax` Code check-in: GET `/status`, POST `/claim` when today's panel is claimable.
///
/// # Errors
///
/// Returns [`PluginError`] only when the transport fails before a vendor body is read.
pub async fn run<T: CheckinTransport>(
    transport: &T,
    site: MiniMaxSite,
    now: SystemTime,
) -> Result<CheckinResult, PluginError> {
    let status: StatusBody = match call(transport, site, "GET", "/status", "", now).await {
        Ok(body) => body,
        Err(error) => return Ok(result_failed(error)),
    };
    if let Some(base) = &status.base_resp
        && base.code != 0
    {
        return Ok(CheckinResult::new(
            CheckinOutcome::Failed,
            format!("code {}: {}", base.code, base.message),
        ));
    }
    let today = status.data.days.iter().find(|day| day.is_today);
    let Some(today) = today else {
        return Ok(CheckinResult::new(CheckinOutcome::Inactive, ""));
    };
    match today.status {
        DAY_CLAIMED => {
            return Ok(CheckinResult::new(CheckinOutcome::Done, "")
                .with_rewards(today.points, today.day_no));
        }
        DAY_DISABLED => {
            return Ok(CheckinResult::new(
                CheckinOutcome::Ineligible,
                "today's check-in is closed for this account",
            ));
        }
        DAY_UPCOMING | DAY_CLAIMABLE => {}
        _ => {
            return Ok(CheckinResult::new(
                CheckinOutcome::Failed,
                format!("unexpected panel status {}", today.status),
            ));
        }
    }
    let claim: ClaimBody = match call(transport, site, "POST", "/claim", "{}", now).await {
        Ok(body) => body,
        Err(error) => return Ok(result_failed(error)),
    };
    if let Some(base) = &claim.base_resp
        && base.code != 0
    {
        return Ok(CheckinResult::new(
            CheckinOutcome::Failed,
            format!("code {}: {}", base.code, base.message),
        ));
    }
    let mut day = claim.data.day_no;
    let mut credit = claim.data.points;
    if day == 0 {
        day = today.day_no;
    }
    if credit == 0.0 {
        credit = today.points;
    }
    match claim.data.result {
        1 => Ok(CheckinResult::new(CheckinOutcome::Claimed, "").with_rewards(credit, day)),
        2 => Ok(CheckinResult::new(CheckinOutcome::Done, "").with_rewards(credit, day)),
        other => Ok(CheckinResult::new(
            CheckinOutcome::Failed,
            format!("claim_result {other}"),
        )),
    }
}

async fn call<T: CheckinTransport, V: serde::de::DeserializeOwned>(
    transport: &T,
    site: MiniMaxSite,
    method: &'static str,
    path: &str,
    body: &str,
    now: SystemTime,
) -> Result<V, String> {
    let url = format!("{}{path}?timezone_id=Asia%2FShanghai", site.base_url());
    let mut call = if method == "GET" {
        Call::get(url)
    } else {
        Call::post(url).bytes(body.as_bytes())
    };
    call = sign(call, body, now);
    let reply = transport
        .send(call)
        .await
        .map_err(|error| error.to_string())?;
    if expired_sign_in(&reply) {
        return Err("MiniMax Code's sign-in has expired — sign in again".to_owned());
    }
    let text = std::str::from_utf8(reply.body()).unwrap_or("");
    if !(200..300).contains(&reply.status()) {
        return Err(format!("HTTP {}", reply.status()));
    }
    json::from_str(text).map_err(|error| error.to_string())
}

use md5::{Digest, Md5};

fn sign(call: Call, body: &str, now: SystemTime) -> Call {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let ts = secs.to_string();
    let mut hasher = Md5::new();
    hasher.update(format!("{ts}{SIGN_SECRET}{body}"));
    let digest = hasher.finalize();
    let signature = format!("{digest:x}");
    call.header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("x-timestamp", ts)
        .header("x-signature", signature)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, UNIX_EPOCH};

    use hofund_plugin_api::CheckinOutcome;

    use super::{MiniMaxSite, run};
    use crate::checkin::CheckinTransport;
    use crate::transport::{Call, Reply};

    struct Mock {
        seen: Arc<Mutex<Vec<String>>>,
        responses: Vec<Reply>,
    }

    impl CheckinTransport for Mock {
        async fn send(&self, call: Call) -> Result<Reply, hofund_plugin_api::PluginError> {
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
    async fn a3b_8_minimax_checkin_signs_status_and_claim() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let transport = Mock {
            seen: Arc::clone(&seen),
            responses: vec![
                Reply::from_parts(
                    200,
                    br#"{"data":{"days":[{"day_no":3,"points":10,"status":2,"is_today":true}]},"base_resp":{"status_code":0,"status_msg":""}}"#,
                ),
                Reply::from_parts(
                    200,
                    br#"{"data":{"claim_result":1,"day_no":3,"points":10},"base_resp":{"status_code":0,"status_msg":""}}"#,
                ),
            ],
        };
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let result = run(&transport, MiniMaxSite::China, now).await.expect("run");
        assert_eq!(result.outcome(), CheckinOutcome::Claimed);
        let rows = seen.lock().expect("lock");
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("agent.minimax.cn"));
        assert!(rows[0].contains("/status"));
        assert!(rows[1].contains("/claim"));
    }
}
