//! Daily claims for gift plans offered by the Z Code marketing feed.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, CheckinOutcome, CheckinResult, HostContext, PluginError, SecretString,
};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{TokenCache, secret_text};

use crate::session::{self, Stored};
use crate::{APP_VERSION, Endpoints, api, gift, start};

const RECHECK: Duration = Duration::from_secs(30 * 60);
const CLAIM_ACTION: &str = "claim_zcode_plan";

pub(crate) async fn run(
    account: &AccountId,
    host: &HostContext,
    endpoints: Arc<Endpoints>,
    device_file: &Path,
    team_keys: &TokenCache<SecretString>,
    touch_seq: &AtomicU64,
) -> Result<CheckinResult, PluginError> {
    let stored = session::load(host, &endpoints, account, device_file, team_keys).await?;
    let Some(jwt) = stored.auth.jwt.as_ref() else {
        return Ok(CheckinResult::new(
            CheckinOutcome::Ineligible,
            "Z Code gift plans are claimed with a Z Code sign-in; this account has none",
        ));
    };
    if let Err(error) = gift::ensure_live_jwt(host, jwt) {
        return Ok(CheckinResult::new(CheckinOutcome::Failed, error.message()));
    }
    let plans = match marketing_plans(host, &endpoints, &stored, device_file, touch_seq).await {
        Ok(plans) => plans,
        Err(error) => {
            return Ok(CheckinResult::new(CheckinOutcome::Failed, error.message()));
        }
    };
    if plans.is_empty() {
        return Ok(
            CheckinResult::new(CheckinOutcome::Inactive, "no Z Code gift plan to claim")
                .with_recheck_at(host.clock().now() + RECHECK),
        );
    }
    let mut results = Vec::new();
    for plan in plans {
        let result = claim_one(host, &endpoints, &stored, &plan.id).await;
        results.push((plan.name, result));
    }
    Ok(summarize(host, results))
}

struct Offer {
    id: String,
    name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaimOutcome {
    Claimed,
    Done,
    Captcha,
    Ineligible,
    Failed,
}

async fn marketing_plans(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
    device_file: &Path,
    touch_seq: &AtomicU64,
) -> Result<Vec<Offer>, PluginError> {
    let jwt = stored.auth.jwt.as_ref().expect("checked jwt");
    let authorization = SecretString::from(format!("Bearer {}", secret_text(jwt)));
    let seq = touch_seq.fetch_add(1, Ordering::Relaxed);
    let url = format!(
        "{}/api/v1/marketing/touch?seq={seq}",
        endpoints.portal.trim_end_matches('/')
    );
    let headers = marketing_headers(stored);
    let refs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let data = api::get_envelope(&api::EnvelopeRequest::get(
        host,
        endpoints,
        &url,
        Some(&authorization),
        &refs,
        device_file,
    ))
    .await?;
    parse_offers(&data)
}

fn parse_offers(data: &Value) -> Result<Vec<Offer>, PluginError> {
    if data.is_null() {
        return Ok(Vec::new());
    }
    let Some(deliveries) = data.get("deliveries") else {
        return Ok(Vec::new());
    };
    let Some(deliveries) = deliveries.as_array() else {
        return Err(PluginError::decode(
            "the Z Code marketing plan list is unreadable",
        ));
    };
    let mut offers: Vec<Offer> = Vec::new();
    for delivery in deliveries {
        let mut ids = Vec::new();
        let mut names = Vec::new();
        walk_delivery(delivery, 0, &mut ids, &mut names);
        for id in ids {
            if offers.iter().any(|offer| offer.id == id) {
                continue;
            }
            offers.push(Offer {
                name: names.first().cloned().unwrap_or_else(|| id.clone()),
                id,
            });
        }
    }
    Ok(offers)
}

fn walk_delivery(value: &Value, depth: usize, ids: &mut Vec<String>, names: &mut Vec<String>) {
    if depth > 12 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                walk_delivery(item, depth + 1, ids, names);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some(CLAIM_ACTION)
                && let Some(id) = object
                    .get("args")
                    .and_then(|args| args.get("plan_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
            {
                ids.push(id.to_owned());
            }
            if let Some(name) = object
                .get("zcode_plan")
                .and_then(|plan| plan.get("name"))
                .and_then(Value::as_str)
            {
                names.push(name.to_owned());
            }
            for child in object.values() {
                walk_delivery(child, depth + 1, ids, names);
            }
        }
        _ => {}
    }
}

async fn claim_one(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
    plan_id: &str,
) -> (ClaimOutcome, String) {
    let jwt = stored.auth.jwt.as_ref().expect("checked jwt");
    let authorization = SecretString::from(format!("Bearer {}", secret_text(jwt)));
    let url = format!(
        "{}/api/v1/zcode-plan/billing/claim",
        endpoints.portal.trim_end_matches('/')
    );
    let body = json::json!({"plan_id": plan_id});
    let Ok(bytes) = json::to_vec(&body) else {
        return (ClaimOutcome::Failed, "claim body is not JSON".to_owned());
    };
    let mut call = Call::post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("user-agent", format!("ZCode/{APP_VERSION}"))
        .sensitive_header("authorization", secret_text(&authorization))
        .bytes(bytes);
    for (name, value) in marketing_headers(stored) {
        call = call.header(name, value);
    }
    let reply = match call.fetch(host.http()).await {
        Ok(reply) => reply,
        Err(error) => return (ClaimOutcome::Failed, error.message().to_owned()),
    };
    let Ok(root) = json::from_slice::<Value>(reply.body()) else {
        return (
            ClaimOutcome::Failed,
            format!("HTTP {}, unreadable claim reply", reply.status()),
        );
    };
    let Some(code) = root.get("code").and_then(json_i64) else {
        return (
            ClaimOutcome::Failed,
            format!("HTTP {}, unreadable claim reply", reply.status()),
        );
    };
    let msg = root
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if code == 3007 || msg.to_ascii_lowercase().contains("captcha") {
        return (
            ClaimOutcome::Captcha,
            if msg.is_empty() {
                format!("code {code}")
            } else {
                msg
            },
        );
    }
    if code == 1003 {
        return (ClaimOutcome::Done, String::new());
    }
    if code == 1004 {
        return (
            ClaimOutcome::Ineligible,
            if msg.is_empty() {
                format!("code {code}")
            } else {
                msg
            },
        );
    }
    if code == 0 && (200..300).contains(&reply.status()) {
        return (ClaimOutcome::Claimed, String::new());
    }
    (
        ClaimOutcome::Failed,
        format!(
            "{} (HTTP {}, code {code})",
            if msg.is_empty() { "claim failed" } else { &msg },
            reply.status()
        ),
    )
}

fn summarize(host: &HostContext, results: Vec<(String, (ClaimOutcome, String))>) -> CheckinResult {
    let names = |outcome| {
        results
            .iter()
            .filter(|(_, (actual, _))| *actual == outcome)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    };
    let claimed = names(ClaimOutcome::Claimed);
    let mut messages = Vec::new();
    if !claimed.is_empty() {
        messages.push(format!("claimed {}", claimed.join(", ")));
    }
    for (name, (outcome, why)) in &results {
        match outcome {
            ClaimOutcome::Captcha => {
                messages.push(format!("{name}: claim it in the Z Code app ({why})"));
            }
            ClaimOutcome::Ineligible | ClaimOutcome::Failed => {
                messages.push(format!("{name}: {why}"));
            }
            _ => {}
        }
    }
    let message = messages.join("; ");
    if results
        .iter()
        .any(|(_, (outcome, _))| *outcome == ClaimOutcome::Failed)
    {
        return CheckinResult::new(CheckinOutcome::Failed, message);
    }
    if results
        .iter()
        .any(|(_, (outcome, _))| *outcome == ClaimOutcome::Captcha)
    {
        return CheckinResult::new(CheckinOutcome::Captcha, message);
    }
    if !claimed.is_empty() {
        return CheckinResult::new(CheckinOutcome::Claimed, message);
    }
    if results
        .iter()
        .any(|(_, (outcome, _))| *outcome == ClaimOutcome::Ineligible)
    {
        return CheckinResult::new(CheckinOutcome::Ineligible, message)
            .with_recheck_at(host.clock().now() + RECHECK);
    }
    CheckinResult::new(
        CheckinOutcome::Done,
        format!("already claimed: {}", names(ClaimOutcome::Done).join(", ")),
    )
}

fn marketing_headers(stored: &Stored) -> Vec<(&'static str, String)> {
    vec![
        ("x-device-mid", stored.device.clone()),
        ("x-zcode-app-version", APP_VERSION.to_owned()),
        ("x-platform", start::platform()),
        ("x-client-language", start::marketing_language()),
    ]
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r57b_offer_parser_requires_a_claim_action() {
        let data = json::json!({"deliveries": [{"banner": {
            "background": {"args": {"zcode_plan": {"plan_id": "plan-1", "name": "Weekend"}}},
            "buttons": [{"action": {"type": "close", "args": {}}}]
        }}]});
        assert!(parse_offers(&data).expect("offers").is_empty());
        let data = json::json!({"deliveries": [{"banner": {
            "background": {"args": {"zcode_plan": {"plan_id": "plan-1", "name": "Weekend"}}},
            "buttons": [{"action": {"type": "claim_zcode_plan", "args": {"plan_id": "plan-1"}}}]
        }}]});
        let offers = parse_offers(&data).expect("offers");
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].id, "plan-1");
        assert_eq!(offers[0].name, "Weekend");
    }
}
