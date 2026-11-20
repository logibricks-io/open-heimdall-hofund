//! Inbound `service_tier: "fast"` becomes `priority` on Chat and Responses.

use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::StreamEvent;

#[test]
fn d224_chat_and_responses_send_fast_as_priority() {
    let chat = OpenAiChat;
    let decoded = chat
        .decode_request(
            br#"{"model":"gpt-6.2-sol","messages":[{"role":"user","content":"hi"}],"service_tier":"fast"}"#,
        )
        .expect("chat");
    assert_eq!(
        decoded
            .extensions
            .get("service_tier")
            .and_then(|value| value.as_str()),
        Some("priority")
    );
    let encoded = chat.encode_request(&decoded).expect("encode chat");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["service_tier"], "priority");

    let responses = OpenAiResponses;
    let decoded = responses
        .decode_request(br#"{"model":"gpt-6.2-sol","input":"hi","service_tier":"fast"}"#)
        .expect("responses");
    assert_eq!(
        decoded
            .extensions
            .get("service_tier")
            .and_then(|value| value.as_str()),
        Some("priority")
    );
    let encoded = responses
        .encode_request(&decoded)
        .expect("encode responses");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["service_tier"], "priority");

    let kept = chat
        .decode_request(
            br#"{"model":"gpt-6.2-sol","messages":[{"role":"user","content":"hi"}],"service_tier":"flex"}"#,
        )
        .expect("flex");
    assert_eq!(
        kept.extensions
            .get("service_tier")
            .and_then(|value| value.as_str()),
        Some("flex")
    );
}

fn tier_of(event: &StreamEvent) -> Option<&str> {
    let extensions = match event {
        StreamEvent::MessageStart(start) => &start.extensions,
        StreamEvent::MessageDelta(delta) => &delta.extensions,
        _ => return None,
    };
    extensions
        .get("service_tier")
        .and_then(|value| value.as_str())
}

#[test]
fn d224_responses_stream_keeps_the_served_tier() {
    let codec = OpenAiResponses;
    let mut decoder = codec.stream_decoder();
    let bytes = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6.2-sol\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"service_tier\":\"default\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6.2-sol\",\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
    );
    let events = decoder.push(bytes.as_bytes()).expect("events");
    let tiers: Vec<_> = events.iter().filter_map(tier_of).collect();
    assert_eq!(tiers, ["default"]);

    let mut nested = codec.stream_decoder();
    let nested_bytes = concat!(
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"status\":\"completed\",\"service_tier\":\"priority\"}}\n\n",
    );
    let events = nested.push(nested_bytes.as_bytes()).expect("events");
    let tiers: Vec<_> = events.iter().filter_map(tier_of).collect();
    assert!(tiers.contains(&"priority"), "{tiers:?}");
}
