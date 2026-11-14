//! Credit-funded rate-limit window resets from `/wham/rate-limit-reset-credits`.

use std::collections::HashMap;
use std::sync::Mutex;

use hofund_plugin_api::{
    AccountId, HostContext, PluginError, PluginErrorKind, QuotaResetCard, QuotaResetCredits,
    QuotaResetOutcome,
};
use hofund_plugin_sdk::json::{self, Value};

use crate::tokens::Access;
use crate::upstream::{account_call, account_post};
use crate::{Endpoints, usage};

/// Pending `redeem_request_id` values for in-flight explicit consumes.
pub(crate) type RedeemLedger = Mutex<HashMap<(AccountId, String), String>>;

fn lock_ledger(
    ledger: &RedeemLedger,
) -> std::sync::MutexGuard<'_, HashMap<(AccountId, String), String>> {
    ledger
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    let Some(count) = available_count(reply.body())? else {
        return Ok(None);
    };
    let mut out = QuotaResetCredits::new(count);
    match account_call(reset_credits_url(endpoints), access)
        .fetch(host.http())
        .await
    {
        Ok(reply) if reply.status() == 200 => match parse_reset_credits(reply.body()) {
            Ok(wire) => {
                out = out.with_until(wire.until);
                if wire.each.len() == count as usize {
                    out = out.with_each(wire.each);
                }
            }
            Err(()) => out = out.with_until_unread(true),
        },
        _ => out = out.with_until_unread(true),
    }
    Ok(Some(out))
}

fn redeem_for(
    ledger: &RedeemLedger,
    account: &AccountId,
    credit: &str,
) -> Result<String, PluginError> {
    let key = (account.clone(), credit.to_owned());
    let mut guard = lock_ledger(ledger);
    if let Some(redeem) = guard.get(&key) {
        return Ok(redeem.clone());
    }
    let redeem =
        hofund_plugin_sdk::uuid::v4().map_err(|_| PluginError::internal("reset redeem id"))?;
    guard.insert(key, redeem.clone());
    Ok(redeem)
}

fn finish_redeem(ledger: &RedeemLedger, account: &AccountId, credit: &str) {
    lock_ledger(ledger).remove(&(account.clone(), credit.to_owned()));
}

/// Spends one reset credit. `credit_id` names the credit; empty spends the
/// one that runs out first. Only call this from an explicit user action.
pub(crate) async fn consume(
    host: &HostContext,
    account: &AccountId,
    endpoints: &Endpoints,
    access: &Access,
    credit_id: Option<&str>,
    ledger: &RedeemLedger,
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
        let wire = parse_reset_credits(list.body())
            .map_err(|_| PluginError::decode("ChatGPT reset credits reply is not JSON"))?;
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
        let wire = parse_reset_credits(list.body())
            .map_err(|_| PluginError::decode("ChatGPT reset credits reply is not JSON"))?;
        if !wire.held(&credit) {
            return Err(PluginError::quota(
                "that reset is no longer on the account, so none was used",
            ));
        }
    }
    let redeem = redeem_for(ledger, account, &credit)?;
    let body = json::to_vec(&json::json!({
        "credit_id": credit,
        "redeem_request_id": redeem,
    }))
    .map_err(|_| PluginError::internal("reset consume body"))?;
    let post = || {
        account_post(consume_url(endpoints), access)
            .header("content-type", "application/json")
            .bytes(body.clone())
    };
    let reply = match post().fetch(host.http()).await {
        Ok(reply) => reply,
        Err(error)
            if matches!(
                error.kind(),
                PluginErrorKind::Transport | PluginErrorKind::Connect
            ) =>
        {
            post().fetch(host.http()).await?
        }
        Err(error) => return Err(error),
    };
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "ChatGPT did not accept the reset",
        ));
    }
    let outcome = parse_consume_outcome(reply.body())?;
    finish_redeem(ledger, account, &credit);
    Ok(outcome)
}

/// Reads `available_count` from a usage page body. Only an explicit zero
/// means none; a bad page is a decode error like [`usage::read`].
fn available_count(body: &[u8]) -> Result<Option<u32>, PluginError> {
    let usage: Value = json::from_slice(body)
        .map_err(|_| PluginError::decode("the ChatGPT usage page is not JSON"))?;
    let Some(field) = usage.pointer("/rate_limit_reset_credits/available_count") else {
        return Ok(None);
    };
    let count = field
        .as_i64()
        .ok_or_else(|| PluginError::decode("the ChatGPT usage page is not JSON"))?;
    if count == 0 {
        return Ok(None);
    }
    if count < 0 {
        return Err(PluginError::decode("the ChatGPT usage page is not JSON"));
    }
    u32::try_from(count)
        .map(Some)
        .map_err(|_| PluginError::decode("the ChatGPT usage page is not JSON"))
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

/// Parses a credits list response. Err only when the body is not JSON.
fn parse_reset_credits(body: &[u8]) -> Result<ResetCreditsWire, ()> {
    let wire: Value = json::from_slice(body).map_err(|_| ())?;
    let empty: &[Value] = &[];
    let credits: &[Value] = match wire.get("credits") {
        None => empty,
        Some(Value::Array(entries)) => entries,
        Some(_) => empty,
    };
    let mut each = Vec::new();
    let mut first_id = None;
    let mut first_until = None;
    for entry in credits {
        if entry.get("status").and_then(Value::as_str) != Some("available") {
            continue;
        }
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
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
    Ok(ResetCreditsWire {
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

    fn vendor_refusal(code: &str) -> bool {
        matches!(
            code,
            "nothing_to_reset" | "no_credit" | "already_redeemed" | "already_used"
                | "already_reset"
        )
    }

    fn vendor_spent(code: &str) -> bool {
        code == "reset"
    }

    #[test]
    fn r47a_k2_usage_count_explicit_zero_is_none() {
        assert_eq!(
            available_count(br#"{"rate_limit_reset_credits":{"available_count":0}}"#)
                .expect("parse"),
            None
        );
        assert_eq!(
            available_count(br#"{"rate_limit_reset_credits":{"available_count":2}}"#)
                .expect("parse"),
            Some(2)
        );
    }

    #[test]
    fn r47a_k2_usage_not_json_is_decode_error() {
        let error = available_count(b"<html>").expect_err("not json");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Decode);
    }

    #[test]
    fn r47a_k2_usage_bad_count_is_decode_error() {
        let error = available_count(br#"{"rate_limit_reset_credits":{"available_count":"x"}}"#)
            .expect_err("bad count");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Decode);
    }

    #[test]
    fn r47a_k2_skips_available_credit_with_empty_id() {
        let body = br#"{"credits":[
            {"id":"","status":"available","expires_at":"2030-01-01T00:00:00Z"},
            {"id":"good","status":"available","expires_at":"2029-06-01T00:00:00Z"}
        ]}"#;
        let wire = parse_reset_credits(body).expect("json");
        assert_eq!(wire.first_id.as_deref(), Some("good"));
        assert_eq!(wire.each.len(), 1);
    }

    #[test]
    fn r47a_k2_missing_credits_array_is_empty_not_unread() {
        let wire = parse_reset_credits(br#"{}"#).expect("json");
        assert!(wire.first_id.is_none());
        assert!(wire.each.is_empty());
    }

    #[test]
    fn r47a_k2_non_json_credits_body_is_unreadable() {
        assert!(parse_reset_credits(b"not-json").is_err());
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

    #[test]
    fn r47n_vendor_refusal_codes() {
        for code in [
            "nothing_to_reset",
            "no_credit",
            "already_redeemed",
            "already_used",
            "already_reset",
        ] {
            assert!(vendor_refusal(code), "{code}");
        }
        assert!(!vendor_refusal("reset"));
        assert!(vendor_spent("reset"));
        assert!(!vendor_spent("nothing_to_reset"));
    }

    #[test]
    fn r47n_parse_consume_refusals_on_200() {
        for (code, windows) in [
            ("nothing_to_reset", 0),
            ("already_redeemed", 0),
            ("already_used", 0),
            ("reset", 2),
        ] {
            let body = format!(r#"{{"code":"{code}","windows_reset":{windows}}}"#);
            let outcome = parse_consume_outcome(body.as_bytes()).expect(code);
            assert_eq!(outcome.code(), code);
            assert_eq!(outcome.windows(), windows);
        }
    }
}
