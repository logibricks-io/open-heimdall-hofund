//! Client `hofund_` keys must not become trusted request extensions.

use hofund_codec::{
    AnthropicMessages, Codec, Converse, GenerativeLanguage, OpenAiChat, OpenAiResponses,
};
use hofund_ir::ContentPart;
use serde_json::Value;

fn reserved(request: &hofund_ir::Request) -> Vec<String> {
    request
        .extensions
        .iter()
        .map(|(key, _)| key.clone())
        .filter(|key| key.starts_with("hofund_"))
        .collect()
}

#[test]
fn d247_responses_decode_drops_client_compact_fields_and_keeps_metadata() {
    let body = br#"{"model":"m1","input":"keep me","metadata":{"k":"v"},"hofund_responses_compact":true,"hofund_responses_compact_body":"{\"input\":\"substituted\"}","hofund_responses_wire":false}"#;
    let request = OpenAiResponses.decode_request(body).expect("decode");
    let reserved = reserved(&request);
    assert!(
        !reserved.iter().any(|key| key.contains("compact")),
        "{reserved:?}"
    );
    assert_eq!(
        request.extensions.get("hofund_responses_wire"),
        Some(&Value::Bool(true)),
        "the codec still marks its own wire after the client key is dropped"
    );
    assert_eq!(
        request
            .extensions
            .get("metadata")
            .and_then(|value| value.get("k")),
        Some(&Value::String("v".to_owned()))
    );
    match &request.messages[0].content[0] {
        ContentPart::Text(part) => assert_eq!(part.text, "keep me"),
        other => panic!("user text, got {other:?}"),
    }
    let encoded = OpenAiResponses.encode_request(&request).expect("encode");
    let encoded = String::from_utf8(encoded).expect("utf8");
    assert!(!encoded.contains("hofund_"), "{encoded}");
    assert!(encoded.contains("keep me"), "{encoded}");
    assert!(encoded.contains("\"k\":\"v\""), "{encoded}");
}

#[test]
fn d247_chat_decode_drops_reserved_fields_across_translation() {
    let body = br#"{"model":"m1","messages":[{"role":"user","content":"keep me"}],"metadata":{"k":"v"},"user":"alice","hofund_responses_compact":true,"hofund_responses_compact_body":"{\"input\":\"substituted\"}"}"#;
    let request = OpenAiChat.decode_request(body).expect("decode");
    assert!(reserved(&request).is_empty(), "{:?}", reserved(&request));
    assert_eq!(
        request
            .extensions
            .get("metadata")
            .and_then(|value| value.get("k")),
        Some(&Value::String("v".to_owned()))
    );
    assert_eq!(
        request.extensions.get("user"),
        Some(&Value::String("alice".to_owned()))
    );
    let chat = OpenAiChat.encode_request(&request).expect("chat");
    let chat = String::from_utf8(chat).expect("utf8");
    assert!(!chat.contains("hofund_"), "{chat}");
    assert!(chat.contains("keep me"), "{chat}");
    assert!(chat.contains("alice"), "{chat}");
    let responses = OpenAiResponses.encode_request(&request).expect("responses");
    let responses = String::from_utf8(responses).expect("utf8");
    assert!(!responses.contains("hofund_"), "{responses}");
    assert!(!responses.contains("substituted"), "{responses}");
    assert!(responses.contains("keep me"), "{responses}");
}

#[test]
fn d247_other_request_decoders_drop_client_compact_fields() {
    let messages = AnthropicMessages
        .decode_request(br#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}],"metadata":{"user_id":"u"},"hofund_responses_compact":true}"#)
        .expect("messages");
    assert!(
        !reserved(&messages)
            .iter()
            .any(|key| key.contains("compact")),
        "{:?}",
        reserved(&messages)
    );
    assert_eq!(
        messages
            .extensions
            .get("metadata")
            .and_then(|value| value.get("user_id")),
        Some(&Value::String("u".to_owned()))
    );
    assert_eq!(
        messages.extensions.get("hofund_messages_wire"),
        Some(&Value::Bool(true))
    );

    let generative = GenerativeLanguage
        .decode_request(br#"{"model":"m","contents":[{"role":"user","parts":[{"text":"hi"}]}],"extraField":1,"hofund_responses_compact":true}"#)
        .expect("generative");
    assert!(
        reserved(&generative).is_empty(),
        "{:?}",
        reserved(&generative)
    );
    assert_eq!(
        generative.extensions.get("extraField"),
        Some(&Value::from(1))
    );

    let converse = Converse
        .decode_request(br#"{"messages":[{"role":"user","content":[{"text":"hi"}]}],"metadata":{"k":1},"hofund_responses_compact":true}"#)
        .expect("converse");
    assert!(reserved(&converse).is_empty(), "{:?}", reserved(&converse));
    assert_eq!(
        converse
            .extensions
            .get("metadata")
            .and_then(|value| value.get("k")),
        Some(&Value::from(1))
    );
}
