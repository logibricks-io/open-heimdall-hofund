//! Service-tier words shared by Chat Completions, Responses, and the catalog.
//!
//! The inbound word `fast` is the same tier as `priority`. Ultrafast is a
//! family of GPT Sol, Astra, and Terra models. The public OpenAI API accepts
//! `ultrafast` only on Responses, and only for a short list of those models.

use hofund_ir::{Extensions, Request, Response, StreamEvent};
use serde_json::{Map, Value};

/// Service tiers a Codex catalog names when a model offers Ultrafast.
pub const ULTRA_OFFER: [&str; 2] = ["priority", "ultrafast"];

/// Rewrites a top-level string `service_tier` of `fast` to `priority`.
///
/// The comparison ignores ASCII case, so `Fast` and `FAST` are the same word.
/// Any other value, including a non-string or a nested field, is left as it
/// arrived.
pub fn rewrite_fast_word(object: &mut Map<String, Value>) {
    if object
        .get("service_tier")
        .and_then(Value::as_str)
        .is_some_and(|tier| tier.eq_ignore_ascii_case("fast"))
    {
        object.insert(
            "service_tier".to_owned(),
            Value::String("priority".to_owned()),
        );
    }
}

/// The tier a group member is configured to send.
///
/// Ultrafast wins when both flags are set. An unconfigured member leaves the
/// client's tier in place.
#[must_use]
pub fn configured_tier(fast: bool, ultra: bool) -> String {
    if ultra {
        "ultrafast".to_owned()
    } else if fast {
        "priority".to_owned()
    } else {
        String::new()
    }
}

/// Whether `model` is a versioned GPT Sol, Astra, or Terra model.
///
/// The check is the family, not a fixed id list. A provider prefix, a leading
/// bracket tag, a `:suffix`, and a dated snapshot suffix are ignored. Mini,
/// fast, and pro variants are not the family.
#[must_use]
pub fn offers_ultra_family(model: &str) -> bool {
    family_tail(&comparable_model(model))
}

/// Whether Responses on `api.openai.com` accepts `ultrafast` for `model`.
///
/// The public API accepts that word for four model ids and their dated
/// snapshots. Other Sol, Astra, and Terra models are still the Ultra family,
/// but that host rejects `ultrafast` for them.
#[must_use]
pub fn openai_responses_ultrafast(model: &str) -> bool {
    matches!(
        comparable_model(model).as_str(),
        "gpt-6.2-astra" | "gpt-6.2-sol" | "gpt-6-astra" | "gpt-6.1-sol"
    )
}

/// `true` when `base_url` is the public OpenAI API host.
#[must_use]
pub fn is_openai_api_host(base_url: &str) -> bool {
    host_of(base_url).eq_ignore_ascii_case("api.openai.com")
}

/// `true` when `base_url` is the ChatGPT backend Codex subscriptions use.
#[must_use]
pub fn chatgpt_backend(base_url: &str) -> bool {
    let host = host_of(base_url);
    host.eq_ignore_ascii_case("chatgpt.com") || host.ends_with(".chatgpt.com")
}

/// The codec the host rules should use.
///
/// `spoken` is the codec the upstream calls. An empty `spoken` keeps
/// `member_protocol`, so a member whose label is already that codec is unchanged.
#[must_use]
pub fn spoken_protocol<'a>(member_protocol: &'a str, spoken: &'a str) -> &'a str {
    if spoken.is_empty() {
        member_protocol
    } else {
        spoken
    }
}

/// Whether `auto` and `default` on a reply are placeholders.
///
/// The ChatGPT backend says those words whether a faster tier ran or not.
/// The public API means them: `default` and `standard` are the 1x rate.
#[must_use]
pub fn clear_placeholder_served(base_url: &str) -> bool {
    chatgpt_backend(base_url)
}

/// Tier ids `/v1/models` should offer for one hop.
///
/// A ChatGPT-backend hop offers Ultrafast for any GPT Sol, Astra, or Terra
/// model. A public `api.openai.com` Responses hop offers it only for
/// [`openai_responses_ultrafast`]. A Chat hop on that host does not. A member
/// configured `ultra` offers it on any model. There is no per-account plan list.
#[must_use]
pub fn offered_tier_ids(
    base_url: &str,
    protocol: &str,
    model: &str,
    member_ultra: bool,
) -> Vec<&'static str> {
    let family = offers_ultra_family(model);
    let public_host = is_openai_api_host(base_url);
    let backend = chatgpt_backend(base_url);
    let ultra = member_ultra
        || (backend && family)
        || (public_host && protocol == "openai-responses" && openai_responses_ultrafast(model));
    let fast = ultra || ((public_host || backend) && family);
    if ultra {
        ULTRA_OFFER.to_vec()
    } else if fast {
        vec!["priority"]
    } else {
        Vec::new()
    }
}

/// The replacement tier when `api.openai.com` must not be sent `ultrafast`.
///
/// Chat Completions never carries `ultrafast`. Responses carries it only for
/// [`openai_responses_ultrafast`]. Other hosts are left alone.
#[must_use]
pub fn downgrade_openai_ultrafast(
    base_url: &str,
    protocol: &str,
    model: &str,
    tier: &str,
) -> Option<&'static str> {
    if tier != "ultrafast" || !is_openai_api_host(base_url) {
        return None;
    }
    if protocol == "openai-chat" {
        return Some("priority");
    }
    if protocol == "openai-responses" && !openai_responses_ultrafast(model) {
        return Some("priority");
    }
    None
}

/// Tiers a ChatGPT-backend body may still carry after the keep-list.
const KEPT_BACKEND_TIERS: [&str; 2] = ["priority", "ultrafast"];

/// Applies a member's configured tier, then the host rules.
///
/// An empty `member_tier` keeps the tier already on the request. `fast` in
/// any ASCII case becomes `priority` first. A ChatGPT-backend hop then drops
/// every word other than `priority` and `ultrafast`, so the tier the router
/// records is the one still on the body. The Codex body filter repeats that
/// keep-list.
pub fn apply_outbound(request: &mut Request, member_tier: &str, base_url: &str, protocol: &str) {
    if !member_tier.is_empty() {
        request
            .extensions
            .insert("service_tier", Value::String(member_tier.to_owned()));
    }
    let mut current = extension_tier(&request.extensions);
    if current.eq_ignore_ascii_case("fast") {
        current = "priority".to_owned();
        request
            .extensions
            .insert("service_tier", Value::String(current.clone()));
    }
    if let Some(next) = downgrade_openai_ultrafast(base_url, protocol, &request.model, &current) {
        current = next.to_owned();
        request
            .extensions
            .insert("service_tier", Value::String(current.clone()));
    }
    if chatgpt_backend(base_url) && !KEPT_BACKEND_TIERS.contains(&current.as_str()) {
        request.extensions.remove("service_tier");
    }
}

/// The `service_tier` string on `extensions`, or empty.
#[must_use]
pub fn extension_tier(extensions: &Extensions) -> String {
    extensions
        .get("service_tier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Drops every extension on `event` except a served `service_tier`.
pub fn retain_served_tier(event: &mut StreamEvent) {
    let tier = event
        .extensions()
        .map(extension_tier)
        .filter(|tier| !tier.is_empty());
    event.strip_extensions();
    let Some(tier) = tier else {
        return;
    };
    let Some(slot) = event_extensions_mut(event) else {
        return;
    };
    slot.insert("service_tier", Value::String(tier));
}

/// Drops every extension on `response` except a served `service_tier`.
pub fn retain_response_tier(response: &mut Response) {
    let tier = extension_tier(&response.extensions);
    response.strip_extensions();
    if tier.is_empty() {
        return;
    }
    response
        .extensions
        .insert("service_tier", Value::String(tier));
}

/// Writes the last served `service_tier` in `events` onto `response`.
///
/// [`hofund_ir::assemble`] keeps the first extension, so a
/// `response.created` word would hide the word on `response.completed`.
pub fn stamp_last_served_tier(events: &[StreamEvent], response: &mut Response) {
    let mut last = String::new();
    for event in events {
        let Some(extensions) = event.extensions() else {
            continue;
        };
        let tier = extension_tier(extensions);
        if !tier.is_empty() {
            last = tier;
        }
    }
    if last.is_empty() {
        return;
    }
    response
        .extensions
        .insert("service_tier", Value::String(last));
}

fn event_extensions_mut(event: &mut StreamEvent) -> Option<&mut Extensions> {
    Some(match event {
        StreamEvent::MessageStart(event) => &mut event.extensions,
        StreamEvent::ContentBlockStart(event) => &mut event.extensions,
        StreamEvent::TextDelta(event) => &mut event.extensions,
        StreamEvent::ThinkingDelta(event) => &mut event.extensions,
        StreamEvent::SignatureDelta(event) => &mut event.extensions,
        StreamEvent::ToolArgumentsDelta(event) => &mut event.extensions,
        StreamEvent::ContentBlockStop(event) => &mut event.extensions,
        StreamEvent::MessageDelta(event) => &mut event.extensions,
        StreamEvent::MessageStop(event) => &mut event.extensions,
        StreamEvent::Ping(event) => &mut event.extensions,
        StreamEvent::StillWorking(event) => &mut event.extensions,
        StreamEvent::Unknown(_) => return None,
    })
}

/// Rewrites `ultrafast` inside an encoded JSON object when the host requires it.
///
/// A body that is not a JSON object is returned unchanged.
#[must_use]
pub fn adjust_encoded_body(base_url: &str, protocol: &str, body: &[u8]) -> Vec<u8> {
    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let tier = object
        .get("service_tier")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let Some(next) = downgrade_openai_ultrafast(base_url, protocol, &model, &tier) else {
        return body.to_vec();
    };
    object.insert("service_tier".to_owned(), Value::String(next.to_owned()));
    serde_json::to_vec(&object).unwrap_or_else(|_| body.to_vec())
}

/// Catalog objects for `ids`, using the names Codex's picker shows.
#[must_use]
pub fn named_tiers(ids: &[&str]) -> Value {
    Value::Array(ids.iter().copied().map(named_tier).collect())
}

fn named_tier(id: &str) -> Value {
    let name = match id {
        "priority" => "Fast",
        "ultrafast" => "Ultrafast",
        other => other,
    };
    serde_json::json!({"id": id, "name": name})
}

/// Model id used for family and public-API checks.
#[must_use]
pub fn comparable_model(model: &str) -> String {
    let mut id = model.trim().to_ascii_lowercase();
    if let Some((_, tail)) = id.rsplit_once('/') {
        id = tail.to_owned();
    }
    while let Some(rest) = id.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            break;
        };
        id = rest[end + 1..].to_owned();
    }
    if let Some((base, _)) = id.split_once(':') {
        id = base.to_owned();
    }
    strip_dated(&id).to_owned()
}

fn family_tail(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("gpt-") else {
        return false;
    };
    let Some((version, family)) = rest.rsplit_once('-') else {
        return false;
    };
    if !matches!(family, "sol" | "astra" | "terra") || version.is_empty() {
        return false;
    }
    version
        .split(['.', '-'])
        .all(|part| !part.is_empty() && part.chars().all(|character| character.is_ascii_digit()))
}

fn strip_dated(id: &str) -> &str {
    let bytes = id.as_bytes();
    if bytes.len() >= 12 && bytes[bytes.len() - 11] == b'-' {
        let tail = &id[bytes.len() - 10..];
        if calendar_day(tail) {
            return &id[..bytes.len() - 11];
        }
    }
    if bytes.len() >= 9 && bytes[bytes.len() - 9] == b'-' {
        let tail = &id[bytes.len() - 8..];
        if tail.len() == 8 && tail.chars().all(|character| character.is_ascii_digit()) {
            return &id[..bytes.len() - 9];
        }
    }
    id
}

fn calendar_day(tail: &str) -> bool {
    let bytes = tail.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && tail[..4]
            .chars()
            .all(|character| character.is_ascii_digit())
        && tail[5..7]
            .chars()
            .all(|character| character.is_ascii_digit())
        && tail[8..10]
            .chars()
            .all(|character| character.is_ascii_digit())
}

fn host_of(base_url: &str) -> &str {
    let rest = base_url.split("://").nth(1).unwrap_or(base_url);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let rest = rest.rsplit('@').next().unwrap_or(rest);
    rest.split(':').next().unwrap_or(rest)
}

#[cfg(test)]
mod tests {
    use super::{
        adjust_encoded_body, apply_outbound, chatgpt_backend, clear_placeholder_served,
        comparable_model, configured_tier, downgrade_openai_ultrafast, extension_tier,
        offered_tier_ids, offers_ultra_family, openai_responses_ultrafast, retain_response_tier,
        retain_served_tier, rewrite_fast_word, spoken_protocol, stamp_last_served_tier,
    };
    use hofund_ir::{
        Extensions, Message, MessageDelta, MessageStart, Request, Response, Role, StreamEvent,
    };
    use serde_json::{Map, Value};

    #[test]
    fn d224_completed_served_tier_replaces_the_created_word() {
        let mut created = StreamEvent::MessageStart(MessageStart {
            id: None,
            model: None,
            usage: None,
            extensions: Extensions::new(),
        });
        if let StreamEvent::MessageStart(start) = &mut created {
            start
                .extensions
                .insert("service_tier", Value::String("ultrafast".to_owned()));
        }
        retain_served_tier(&mut created);
        let mut completed = StreamEvent::MessageDelta(MessageDelta {
            stop: None,
            usage: None,
            extensions: Extensions::new(),
        });
        if let StreamEvent::MessageDelta(delta) = &mut completed {
            delta
                .extensions
                .insert("service_tier", Value::String("default".to_owned()));
            delta.extensions.insert("other", Value::from(1));
        }
        retain_served_tier(&mut completed);
        let events = [created, completed];
        let mut response = hofund_ir::assemble(&events);
        assert_eq!(extension_tier(&response.extensions), "ultrafast");
        stamp_last_served_tier(&events, &mut response);
        assert_eq!(extension_tier(&response.extensions), "default");
        if let StreamEvent::MessageDelta(delta) = &events[1] {
            assert!(delta.extensions.get("other").is_none());
        }
    }

    #[test]
    fn d224_unary_response_keeps_only_the_served_tier() {
        let mut response = Response::new(Message::new(Role::Assistant));
        response
            .extensions
            .insert("service_tier", Value::String("default".to_owned()));
        response.extensions.insert("other", Value::from(1));
        retain_response_tier(&mut response);
        assert_eq!(extension_tier(&response.extensions), "default");
        assert!(response.extensions.get("other").is_none());
    }

    #[test]
    fn d224_spoken_protocol_wins_over_the_member_label() {
        assert_eq!(
            spoken_protocol("subscription", "openai-responses"),
            "openai-responses"
        );
        assert_eq!(spoken_protocol("openai-chat", ""), "openai-chat");
    }

    #[test]
    fn d224_fast_word_becomes_priority() {
        let mut object = Map::new();
        object.insert("service_tier".to_owned(), Value::String("fast".to_owned()));
        rewrite_fast_word(&mut object);
        assert_eq!(object["service_tier"], Value::String("priority".to_owned()));

        for kept in ["priority", "ultrafast", "flex", "auto"] {
            let mut object = Map::new();
            object.insert("service_tier".to_owned(), Value::String(kept.to_owned()));
            rewrite_fast_word(&mut object);
            assert_eq!(
                object["service_tier"],
                Value::String(kept.to_owned()),
                "{kept}"
            );
        }
        for capital in ["Fast", "FAST", "fAsT"] {
            let mut object = Map::new();
            object.insert("service_tier".to_owned(), Value::String(capital.to_owned()));
            rewrite_fast_word(&mut object);
            assert_eq!(
                object["service_tier"],
                Value::String("priority".to_owned()),
                "{capital}"
            );
        }

        let mut nested = Map::new();
        nested.insert(
            "metadata".to_owned(),
            serde_json::json!({"service_tier": "fast"}),
        );
        rewrite_fast_word(&mut nested);
        assert_eq!(nested["metadata"]["service_tier"], "fast");

        let mut number = Map::new();
        number.insert("service_tier".to_owned(), Value::from(1));
        rewrite_fast_word(&mut number);
        assert_eq!(number["service_tier"], Value::from(1));
    }

    #[test]
    fn d224_ultra_family_matches_any_gpt_version() {
        for model in [
            "gpt-6.1-sol",
            "openai/gpt-6.1-sol",
            "GPT-6.1-Sol",
            "gpt-6.1-sol-2026-09-14",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-6.1-terra",
            "relay/gpt-6.2-sol",
            "gpt-6.2-astra",
            "gpt-6.3-terra",
            "gpt-6.2-sol:official",
            "[A]gpt-6.2-sol",
            "gpt-6-1-sol",
        ] {
            assert!(offers_ultra_family(model), "{model}");
        }
        for model in [
            "gpt-6-luna",
            "gpt-6.2-astra-mini",
            "gpt-6-sol-fast",
            "gpt-6-sol-pro",
            "gpt-5.5",
            "claude-opus-4",
            "",
            "gpt-sol",
        ] {
            assert!(!offers_ultra_family(model), "{model}");
        }
    }

    #[test]
    fn d224_openai_responses_ultrafast_is_a_short_list() {
        for model in [
            "gpt-6.2-astra",
            "gpt-6.2-sol",
            "gpt-6-astra",
            "gpt-6.1-sol",
            "openai/gpt-6.2-sol",
            "GPT-6-Astra-2026-09-14",
            "gpt-6.1-sol-20261001",
        ] {
            assert!(openai_responses_ultrafast(model), "{model}");
        }
        for model in [
            "gpt-6-sol",
            "gpt-6.1-terra",
            "gpt-6.2-astra-mini",
            "gpt-6.2",
            "claude-opus-5-5",
            "gpt-5.6-sol",
            "gpt-6.3-terra",
        ] {
            assert!(!openai_responses_ultrafast(model), "{model}");
        }
        assert_eq!(
            comparable_model("openai/GPT-6.2-Sol:official"),
            "gpt-6.2-sol"
        );
    }

    #[test]
    fn d224_chat_on_the_public_api_never_sends_ultrafast() {
        assert_eq!(
            downgrade_openai_ultrafast(
                "https://api.openai.com/v1",
                "openai-chat",
                "gpt-6.2-sol",
                "ultrafast"
            ),
            Some("priority")
        );
        assert_eq!(
            downgrade_openai_ultrafast(
                "https://api.openai.com/v1",
                "openai-responses",
                "gpt-6.2-sol",
                "ultrafast"
            ),
            None
        );
        assert_eq!(
            downgrade_openai_ultrafast(
                "https://api.openai.com/v1",
                "openai-responses",
                "gpt-6-sol",
                "ultrafast"
            ),
            Some("priority")
        );
        assert_eq!(
            downgrade_openai_ultrafast(
                "https://relay.example/v1",
                "openai-chat",
                "gpt-6.2-sol",
                "ultrafast"
            ),
            None
        );
        let body = adjust_encoded_body(
            "https://api.openai.com/v1",
            "openai-chat",
            br#"{"model":"gpt-6.2-sol","service_tier":"ultrafast"}"#,
        );
        let value: Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["service_tier"], "priority");
        let kept = adjust_encoded_body(
            "https://api.openai.com/v1",
            "openai-responses",
            br#"{"model":"gpt-6.1-sol","service_tier":"ultrafast"}"#,
        );
        let value: Value = serde_json::from_slice(&kept).expect("json");
        assert_eq!(value["service_tier"], "ultrafast");
    }

    #[test]
    fn d224_member_tier_replaces_the_client_tier_before_the_host_rule() {
        let mut request = Request::new("gpt-6-sol");
        request
            .extensions
            .insert("service_tier", Value::String("priority".to_owned()));
        apply_outbound(
            &mut request,
            &configured_tier(false, true),
            "https://api.openai.com/v1",
            "openai-responses",
        );
        assert_eq!(
            request
                .extensions
                .get("service_tier")
                .and_then(Value::as_str),
            Some("priority")
        );

        let mut listed = Request::new("gpt-6.2-sol");
        apply_outbound(
            &mut listed,
            "ultrafast",
            "https://api.openai.com/v1",
            "openai-responses",
        );
        assert_eq!(
            listed
                .extensions
                .get("service_tier")
                .and_then(Value::as_str),
            Some("ultrafast")
        );
        assert_eq!(configured_tier(true, false), "priority");
        assert_eq!(configured_tier(false, false), "");
    }

    #[test]
    fn d224_chatgpt_backend_keeps_only_priority_and_ultrafast() {
        let backend = "https://chatgpt.com/backend-api/codex";
        let public_api = "https://api.openai.com/v1";
        let mut flex = Request::new("gpt-6.2-sol");
        flex.extensions
            .insert("service_tier", Value::String("flex".to_owned()));
        apply_outbound(&mut flex, "", backend, "openai-responses");
        assert!(flex.extensions.get("service_tier").is_none());

        let mut capital = Request::new("gpt-6.2-sol");
        capital
            .extensions
            .insert("service_tier", Value::String("Fast".to_owned()));
        apply_outbound(&mut capital, "", backend, "openai-responses");
        assert_eq!(
            capital
                .extensions
                .get("service_tier")
                .and_then(Value::as_str),
            Some("priority")
        );

        let mut kept = Request::new("gpt-6.2-sol");
        kept.extensions
            .insert("service_tier", Value::String("ultrafast".to_owned()));
        apply_outbound(&mut kept, "", backend, "openai-responses");
        assert_eq!(
            kept.extensions.get("service_tier").and_then(Value::as_str),
            Some("ultrafast")
        );

        let mut public_flex = Request::new("gpt-6.2-sol");
        public_flex
            .extensions
            .insert("service_tier", Value::String("flex".to_owned()));
        apply_outbound(&mut public_flex, "", public_api, "openai-responses");
        assert_eq!(
            public_flex
                .extensions
                .get("service_tier")
                .and_then(Value::as_str),
            Some("flex")
        );
    }

    #[test]
    fn d224_public_offers_follow_the_host_and_the_protocol() {
        let public_api = "https://api.openai.com/v1";
        let chat = "openai-chat";
        let responses = "openai-responses";
        assert_eq!(
            offered_tier_ids(public_api, responses, "gpt-6-sol", false),
            ["priority"]
        );
        assert_eq!(
            offered_tier_ids(public_api, chat, "gpt-6.2-sol", false),
            ["priority"]
        );
        assert!(!offered_tier_ids(public_api, chat, "gpt-6.2-sol", false).contains(&"ultrafast"));
        assert_eq!(
            offered_tier_ids(public_api, responses, "gpt-6.2-sol", false),
            ["priority", "ultrafast"]
        );
        assert_eq!(
            offered_tier_ids(public_api, responses, "gpt-6-astra", false),
            ["priority", "ultrafast"]
        );
        assert_eq!(
            offered_tier_ids(public_api, responses, "gpt-6.1-sol", false),
            ["priority", "ultrafast"]
        );
        let backend = "https://chatgpt.com/backend-api/codex";
        assert!(chatgpt_backend(backend));
        assert!(clear_placeholder_served(backend));
        assert!(!clear_placeholder_served(public_api));
        assert_eq!(
            offered_tier_ids(backend, responses, "gpt-6-sol", false),
            ["priority", "ultrafast"]
        );
        assert_eq!(
            offered_tier_ids(backend, responses, "gpt-6.2-astra", false),
            ["priority", "ultrafast"]
        );
        assert_eq!(
            offered_tier_ids(backend, chat, "gpt-5.6-terra", false),
            ["priority", "ultrafast"]
        );
        assert!(offered_tier_ids(public_api, chat, "glm-4.6", false).is_empty());
        assert_eq!(
            offered_tier_ids(public_api, chat, "glm-4.6", true),
            ["priority", "ultrafast"]
        );
        assert!(
            offered_tier_ids("https://relay.example/v1", responses, "gpt-6.2-sol", false)
                .is_empty()
        );
    }
}
