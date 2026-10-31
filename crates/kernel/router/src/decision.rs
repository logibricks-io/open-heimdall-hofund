//! Which upstream failures another member may answer, and which rest nobody.
//!
//! The word lists match the reference's regular expressions one for one
//! (`reference: internal/gateway/fallback.go:1239-1527`,
//! `reference: internal/gateway/gateway.go:8230-8265`,
//! `reference: internal/gateway/routing.go:267`,
//! `reference: internal/catalog/catalog.go:1603-1679`). Each list is read on
//! the error's text and on the strings its JSON body escaped as `\u` (see
//! [`words_of`]). Text outside ASCII is written as `\u{..}` escapes; the
//! comment above each list says what it means.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use hofund_plugin_api::{PluginError, PluginErrorKind};
use regex::Regex;

/// The words a model that generated nothing is told with.
const EMPTY_REPLY: &str = "an empty reply (the model answered nothing; try again)";

/// How far past a model's window an estimate must go to be taken as too long.
const UNSAID_MARGIN: f64 = 1.2;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a failover word list compiles")
}

/// Out of quota or slow down, said without the status for it. The last
/// words are balance, allowance, arrears, throttling, frequency, frequent,
/// pace, plan, usage, and ceiling.
static QUOTA_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)quota|insufficient|balance|credit|billing|out of budget|budget (exceeded|exhausted)|exceeded|rate.?limit|usage.?limit|limit.?reached|hit your .*limit|limit.{0,24}resets|too many requests|overloaded|\u{4f59}\u{989d}|\u{989d}\u{5ea6}|\u{6b20}\u{8d39}|\u{9650}\u{6d41}|\u{9891}\u{7387}|\u{9891}\u{7e41}|\u{901f}\u{7387}|\u{5957}\u{9910}|\u{7528}\u{91cf}|\u{4e0a}\u{9650}",
    )
});

/// The account or key has no money left. The last words are balance,
/// arrears, top up, and account (short or stopped).
static CREDIT_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)insufficient.?(balance|credit|fund)|balance|credit|billing|payment|arrear|overdue|suspended|\u{4f59}\u{989d}|\u{6b20}\u{8d39}|\u{5145}\u{503c}|\u{8d26}\u{6237}.*(\u{4e0d}\u{8db3}|\u{505c})",
    )
});

/// The model is not one this key serves. The last clause is model, then
/// absent, unsupported, unauthorized, or not enabled.
static UNSERVED_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)model.{0,80}(not (supported|accessible|available|found|enabled|allowed)|unsupported|does ?n[o']t exist|unknown|invalid)|(no such|unknown|invalid|unsupported) model|model_not_found|\u{6a21}\u{578b}.{0,12}(\u{4e0d}\u{5b58}\u{5728}|\u{4e0d}\u{652f}\u{6301}|\u{65e0}\u{6743}|\u{672a}\u{5f00}\u{901a})",
    )
});

/// A pooled upstream says only the requested model's credentials cooled down.
static MODEL_COOLING: LazyLock<Regex> =
    LazyLock::new(|| re("(?i)credentials for (?:the requested )?model .{0,200}?cooling down"));

/// The model is retired there, though a list may still name it.
static RETIRED_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)\"ModelDeprecated\"|model_deprecated|model.{0,80}(has been|was|is now|is) (deprecated|retired|discontinued|decommissioned|sunset)",
    )
});

/// The vendor will not take requests from this client at all.
static REFUSED_WORDS: LazyLock<Regex> =
    LazyLock::new(|| re("(?i)unapproved channel|illegal api invocation"));

/// The vendor's API cannot read the request's shape.
static SHAPE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r#"(?i)failed to deserialize|unknown (item |content |input )?(type|variant|field|parameter)|unknown_parameter|unrecognized (request argument|field|parameter)|extra (inputs|fields) are not permitted|additional properties are not allowed|schema exceeds the maximum (nesting )?depth|schema[^"]{0,60}(?-u:\b)nesting depth(?-u:\b)|function_declarations\[[0-9]+\]\.parameters(?-u:\b)"#,
    )
});

/// A request turned away with nothing but a trace id.
static UNSAID_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(r#"(?i)(^|"|\\"|: )invalid request error trace_id: [0-9a-f]{16,}(\\?"|\s*$)"#)
});

/// The vendor does not serve where the request comes from.
static REGION_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)(?-u:\\b)user location is not supported for the api use(?-u:\\b)|unsupported_country_region_territory|country, region,? or territory not supported",
    )
});

/// The same history fails the same on every account of the backend.
static PROTECTION_WORDS: LazyLock<Regex> =
    LazyLock::new(|| re("(?i)response protection is unavailable"));

/// No zero-data-retention upstream serves the model.
static ZDR_CODED: LazyLock<Regex> =
    LazyLock::new(|| re(r#"\\?"(type|code)\\?"\s*:\s*\\?"cmd_zdr_no_providers\\?""#));

/// The vendor does not take custom tools.
static CUSTOM_TOOL_REFUSAL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)unsupported custom tool|custom tools? (?:is |are )?not (?:supported|allowed)|(?:unsupported|unknown|invalid) tool type[^\\n{}]{0,20}?['\"`]custom['\"`]|tool type ['\"`]custom['\"`] (?:is )?(?:not supported|unsupported)",
    )
});

/// A firewall's block page rather than the vendor's API answering.
static EDGE_BLOCKED: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)request has been blocked|errors\\.aliyun\\.com|you have been blocked|used cloudflare to restrict access|error code: 10(0[5-9]|1[0-2]|20)(?-u:\\b)",
    )
});

/// A firewall block, already said in plain words.
const BLOCKED_HINT: &str = "the provider's network firewall blocked requests from this IP; wait a while, or switch to another network or proxy";

/// The conversation no longer fits the model. The Chinese clauses name the
/// context, or what was sent (content, input, text, prompt, question,
/// conversation, message), beside its length, tokens, or character count.
static TOO_LONG: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        "(?i)context_length_exceeded|prompt is too long|input is too long|(exceeds?|exceeded|over|beyond)( the)?( model'?s?)?( maximum)? context|context (length|limit|window) (exceeded|is exceeded)|maximum context length|too many (input |prompt )?tokens|\u{4e0a}\u{4e0b}\u{6587}(\u{957f}\u{5ea6})?(\u{8d85}|\u{8fc7}\u{957f})|\u{8d85}(\u{8fc7}|\u{51fa})(\u{4e86})?(\u{6a21}\u{578b})?(\u{7684})?(\u{6700}\u{5927})?\u{4e0a}\u{4e0b}\u{6587}",
        "|input token count.*exceeds the maximum|maximum prompt length is [0-9]|reduce the length of the messages|maximum allowed input length|longer than the model'?s context length|available context size|greater than the context length|prompt token count of [0-9,]+ exceeds the limit|context window exceeds limit|exceeded model token limit|configured context size|prompt too long|prompt exceeds max length|context_window_exceeded|exceeded (max |maximum )?context length|range of input length should be|token limit exceeded|input tokens? (count )?(exceeds?|exceeded) (the )?(max(imum)? )?limit",
        "|(\u{5185}\u{5bb9}|\u{8f93}\u{5165}|\u{6587}\u{672c}|\u{63d0}\u{793a}\u{8bcd}?|prompt|\u{95ee}\u{9898}|\u{5bf9}\u{8bdd}|\u{6d88}\u{606f})(\u{5185}\u{5bb9}|\u{6587}\u{672c})?(\u{957f}\u{5ea6})?\\s*(\u{592a}\u{957f}|\u{8fc7}\u{957f}|\u{8d85}\u{957f})|\u{5b57}\u{6570}(\u{592a}\u{591a}|\u{8fc7}\u{591a})",
        "|(\u{5185}\u{5bb9}|\u{8f93}\u{5165}|\u{6587}\u{672c}|\u{63d0}\u{793a}\u{8bcd}?|prompt|\u{95ee}\u{9898}|\u{5bf9}\u{8bdd}|\u{6d88}\u{606f})[^\u{ff0c}\u{3002},.;\u{ff1b}!\u{ff01}]{0,12}\u{8d85}(\u{8fc7}|\u{51fa})[^\u{ff0c}\u{3002},.;\u{ff1b}!\u{ff01}]{0,12}(\u{957f}\u{5ea6}|token|\u{4ee4}\u{724c}|\u{5b57}\u{6570})",
        "|(\u{8f93}\u{5165}|\u{63d0}\u{793a}\u{8bcd}?|prompt)\\s*\u{7684}?\\s*(\u{957f}\u{5ea6}|token|\u{4ee4}\u{724c}|\u{5b57}\u{6570})[^\u{ff0c}\u{3002},.;\u{ff1b}!\u{ff01}]{0,8}\u{8d85}(\u{8fc7}|\u{51fa})",
    ))
});

/// A limit on how fast or how much, not on one prompt's size. The Chinese
/// words are pace, frequent, frequency, throttling, concurrency, per
/// (second, minute, hour, day), and try again later.
static PACE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    re(
        "(?i)rate.?limit|per (minute|hour|day)|(?-u:\\b)TP[MD](?-u:\\b)|please wait|try again later|throttl|\u{901f}\u{7387}|\u{9891}\u{7e41}|\u{9891}\u{7387}|\u{9650}\u{6d41}|\u{5e76}\u{53d1}|\u{6bcf}(\u{79d2}|\u{5206}\u{949f}|\u{5c0f}\u{65f6}|\u{5929}|\u{65e5})|\u{7a0d}\u{540e}(\u{518d}\u{8bd5}|\u{91cd}\u{8bd5})",
    )
});

/// A vendor usage-policy code.
static POLICY_CODE: LazyLock<Regex> = LazyLock::new(|| re("^[a-z]+_policy$"));

/// Words that hold a prompt to a usage policy.
static FLAGGED_WORDS: LazyLock<Regex> = LazyLock::new(|| re("(?i)flagged|usage polic"));

/// Safety-filter codes that refuse the request, not the account.
const FILTER_CODES: &[&str] = &[
    "refusal",
    "content_filter",
    "content_policy_violation",
    "content_policy_rejected",
    "SAFETY",
    "PROHIBITED_CONTENT",
    "BLOCKLIST",
    "SPII",
    "IMAGE_SAFETY",
];

/// A refusal's whole message when no code comes with it.
const HIGH_RISK_REFUSAL: &str = "The request was rejected because it was considered high risk";

/// The model is served on another API of this vendor. The last two phrases
/// say the model is not served on Chat, and to use Responses instead.
const WRONG_ENDPOINT: &[&str] = &[
    "not a chat model",
    "not supported in the v1/chat/completions",
    "not supported in /v1/chat/completions",
    "not supported in the v1/responses",
    "not supported in /v1/responses",
    "only supported in v1/responses",
    "only supported in /v1/responses",
    "use v1/completions",
    "use /v1/completions",
    "use v1/responses",
    "use /v1/responses",
    "use v1/chat/completions",
    "use /v1/chat/completions",
    "not accessible via the",
    "unsupported_api_for_model",
    "no model endpoints available given user constraints",
    "is not supported for format",
    "does not support this protocol",
    "requires responses",
    "\u{4e0d}\u{652f}\u{6301} /v1/chat/completions \u{63a5}\u{53e3}",
    "\u{6539}\u{7528} /v1/responses",
];

/// Whether `error`, before any byte reached the client, may go to the next
/// member (`reference: internal/gateway/fallback.go:1487-1517`). A transport
/// failure answers as a 502 does; overflow is ruled out before the status.
pub(crate) fn allows(error: &PluginError, connect_errors: bool, statuses: &BTreeSet<u16>) -> bool {
    match error.kind() {
        PluginErrorKind::Connect
        | PluginErrorKind::Timeout
        | PluginErrorKind::Transport
        | PluginErrorKind::Interrupted => return connect_errors,
        PluginErrorKind::Status | PluginErrorKind::Quota => {}
        _ => return false,
    }
    let Some(status) = error.status() else {
        return false;
    };
    let text = words_of(error.message());
    if overflowed(status, &text) {
        return false;
    }
    if statuses.contains(&status)
        || model_retired(status, error.message(), &text)
        || status >= 400 && edge_blocked(&text)
        || error.is_credit() && (400..500).contains(&status)
    {
        return true;
    }
    matches!(status, 400 | 422)
        && (QUOTA_WORDS.is_match(&text)
            || UNSERVED_WORDS.is_match(&text)
            || REFUSED_WORDS.is_match(&text)
            || SHAPE_WORDS.is_match(&text)
            || UNSAID_WORDS.is_match(&text)
            || REGION_WORDS.is_match(&text)
            || wrong_endpoint(status, &text))
}

/// Whether the request was too long for this member's model, said by the
/// vendor (an overflow, or a 413 byte cap). Another member, not known to be
/// too small, may still hold it; nobody rests.
pub(crate) fn outgrown(error: &PluginError) -> bool {
    error
        .status()
        .is_some_and(|status| status == 413 || overflowed(status, &words_of(error.message())))
}

/// Whether `error` says the vendor refuses the request's content on every
/// account of its provider, so none of them is asked it again
/// (`reference: internal/gateway/fallback.go:1425`).
pub(crate) fn skips_provider(error: &PluginError) -> bool {
    error
        .status()
        .is_some_and(|status| status >= 400 && PROTECTION_WORDS.is_match(error.message()))
}

/// Whether `error` is a request this vendor's API cannot read, which another
/// vendor may, so the next member is asked whatever `failover.statuses` says
/// (`reference: internal/gateway/gateway.go:3542`).
pub(crate) fn asks_next(error: &PluginError) -> bool {
    error
        .status()
        .is_some_and(|status| shape_refused(status, error.message()))
}

/// Whether a failure rests the member. A reply that broke off after it began
/// rests only for the vendor's faults (`reference:
/// internal/gateway/fallback.go:1525-1527`); before a byte, a request the
/// vendor could not read, one too long for it, or one it refused for its
/// content rests nobody either (`reference: internal/gateway/gateway.go:3475-3600`).
pub(crate) fn rests(error: &PluginError) -> bool {
    let message = error.message();
    if too_long(400, message)
        || REFUSED_WORDS.is_match(message)
        || PROTECTION_WORDS.is_match(message)
        || message.contains(EMPTY_REPLY)
    {
        return false;
    }
    error
        .status()
        .is_none_or(|status| status != 413 && !shape_refused(status, message) && !outgrown(error))
}

/// Whether a refusal says only the requested model's pooled credentials
/// cooled. The caller scopes it to the model only when the refusal is a rate
/// limit, exhausted quota, or credit refusal (`reference:
/// internal/gateway/routing.go:1475-1477`, `modelCooledDown`); a bare
/// `model_cooldown` code without the sentence rests the member.
pub(crate) fn model_cooling(error: &PluginError) -> bool {
    MODEL_COOLING.is_match(&words_of(error.message()))
}

/// Whether the member does not serve this model, rather than being out of
/// quota or refused for the request's content (`reference:
/// internal/gateway/fallback.go:1281-1299`).
pub(crate) fn model_unserved(error: &PluginError) -> bool {
    let Some(status) = error.status() else {
        return false;
    };
    if !matches!(status, 400 | 403 | 404 | 422) {
        return false;
    }
    let text = words_of(error.message());
    UNSERVED_WORDS.is_match(&text)
        && !QUOTA_WORDS.is_match(&text)
        && !REFUSED_WORDS.is_match(&text)
        && !ZDR_CODED.is_match(&text)
}

/// Tells a vague failure as the request being too long for the model, when
/// the request's estimate is past `window` by the margin (`reference:
/// internal/gateway/fallback.go:1533-1575`). The returned error does not cool.
pub(crate) fn too_long_unsaid(
    error: &PluginError,
    tokens: u64,
    window: Option<u64>,
    model: &str,
    provider: &str,
) -> Option<PluginError> {
    let status = error.status()?;
    if !matches!(status, 400 | 413 | 422 | 500 | 502 | 503 | 504) {
        return None;
    }
    let text = words_of(error.message());
    if overflowed(status, &text)
        || error.is_credit()
        || error.kind() == PluginErrorKind::Quota
        || QUOTA_WORDS.is_match(&text)
        || UNSERVED_WORDS.is_match(&text)
        || REFUSED_WORDS.is_match(&text)
        || edge_blocked(&text)
        || status >= 400 && REGION_WORDS.is_match(&text)
        || matches!(status, 400 | 422) && ZDR_CODED.is_match(&text)
    {
        return None;
    }
    let window = window.filter(|window| *window > 0)?;
    #[allow(clippy::cast_precision_loss)]
    if (tokens as f64) < (window as f64) * UNSAID_MARGIN {
        return None;
    }
    let message = format!(
        "prompt is too long: about {tokens} tokens, more than {model}'s context window of {window}"
    );
    let mut said = error.message().trim().to_owned();
    if said.len() > 200 {
        let mut end = 200;
        while !said.is_char_boundary(end) {
            end -= 1;
        }
        said.truncate(end);
        said.push('\u{2026}');
    }
    let full = format!("{message} ({provider} answered {status}: {said})");
    let message = if !said.is_empty() && too_long(400, &full) {
        full
    } else {
        format!("{message} ({provider} answered {status})")
    };
    Some(PluginError::status_error(400, message).without_cooldown())
}

fn overflowed(status: u16, text: &str) -> bool {
    matches!(status, 400 | 413 | 422) && too_long(status, text)
}

fn too_long(status: u16, text: &str) -> bool {
    if !(400..500).contains(&status) || status == 429 {
        return false;
    }
    let lower = text.to_lowercase();
    if ["max_tokens", "max_output_tokens", "max_completion_tokens"]
        .into_iter()
        .any(|word| lower.contains(word))
        || PACE_WORDS.is_match(text)
    {
        return false;
    }
    TOO_LONG.is_match(text)
}

fn shape_refused(status: u16, message: &str) -> bool {
    let text = words_of(message);
    matches!(status, 400 | 422)
        && (SHAPE_WORDS.is_match(&text)
            || UNSAID_WORDS.is_match(&text)
            || CUSTOM_TOOL_REFUSAL.is_match(&text)
            || wrong_endpoint(status, &text))
        && !QUOTA_WORDS.is_match(&text)
        && !CREDIT_WORDS.is_match(&text)
        && !UNSERVED_WORDS.is_match(&text)
        && !REFUSED_WORDS.is_match(&text)
}

fn model_retired(status: u16, raw: &str, text: &str) -> bool {
    matches!(status, 400 | 404 | 410 | 422)
        && !policy_refusal(raw)
        && RETIRED_WORDS.is_match(text)
        && !QUOTA_WORDS.is_match(text)
}

fn edge_blocked(text: &str) -> bool {
    EDGE_BLOCKED.is_match(text) || text.contains(BLOCKED_HINT)
}

fn wrong_endpoint(status: u16, text: &str) -> bool {
    if !(400..500).contains(&status) {
        return false;
    }
    let lower = text.to_lowercase();
    WRONG_ENDPOINT.iter().any(|phrase| lower.contains(phrase))
}

/// Whether a JSON error body is the vendor's safety filter refusing the
/// request (`reference: internal/gateway/fallback.go:3381-3425`).
fn policy_refusal(raw: &str) -> bool {
    let Ok(serde_json::Value::Object(body)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    let error = match body.get("error") {
        Some(serde_json::Value::Object(error)) => error,
        _ => &body,
    };
    let field = |name: &str| error.get(name).and_then(serde_json::Value::as_str);
    let message = field("message").unwrap_or_default();
    let code = field("code")
        .filter(|code| !code.is_empty())
        .or_else(|| field("type"))
        .unwrap_or_default();
    FILTER_CODES.contains(&code)
        || POLICY_CODE.is_match(code)
        || code == "invalid_prompt" && FLAGGED_WORDS.is_match(message)
        || message.trim() == HIGH_RISK_REFUSAL
}

/// The text with each string of its JSON appended on its own line, when the
/// text escapes any as `\u` (`reference: internal/catalog/catalog.go:1653`).
fn words_of(text: &str) -> String {
    if !text.contains("\\u") {
        return text.to_owned();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_owned();
    };
    let mut out = text.to_owned();
    let mut stack = vec![&value];
    while let Some(value) = stack.pop() {
        match value {
            serde_json::Value::String(string) => {
                out.push('\n');
                out.push_str(string);
            }
            serde_json::Value::Array(items) => stack.extend(items.iter().rev()),
            serde_json::Value::Object(map) => stack.extend(map.values().rev()),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> BTreeSet<u16> {
        BTreeSet::new()
    }

    #[test]
    fn body_word_classes_fail_over_without_a_listed_status() {
        for (status, message) in [
            (400, "Unknown parameter: tools"),
            (400, "Unrecognized request argument supplied: foo"),
            (422, "invalid request error trace_id: 0123456789abcdef01"),
            (
                400,
                "This model is only supported in v1/responses and not in v1/chat/completions.",
            ),
        ] {
            let error = PluginError::status_error(status, message);
            assert!(allows(&error, false, &none()), "{status} {message}");
        }
        let overflow = PluginError::status_error(400, "prompt is too long: 300 tokens");
        assert!(!allows(&overflow, false, &[400].into_iter().collect()));
    }

    #[test]
    fn a_custom_tool_refusal_asks_the_next_member() {
        let error = PluginError::status_error(400, "custom tools are not allowed");
        assert!(asks_next(&error));
        assert!(!allows(&error, false, &none()));
    }

    fn unsaid(status: u16, message: &str, tokens: u64) -> Option<PluginError> {
        too_long_unsaid(
            &PluginError::status_error(status, message),
            tokens,
            Some(100),
            "demo",
            "primary",
        )
    }

    #[test]
    fn too_long_unsaid_needs_the_whole_margin() {
        assert!(unsaid(502, "capacity issues", 119).is_none());
        assert!(unsaid(502, "capacity issues", 110).is_none());
        assert!(unsaid(502, "capacity issues", 120).is_some());
        assert!(unsaid(504, "gateway timeout", 120).is_some());
        assert!(unsaid(501, "not implemented", 120).is_none());
    }

    #[test]
    fn too_long_unsaid_leaves_a_failure_that_says_its_cause() {
        for message in [
            "You exceeded your current quota, please check your plan",
            "unapproved channel",
            "User location is not supported for the API use.",
            "prompt is too long: 300 tokens > 200 maximum",
        ] {
            assert!(unsaid(400, message, 1000).is_none(), "{message}");
        }
    }

    #[test]
    fn too_long_unsaid_cuts_the_vendor_text() {
        let said = format!("prompt is too long {}", "x".repeat(300));
        let error = unsaid(502, &said, 1000).expect("inferred");
        let quoted = error
            .message()
            .split("answered 502: ")
            .nth(1)
            .expect("quoted");
        assert_eq!(quoted.chars().count(), 202);
        assert!(quoted.ends_with("\u{2026})"));
    }

    #[test]
    fn every_list_compiles() {
        for list in [
            &QUOTA_WORDS,
            &CREDIT_WORDS,
            &UNSERVED_WORDS,
            &RETIRED_WORDS,
            &REFUSED_WORDS,
            &SHAPE_WORDS,
            &UNSAID_WORDS,
            &REGION_WORDS,
            &PROTECTION_WORDS,
            &ZDR_CODED,
            &CUSTOM_TOOL_REFUSAL,
            &EDGE_BLOCKED,
            &TOO_LONG,
            &PACE_WORDS,
            &POLICY_CODE,
            &FLAGGED_WORDS,
        ] {
            assert!(!list.as_str().is_empty());
        }
    }

    #[test]
    fn escaped_words_are_read_decoded() {
        let body = r#"{"error":{"message":"\u4e0a\u4e0b\u6587\u8fc7\u957f"}}"#;
        assert!(words_of(body).ends_with("\n\u{4e0a}\u{4e0b}\u{6587}\u{8fc7}\u{957f}"));
        assert!(too_long(400, &words_of(body)));
        assert!(!too_long(400, body));
    }

    #[test]
    fn a_rate_limit_counted_in_tokens_is_not_an_overflow() {
        assert!(!too_long(400, "Too many tokens, please wait"));
        assert!(!too_long(400, "context exceeded: 90000 tokens per minute"));
        assert!(too_long(
            400,
            "This model's maximum context length is 8192 tokens"
        ));
        assert!(!too_long(
            400,
            "max_tokens is too large: exceeds the context"
        ));
        assert!(!too_long(429, "prompt is too long"));
    }

    #[test]
    fn overloaded_wording_beside_context_is_not_an_overflow() {
        assert!(!too_long(400, "server overloaded; context saved"));
    }

    #[test]
    fn a_safety_refusal_is_not_a_retired_model() {
        let body = r#"{"error":{"code":"content_filter","message":"the model is deprecated"}}"#;
        assert!(!model_retired(400, body, body));
        assert!(model_retired(
            410,
            "Model exo has been deprecated.",
            "Model exo has been deprecated."
        ));
    }

    #[test]
    fn an_edge_block_code_needs_a_boundary() {
        assert!(edge_blocked("error code: 1020"));
        assert!(!edge_blocked("error code: 10200"));
        assert!(!edge_blocked("error code: 1004"));
    }
}
