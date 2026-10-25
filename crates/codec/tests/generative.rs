//! Snapshot and round-trip tests for the Generative Language codec. No network.

use hofund_codec::{Codec, GenerativeLanguage};
use hofund_ir::{
    ContentPart, ImageSource, Role, StopReason, StreamEvent, ToolChoiceMode, assemble,
};
use serde_json::json;

const REQUEST: &str = include_str!("fixtures/generative_request.json");
const RESPONSE: &str = include_str!("fixtures/generative_response.json");
const STREAM: &[u8] = include_bytes!("fixtures/generative_stream.sse");

fn pretty_json(bytes: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("json");
    serde_json::to_string_pretty(&value).expect("pretty")
}

macro_rules! pretty_value {
    ($value:expr) => {
        serde_json::to_string_pretty($value).expect("pretty")
    };
}

fn stable_request(bytes: &[u8]) -> hofund_ir::Request {
    let first = GenerativeLanguage.decode_request(bytes).expect("decode");
    let encoded = GenerativeLanguage.encode_request(&first).expect("encode");
    let second = GenerativeLanguage
        .decode_request(&encoded)
        .expect("decode encoded");
    assert_eq!(first, second);
    first
}

fn stable_response(bytes: &[u8]) -> hofund_ir::Response {
    let first = GenerativeLanguage.decode_response(bytes).expect("decode");
    let encoded = GenerativeLanguage.encode_response(&first).expect("encode");
    let second = GenerativeLanguage
        .decode_response(&encoded)
        .expect("decode encoded");
    assert_eq!(first, second);
    first
}

fn stable_stream(bytes: &[u8]) -> Vec<StreamEvent> {
    let first = GenerativeLanguage.decode_stream(bytes).expect("decode");
    let encoded = GenerativeLanguage.encode_stream(&first).expect("encode");
    let second = GenerativeLanguage
        .decode_stream(&encoded)
        .expect("decode encoded");
    assert_eq!(first, second);
    first
}

#[test]
fn request_round_trip_and_snapshot() {
    let request = stable_request(REQUEST.as_bytes());
    insta::assert_snapshot!("generative_request_ir", pretty_value!(&request));
    let encoded = GenerativeLanguage.encode_request(&request).expect("encode");
    insta::assert_snapshot!("generative_request_wire", pretty_json(&encoded));
    assert_eq!(request.messages[0].role, Role::System);
    assert_eq!(request.params.max_tokens, Some(100));
    assert_eq!(request.params.top_k, Some(40));
    assert_eq!(request.params.seed, Some(1));
    assert_eq!(
        request.params.extensions.get("candidateCount"),
        Some(&json!(1))
    );
    assert!(matches!(
        request.tool_choice.as_ref().map(|choice| &choice.mode),
        Some(ToolChoiceMode::Named { name }) if name == "get_weather"
    ));
    match &request.messages[2].content[0] {
        ContentPart::Thinking(part) => assert_eq!(part.signature.as_deref(), Some("sig-think")),
        other => panic!("expected thinking, got {other:?}"),
    }
    match &request.messages[2].content[1] {
        ContentPart::ToolCall(call) => {
            assert_eq!(
                call.extensions.get("thoughtSignature"),
                Some(&json!("sig-call"))
            );
        }
        other => panic!("expected a function call, got {other:?}"),
    }
    match &request.messages[1].content[2] {
        ContentPart::Image(image) => match &image.source {
            ImageSource::Url { url } => assert_eq!(url, "https://example.com/a.jpg"),
            other => panic!("expected a file uri, got {other:?}"),
        },
        other => panic!("expected an image, got {other:?}"),
    }
}

#[test]
fn response_round_trip_and_snapshot() {
    let response = stable_response(RESPONSE.as_bytes());
    insta::assert_snapshot!("generative_response_ir", pretty_value!(&response));
    let encoded = GenerativeLanguage
        .encode_response(&response)
        .expect("encode");
    insta::assert_snapshot!("generative_response_wire", pretty_json(&encoded));
    assert_eq!(
        response.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
    assert_eq!(
        response.usage.as_ref().and_then(|usage| usage.input_tokens),
        Some(10)
    );
    assert_eq!(
        response
            .usage
            .as_ref()
            .and_then(|usage| usage.reasoning_tokens),
        Some(2)
    );
    assert_eq!(
        response
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_tokens),
        Some(1)
    );
    assert_eq!(
        response.extensions.get("promptFeedback"),
        Some(&json!({"blockReason": "SAFETY"}))
    );
    match &response.message.content[1] {
        ContentPart::Text(part) => {
            assert_eq!(
                part.extensions.get("thoughtSignature"),
                Some(&json!("sig-text"))
            );
        }
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn stream_round_trip_and_snapshot() {
    let events = stable_stream(STREAM);
    insta::assert_snapshot!("generative_stream_ir", pretty_value!(&events));
    let encoded = GenerativeLanguage.encode_stream(&events).expect("encode");
    let text = String::from_utf8(encoded).expect("utf-8");
    insta::assert_snapshot!("generative_stream_wire", text);
    let response = assemble(&events);
    assert_eq!(
        response.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
    match response.message.content.last() {
        Some(ContentPart::ToolCall(call)) => {
            assert_eq!(call.name, "get_weather");
            assert_eq!(call.arguments.to_text(), r#"{"city":"Paris"}"#);
        }
        other => panic!("expected a function call, got {other:?}"),
    }
    match &response.message.content[1] {
        ContentPart::Thinking(part) => assert_eq!(part.signature.as_deref(), Some("sig-think")),
        other => panic!("expected thinking, got {other:?}"),
    }
}

#[test]
fn a_split_chunk_still_decodes() {
    let mut first = STREAM.to_vec();
    let at = first.iter().position(|byte| *byte == b'H').expect("split");
    let rest = first.split_off(at);
    let mut decoder = GenerativeLanguage.stream_decoder();
    let started = decoder.push(&first).expect("push");
    assert!(
        started.is_empty()
            || started
                .iter()
                .any(|event| matches!(event, StreamEvent::MessageStart(_)))
    );
    let mut events = decoder.push(&rest).expect("push");
    events.extend(decoder.finish().expect("finish"));
    let text = events.iter().any(|event| match event {
        StreamEvent::TextDelta(delta) => delta.text.contains('H'),
        _ => false,
    });
    assert!(text);
}

#[test]
fn validated_mode_and_an_extra_candidate_round_trip() {
    let request = stable_request(
        br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"toolConfig":{"functionCallingConfig":{"mode":"VALIDATED","allowedFunctionNames":["a","b"]}}}"#,
    );
    assert!(matches!(
        request.tool_choice.as_ref().map(|choice| &choice.mode),
        Some(ToolChoiceMode::Required)
    ));
    assert_eq!(
        request
            .tool_choice
            .as_ref()
            .and_then(|choice| choice.extensions.get("mode")),
        Some(&json!("VALIDATED"))
    );
    let encoded = GenerativeLanguage.encode_request(&request).expect("encode");
    let text = String::from_utf8(encoded).expect("utf-8");
    assert!(text.contains("VALIDATED"), "{text}");

    let response = stable_response(
        br#"{"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"a"}]}},{"index":1,"content":{"role":"model","parts":[{"text":"b"}]}}]}"#,
    );
    let extra = response
        .extensions
        .get("extraCandidates")
        .and_then(|value| value.as_array())
        .expect("extra");
    assert_eq!(extra.len(), 1);
    assert!(extra[0].to_string().contains('b'));
}

#[test]
fn a_tool_role_comes_back_as_user() {
    let mut request = hofund_ir::Request::new("demo");
    let mut message = hofund_ir::Message::new(Role::Tool);
    let mut result = hofund_ir::ToolResultPart {
        id: "call_1".to_owned(),
        is_error: true,
        content: vec![ContentPart::text("sunny")],
        extensions: hofund_ir::Extensions::new(),
    };
    result.extensions.insert("name", json!("get_weather"));
    message.content.push(ContentPart::ToolResult(result));
    request.messages.push(message);
    let encoded = GenerativeLanguage.encode_request(&request).expect("encode");
    let decoded = GenerativeLanguage.decode_request(&encoded).expect("decode");
    assert_eq!(decoded.messages[0].role, Role::User);
    match &decoded.messages[0].content[0] {
        ContentPart::ToolResult(part) => {
            assert!(!part.is_error);
            assert_eq!(part.extensions.get("name"), Some(&json!("get_weather")));
        }
        other => panic!("expected a function response, got {other:?}"),
    }
}

#[test]
fn error_body_uses_the_protocol_status() {
    let body = GenerativeLanguage.encode_error(401, "unauthorized");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(value["error"]["status"], "UNAUTHENTICATED");
    assert_eq!(value["error"]["code"], 401);
    let body = GenerativeLanguage.encode_error(429, "slow down");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(value["error"]["status"], "RESOURCE_EXHAUSTED");
}
