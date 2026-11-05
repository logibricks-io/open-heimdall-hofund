//! Credit-funded rate-limit window resets from `/wham/rate-limit-reset-credits`.

use hofund_plugin_api::{
    HostContext, PluginError, QuotaResetCard, QuotaResetCredits, QuotaResetOutcome,
};
use hofund_plugin_sdk::json::{self, Value};

use crate::tokens::Access;
use crate::upstream::account_call;
use crate::{Endpoints, usage};

/// Reads reset credits for the account: count from `/wham/usage`, details
/// from `/wham/rate-limit-reset-credits` when the count is positive.
pub(crate) async fn read(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &Access,
) -> Result<Option<QuotaResetCredits>, PluginError> {
    let reply = account_call(usage::url(endpoints), access)
        .fetch(host.http())
        .await?;
    if reply.status() != 200 {
        return Err(PluginError::status_error(
            reply.status(),
            "ChatGPT did not read the account's usage",
        ));
    }
    let count = reset_count(reply.body());
    if count == 0 {
        return Ok(None);
    }
    let mut out = QuotaResetCredits::new(count);
    match account_call(reset_credits_url(endpoints), access)
        .fetch(host.http())
        .await
    {
        Ok(reply) if reply.status() == 200 => {
            if let Some(wire) = parse_reset_credits(reply.body()) {
                out = out.with_until(wire.until).with_each(wire.each);
            }
        }
        _ => out = out.with_until_unread(true),
    }
    Ok(Some(out))
}

/// Spends one reset credit. `credit_id` names the credit; empty spends the
/// one that runs out first. Only call this from an explicit user action.
pub(crate) async fn consume(
    host: &HostContext,
    endpoints: &Endpoints,
    access: &Access,
    credit_id: Option<&str>,
) -> Result<QuotaResetOutcome, PluginError> {
    let mut credit = credit_id.unwrap_or_default().trim().to_owned();
    if credit.is_empty() {
        let list = account_call(reset_credits_url(endpoints), access)
            .fetch(host.http())
            .await?;
        if list.status() != 200 {
            return Err(PluginError::transport(
                "could not read which reset runs out first, so none was used",
            ));
        }
        let Some(wire) = parse_reset_credits(list.body()) else {
            return Err(PluginError::decode(
                "ChatGPT reset credits reply is not JSON",
            ));
        };
        credit = wire.first_id.unwrap_or_default();
        if credit.is_empty() {
            return Ok(QuotaResetOutcome::new("no_credit"));
        }
    } else {
        let list = account_call(reset_credits_url(endpoints), access)
            .fetch(host.http())
            .await?;
        if list.status() != 200 {
            return Err(PluginError::transport(
                "could not read reset credits before spending one",
            ));
        }
        let Some(wire) = parse_reset_credits(list.body()) else {
            return Err(PluginError::decode(
                "ChatGPT reset credits reply is not JSON",
            ));
        };
        if !wire.held(&credit) {
            return Err(PluginError::quota(
                "that reset is no longer on the account, so none was used",
            ));
        }
    }
    let redeem =
        hofund_plugin_sdk::uuid::v4().map_err(|_| PluginError::internal("reset redeem id"))?;
    let body = json::to_vec(&json::json!({
        "credit_id": credit,
        "redeem_request_id": redeem,
    }))
    .map_err(|_| PluginError::internal("reset consume body"))?;
    let reply = account_call(consume_url(endpoints), access)
        .header("content-type", "application/json")
        .bytes(body)
        .fetch(host.http())
        .await?;
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "ChatGPT did not accept the reset",
        ));
    }
    parse_consume_outcome(reply.body())
}

fn reset_count(body: &[u8]) -> u32 {
    json::from_slice::<Value>(body)
        .ok()
        .and_then(|usage| {
            usage
                .pointer("/rate_limit_reset_credits/available_count")
                .and_then(Value::as_i64)
        })
        .filter(|count| *count > 0)
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or(0)
}

fn reset_credits_url(endpoints: &Endpoints) -> String {
    let base = endpoints.base.trim_end_matches('/');
    let base = base.strip_suffix("/codex").unwrap_or(base);
    format!("{base}/wham/rate-limit-reset-credits")
}

fn consume_url(endpoints: &Endpoints) -> String {
    format!("{}/consume", reset_credits_url(endpoints))
}

struct ResetCreditsWire {
    first_id: Option<String>,
    until: Option<std::time::SystemTime>,
    each: Vec<QuotaResetCard>,
}

impl ResetCreditsWire {
    fn held(&self, credit: &str) -> bool {
        self.each.iter().any(|card| card.id() == credit)
    }
}

fn parse_reset_credits(body: &[u8]) -> Option<ResetCreditsWire> {
    let wire: Value = json::from_slice(body).ok()?;
    let credits = wire.get("credits")?.as_array()?;
    let mut each = Vec::new();
    let mut first_id = None;
    let mut first_until = None;
    for entry in credits {
        if entry.get("status").and_then(Value::as_str) != Some("available") {
            continue;
        }
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())?;
        let until = entry
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(hofund_plugin_sdk::time::parse_rfc3339);
        if first_id.is_none()
            || until.is_some_and(|at| first_until.is_none_or(|current| at < current))
        {
            first_id = Some(id.to_owned());
            first_until = until;
        }
        each.push(QuotaResetCard::new(id.to_owned()).with_expires_at(until));
    }
    each.sort_by(
        |left, right| match (left.expires_at(), right.expires_at()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(left), Some(right)) => left.cmp(&right),
        },
    );
    Some(ResetCreditsWire {
        first_id,
        until: first_until,
        each,
    })
}

fn parse_consume_outcome(body: &[u8]) -> Result<QuotaResetOutcome, PluginError> {
    let wire: Value = json::from_slice(body)
        .map_err(|_| PluginError::decode("ChatGPT reset: an answer that says nothing"))?;
    let code = wire
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .ok_or_else(|| PluginError::decode("ChatGPT reset: an answer that says nothing"))?;
    let windows = wire
        .get("windows_reset")
        .and_then(Value::as_i64)
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or(0);
    Ok(QuotaResetOutcome::new(code).with_windows(windows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r47a_k_reset_count_from_usage_page() {
        let body = br#"{"rate_limit_reset_credits":{"available_count":2}}"#;
        assert_eq!(reset_count(body), 2);
        assert_eq!(
            reset_count(br#"{"rate_limit_reset_credits":{"available_count":0}}"#),
            0
        );
    }

    #[test]
    fn r47a_k_picks_soonest_reset_credit() {
        let body = br#"{"credits":[
            {"id":"late","status":"available","expires_at":"2030-01-01T00:00:00Z"},
            {"id":"soon","status":"available","expires_at":"2029-06-01T00:00:00Z"},
            {"id":"spent","status":"used","expires_at":"2029-01-01T00:00:00Z"}
        ]}"#;
        let wire = parse_reset_credits(body).expect("parse");
        assert_eq!(wire.first_id.as_deref(), Some("soon"));
        assert_eq!(wire.each.len(), 2);
        assert!(wire.held("soon"));
        assert!(!wire.held("spent"));
    }
}
