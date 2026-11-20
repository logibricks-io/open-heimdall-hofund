//! Inbound `service_tier: "fast"` becomes `priority` on Chat and Responses.

use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};

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
