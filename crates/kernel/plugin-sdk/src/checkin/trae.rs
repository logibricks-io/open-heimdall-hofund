//! Trae CN daily check-in against api.trae.cn.

use crate::json;
use crate::transport::Call;
use hofund_plugin_api::{CheckinOutcome, CheckinResult, PluginError};

use super::{CheckinTransport, expired_sign_in, result_failed};

const BASE: &str = "https://api.trae.cn/trae/api/v2/ug/checkin_credits";

const SIGNED_OUT: i64 = 1001;
const OWN_APP_ONLY: i64 = 9074;
const DEVICE_CHECKED: i64 = 9095;

#[derive(Debug, serde::Deserialize)]
struct Status {
    code: i64,
    message: String,
    enable: bool,
    checked_in: bool,
    credits: f64,
    extra_credits: f64,
}

/// Runs Trae CN check-in: POST `/status`, then `/claim` when due.
///
/// # Errors
///
/// Returns [`PluginError`] only for transport failures before a vendor body is read.
pub async fn run<T: CheckinTransport>(transport: &T) -> Result<CheckinResult, PluginError> {
    let status = match fetch(transport, "/status").await {
        Ok(status) => status,
        Err(error) => return Ok(result_failed(error)),
    };
    if status.code != 0 {
        return Ok(map_refused(status.code, &status.message, false));
    }
    if !status.enable {
        return Ok(CheckinResult::new(CheckinOutcome::Inactive, ""));
    }
    if status.checked_in {
        return Ok(CheckinResult::new(CheckinOutcome::Done, "").with_rewards(status.credits, 0));
    }
    let claim = match fetch(transport, "/claim").await {
        Ok(claim) => claim,
        Err(error) => return Ok(result_failed(error)),
    };
    if claim.code == OWN_APP_ONLY {
        let message = if claim.message.trim().is_empty() {
            "Trae CN refused the check-in (9074)".to_owned()
        } else {
            claim.message
        };
        return Ok(CheckinResult::new(CheckinOutcome::Ineligible, message));
    }
    if claim.code != 0 {
        return Ok(map_refused(claim.code, &claim.message, true));
    }
    let credit = if claim.credits == 0.0 {
        status.credits
    } else {
        claim.credits
    } + claim.extra_credits;
    Ok(CheckinResult::new(CheckinOutcome::Claimed, "").with_rewards(credit, 0))
}

async fn fetch<T: CheckinTransport>(transport: &T, path: &str) -> Result<Status, String> {
    let call = Call::post(format!("{BASE}{path}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .bytes(b"{}");
    let reply = transport
        .send(call)
        .await
        .map_err(|error| error.to_string())?;
    if expired_sign_in(&reply) {
        return Err("Trae CN's sign-in has expired — sign in again".to_owned());
    }
    let body = std::str::from_utf8(reply.body()).unwrap_or("");
    if reply.status() == 400 && json::from_str::<Status>(body).is_err() {
        return Err(
            "the Trae CN plugin can't check in: update it to a build that supports check-in"
                .to_owned(),
        );
    }
    if !(200..300).contains(&reply.status()) {
        if let Ok(parsed) = json::from_str::<Status>(body)
            && parsed.code != 0
        {
            return Ok(parsed);
        }
        return Err(format!("HTTP {}", reply.status()));
    }
    json::from_str(body).map_err(|error| error.to_string())
}

fn map_refused(code: i64, message: &str, from_claim: bool) -> CheckinResult {
    match code {
        DEVICE_CHECKED => CheckinResult::new(
            CheckinOutcome::Ineligible,
            or_message(message, "this device has checked in today"),
        ),
        SIGNED_OUT => CheckinResult::new(
            CheckinOutcome::Failed,
            "Trae CN's sign-in has expired — sign in again",
        ),
        OWN_APP_ONLY if !from_claim => CheckinResult::new(
            CheckinOutcome::Failed,
            or_message(message, "asked too often"),
        ),
        _ => CheckinResult::new(
            CheckinOutcome::Failed,
            format!("code {code}: {}", or_message(message, "check-in refused")),
        ),
    }
}

fn or_message(message: &str, fallback: &str) -> String {
    if message.trim().is_empty() {
        fallback.to_owned()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use hofund_plugin_api::CheckinOutcome;
    use hofund_plugin_api::PluginError;

    use super::run;
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
    async fn a3b_7_trae_checkin_posts_status_then_claim() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let transport = Mock {
            seen: Arc::clone(&seen),
            responses: vec![
                Reply::from_parts(
                    200,
                    br#"{"code":0,"enable":true,"checked_in":false,"credits":0,"extra_credits":0,"message":""}"#,
                ),
                Reply::from_parts(
                    200,
                    br#"{"code":0,"credits":100,"extra_credits":5,"message":"","checked_in":true,"enable":true}"#,
                ),
            ],
        };
        let result = run(&transport).await.expect("run");
        assert_eq!(result.outcome(), CheckinOutcome::Claimed);
        assert!((result.credit() - 105.0).abs() < f64::EPSILON);
        let urls = seen.lock().expect("lock").clone();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("/status"));
        assert!(urls[1].ends_with("/claim"));
    }
}
