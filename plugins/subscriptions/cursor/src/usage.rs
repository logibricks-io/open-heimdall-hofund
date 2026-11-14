//! Field-14 TurnUsage and dashboard usage-event mapping (R-9c).

use std::time::Duration;

use hofund_plugin_api::{HostContext, ProtocolId, SecretString, Usage};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::sleep;

use crate::connect;
use crate::endpoints::Endpoints;
use crate::proto::TurnUsage;
use crate::run::Limits;
use crate::tokens;

/// Counters claimed from one or more dashboard rows, before inbound mapping.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DashboardTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

/// True when the inbound protocol's prompt total already includes cache-read tokens.
pub(crate) fn input_includes_cache(inbound: ProtocolId) -> bool {
    matches!(
        inbound,
        ProtocolId::OPENAI_CHAT | ProtocolId::OPENAI_RESPONSES | ProtocolId::GENERATIVE_LANGUAGE
    )
}

/// Maps a Run field-14 TurnUsage onto [`Usage`] for `inbound`.
///
/// `uncached = max(inp - cr - cw, 0)`. `input_tokens` is `uncached + cr` when
/// the inbound protocol includes cache reads in the prompt total, otherwise
/// `uncached`. Cache-write tokens are never added back into `input_tokens`.
pub(crate) fn from_turn(turn: &TurnUsage, inbound: ProtocolId) -> Usage {
    let uncached = turn
        .input
        .saturating_sub(turn.cache_read)
        .saturating_sub(turn.cache_write);
    map_counters(
        uncached,
        turn.output,
        turn.cache_read,
        turn.cache_write,
        turn.reasoning,
        inbound,
    )
}

/// Maps summed dashboard counters (where `input` is already uncached).
pub(crate) fn from_dashboard(totals: DashboardTotals, inbound: ProtocolId) -> Usage {
    map_counters(
        totals.input,
        totals.output,
        totals.cache_read,
        totals.cache_write,
        totals.reasoning,
        inbound,
    )
}

/// Zeros used when the dashboard poll misses the deadline.
pub(crate) fn zeros() -> Usage {
    Usage {
        input_tokens: Some(0),
        output_tokens: Some(0),
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        reasoning_tokens: Some(0),
        ..Usage::default()
    }
}

fn map_counters(
    uncached: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    inbound: ProtocolId,
) -> Usage {
    let input = if input_includes_cache(inbound) {
        uncached.saturating_add(cache_read)
    } else {
        uncached
    };
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_read_tokens: Some(cache_read),
        cache_write_tokens: Some(cache_write),
        reasoning_tokens: Some(reasoning),
        ..Usage::default()
    }
}

/// Inputs for one GetFilteredUsageEvents poll loop.
pub(crate) struct PollArgs<'a> {
    pub host: &'a HostContext,
    pub endpoints: &'a Endpoints,
    pub token: &'a SecretString,
    pub conversation_id: &'a str,
    pub team_id: Option<&'a str>,
    pub since_ms: i64,
    pub inbound: ProtocolId,
    pub limits: Limits,
}

/// Waits, then polls the dashboard until a matching row appears or the budget expires.
///
/// Returns [`None`] on a miss so the caller can emit zeros without estimating.
pub(crate) async fn poll_dashboard(args: PollArgs<'_>) -> Option<Usage> {
    let started = args.host.clock().monotonic();
    sleep(args.limits.usage_first_wait).await;
    loop {
        let elapsed = args.host.clock().monotonic().saturating_sub(started);
        let remaining = args.limits.usage_give_up.saturating_sub(elapsed);
        if remaining.is_zero() {
            return None;
        }
        let budget = args.limits.usage_request.min(remaining);
        if let Some(totals) = one_poll(&args, budget).await {
            return Some(from_dashboard(totals, args.inbound));
        }
        if args.host.clock().monotonic().saturating_sub(started) >= args.limits.usage_give_up {
            return None;
        }
        sleep(args.limits.usage_interval).await;
    }
}

async fn one_poll(args: &PollArgs<'_>, budget: Duration) -> Option<DashboardTotals> {
    let now_ms = tokens::unix_ms(args.host.clock().now());
    let start = args.since_ms.saturating_sub(60_000);
    let end = now_ms.saturating_add(60_000);
    let mut body = json!({
        "startDate": start.to_string(),
        "endDate": end.to_string(),
        "page": 1,
        "pageSize": 50,
    });
    if let Some(team) = args.team_id
        && let Ok(id) = team.parse::<u64>()
    {
        body["teamId"] = json!(id);
    }
    let reply = connect::unary_opts(
        args.host,
        args.endpoints,
        "GetFilteredUsageEvents",
        args.token,
        &body.to_string(),
        args.team_id,
        Some(budget),
    )
    .await
    .ok()?;
    claim_rows(&reply, args.conversation_id, args.since_ms)
}

/// Sums events whose conversation id matches and whose timestamp is at least `since_ms - 1000`.
pub(crate) fn claim_rows(
    body: &Value,
    conversation_id: &str,
    since_ms: i64,
) -> Option<DashboardTotals> {
    let events = events_of(body);
    let floor = since_ms.saturating_sub(1000);
    let mut totals = DashboardTotals::default();
    let mut claimed = 0u32;
    for event in events {
        if conversation_of(event) != Some(conversation_id) {
            continue;
        }
        if timestamp_of(event).is_none_or(|ts| ts < floor) {
            continue;
        }
        totals.input = totals
            .input
            .saturating_add(number_of(event, &["inputTokens", "input_tokens"]));
        totals.output = totals
            .output
            .saturating_add(number_of(event, &["outputTokens", "output_tokens"]));
        totals.cache_read = totals
            .cache_read
            .saturating_add(number_of(event, &["cacheReadTokens", "cache_read_tokens"]));
        totals.cache_write = totals.cache_write.saturating_add(number_of(
            event,
            &["cacheWriteTokens", "cache_write_tokens"],
        ));
        totals.reasoning = totals.reasoning.saturating_add(number_of(
            event,
            &[
                "thinkingTokens",
                "reasoningTokens",
                "thinking_tokens",
                "reasoning_tokens",
            ],
        ));
        claimed += 1;
    }
    (claimed > 0).then_some(totals)
}

pub(crate) fn events_of(body: &Value) -> &[Value] {
    body.as_array()
        .map(|items| items.as_slice())
        .or_else(|| {
            [
                "usageEvents",
                "events",
                "filteredUsageEvents",
                "usage_events",
            ]
            .into_iter()
            .find_map(|key| body.get(key).and_then(Value::as_array).map(Vec::as_slice))
        })
        .unwrap_or(&[])
}

pub(crate) fn conversation_of(event: &Value) -> Option<&str> {
    event
        .get("conversationId")
        .or_else(|| event.get("conversation_id"))
        .and_then(Value::as_str)
}

pub(crate) fn timestamp_of(event: &Value) -> Option<i64> {
    let value = event.get("timestamp").or_else(|| event.get("time"))?;
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    if let Some(number) = value.as_u64() {
        return i64::try_from(number).ok();
    }
    value.as_str()?.trim().parse().ok()
}

pub(crate) fn number_of(event: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| {
            let value = event.get(*key)?;
            if let Some(number) = value.as_u64() {
                return Some(number);
            }
            if let Some(number) = value.as_i64().and_then(|n| u64::try_from(n).ok()) {
                return Some(number);
            }
            value.as_str()?.trim().parse().ok()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_sdk::json::json;

    const TURN: TurnUsage = TurnUsage {
        input: 100,
        output: 20,
        cache_read: 30,
        cache_write: 40,
        reasoning: 5,
    };

    fn protocols() -> [(ProtocolId, u64); 5] {
        [
            (ProtocolId::OPENAI_CHAT, 60),
            (ProtocolId::OPENAI_RESPONSES, 60),
            (ProtocolId::GENERATIVE_LANGUAGE, 60),
            (ProtocolId::ANTHROPIC_MESSAGES, 30),
            (ProtocolId::BEDROCK_CONVERSE, 30),
        ]
    }

    #[test]
    fn field14_follows_inbound_input_includes_cache() {
        for (protocol, expected) in protocols() {
            let usage = from_turn(&TURN, protocol);
            assert_eq!(usage.input_tokens, Some(expected), "{protocol:?}");
            assert_eq!(usage.output_tokens, Some(20));
            assert_eq!(usage.cache_read_tokens, Some(30));
            assert_eq!(usage.cache_write_tokens, Some(40));
            assert_eq!(usage.reasoning_tokens, Some(5));
            assert_ne!(
                usage.input_tokens,
                Some(TURN.input),
                "must not set input_tokens = uncached+cr+cw while filling cache fields"
            );
        }
    }

    #[test]
    fn dashboard_input_is_already_uncached() {
        let totals = DashboardTotals {
            input: 30,
            output: 8,
            cache_read: 12,
            cache_write: 7,
            reasoning: 3,
        };
        for (protocol, expected) in [
            (ProtocolId::OPENAI_CHAT, 42),
            (ProtocolId::OPENAI_RESPONSES, 42),
            (ProtocolId::GENERATIVE_LANGUAGE, 42),
            (ProtocolId::ANTHROPIC_MESSAGES, 30),
            (ProtocolId::BEDROCK_CONVERSE, 30),
        ] {
            let usage = from_dashboard(totals, protocol);
            assert_eq!(usage.input_tokens, Some(expected), "{protocol:?}");
            assert_eq!(usage.output_tokens, Some(8));
            assert_eq!(usage.cache_read_tokens, Some(12));
            assert_eq!(usage.cache_write_tokens, Some(7));
            assert_eq!(usage.reasoning_tokens, Some(3));
        }
    }

    #[test]
    fn claim_rows_sums_matching_events_and_skips_the_rest() {
        let body = json!({
            "usageEvents": [
                {
                    "conversationId": "conv-a",
                    "timestamp": 1000,
                    "inputTokens": 10,
                    "outputTokens": 2,
                    "cacheReadTokens": 3,
                    "cacheWriteTokens": 4,
                    "thinkingTokens": 1,
                },
                {
                    "conversationId": "conv-a",
                    "timestamp": 1500,
                    "inputTokens": 5,
                    "outputTokens": 1,
                    "cacheReadTokens": 1,
                    "cacheWriteTokens": 0,
                    "reasoningTokens": 2,
                },
                {
                    "conversationId": "other",
                    "timestamp": 2000,
                    "inputTokens": 99,
                },
                {
                    "conversationId": "conv-a",
                    "timestamp": 0,
                    "inputTokens": 99,
                },
            ]
        });
        let totals = claim_rows(&body, "conv-a", 1500).expect("claimed");
        assert_eq!(
            totals,
            DashboardTotals {
                input: 15,
                output: 3,
                cache_read: 4,
                cache_write: 4,
                reasoning: 3,
            }
        );
        assert!(claim_rows(&body, "missing", 1500).is_none());
    }

    #[test]
    fn zeros_are_explicit_counters() {
        assert_eq!(zeros().input_tokens, Some(0));
        assert_eq!(zeros().reasoning_tokens, Some(0));
    }
}
