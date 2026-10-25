//! OpenAI Responses snapshots, round trips, and stream edges. No network.

use hofund_codec::{AnthropicMessages, Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::{ContentPart, Request, Role, StopReason, StreamEvent, assemble};
use serde_json::json;

const REQUEST: &str = include_str!("fixtures/responses_request.json");
const RESPONSE: &str = include_str!("fixtures/responses_response.json");
const STREAM: &[u8] = include_bytes!("fixtures/responses_stream.sse");

fn pretty_json(bytes: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("json");
    serde_json::to_string_pretty(&value).expect("pretty")
}

macro_rules! pretty_value {
    ($value:expr) => {
        serde_json::to_string_pretty($value).expect("pretty")
    };
}

fn stable_request(bytes: &[u8]) -> Request {
    let first = OpenAiResponses.decode_request(bytes).expect("decode");
    let encoded = OpenAiResponses.encode_request(&first).expect("encode");
    let second = OpenAiResponses.decode_request(&encoded).expect("redecode");
    assert_eq!(first, second);
    first
}

#[test]
fn request_round_trip_and_snapshot() {
    let request = stable_request(REQUEST.as_bytes());
    insta::assert_snapshot!("responses_request_ir", pretty_value!(&request));
    let encoded = OpenAiResponses.encode_request(&request).expect("encode");
    insta::assert_snapshot!("responses_request_wire", pretty_json(&encoded));
    assert_eq!(request.messages[0].role, Role::System);
    assert_eq!(request.extensions.get("store"), Some(&json!(true)));
    assert_eq!(
        request.extensions.get("metadata"),
        Some(&json!({"user": "u1"}))
    );
    let sealed = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|part| match part {
            ContentPart::Thinking(thinking) => thinking.extensions.get("sealed_item").cloned(),
            _ => None,
        });
    assert_eq!(
        sealed
            .as_ref()
            .and_then(|value| value.get("encrypted_content")),
        Some(&json!("seal"))
    );
}

#[test]
fn response_round_trip_and_snapshot() {
    let first = OpenAiResponses
        .decode_response(RESPONSE.as_bytes())
        .expect("decode");
    let encoded = OpenAiResponses.encode_response(&first).expect("encode");
    let second = OpenAiResponses.decode_response(&encoded).expect("redecode");
    assert_eq!(first, second);
    insta::assert_snapshot!("responses_response_ir", pretty_value!(&first));
    insta::assert_snapshot!("responses_response_wire", pretty_json(&encoded));
    assert_eq!(
        first.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
    assert_eq!(
        first
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_write_tokens),
        Some(1)
    );
    assert_eq!(
        first
            .usage
            .as_ref()
            .and_then(|usage| usage.reasoning_tokens),
        Some(3)
    );
}

#[test]
fn stream_round_trip_reconciles_argument_suffix() {
    let first = OpenAiResponses.decode_stream(STREAM).expect("decode");
    let encoded = OpenAiResponses.encode_stream(&first).expect("encode");
    let second = OpenAiResponses.decode_stream(&encoded).expect("redecode");
    assert_eq!(first, second);
    insta::assert_snapshot!("responses_stream_ir", pretty_value!(&first));
    let response = assemble(&first);
    assert_eq!(
        response.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
    let arguments = response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call.arguments.to_text()),
            _ => None,
        })
        .expect("tool call");
    assert_eq!(arguments, "{\"city\":\"Paris\"}");
}

#[test]
fn stream_chunks_can_split_a_line() {
    let mut decoder = OpenAiResponses.stream_decoder();
    let split = STREAM
        .windows(2)
        .position(|pair| pair == b"de")
        .expect("split");
    let (left, right) = STREAM.split_at(split + 1);
    let mut events = decoder.push(left).expect("left");
    events.extend(decoder.push(right).expect("right"));
    events.extend(decoder.finish().expect("finish"));
    assert_eq!(
        events,
        OpenAiResponses.decode_stream(STREAM).expect("whole")
    );
}

#[test]
fn allowed_tools_filters_the_list() {
    let body = r#"{
        "model": "m",
        "input": "hi",
        "tools": [
            {"type": "function", "name": "keep", "parameters": {}},
            {"type": "function", "name": "drop", "parameters": {}}
        ],
        "tool_choice": {
            "type": "allowed_tools",
            "mode": "required",
            "tools": [{"type": "function", "name": "keep"}]
        }
    }"#;
    let request = stable_request(body.as_bytes());
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "keep");
    assert!(matches!(
        request.tool_choice.as_ref().map(|choice| &choice.mode),
        Some(hofund_ir::ToolChoiceMode::Required)
    ));
}

#[test]
fn prepare_upstream_body_stores_nothing_and_closes_calls() {
    let request = OpenAiResponses
        .decode_request(
            br#"{"model":"m","input":[{"type":"function_call","call_id":"c","name":"n","arguments":"{}"}]}"#,
        )
        .expect("decode");
    let encoded = OpenAiResponses.encode_request(&request).expect("encode");
    let prepared = OpenAiResponses::prepare_upstream_body(&encoded).expect("prepare");
    let value: serde_json::Value = serde_json::from_slice(&prepared).expect("json");
    assert_eq!(value["store"], json!(false));
    assert_eq!(value["input"][1]["type"], "function_call_output");
    assert_eq!(value["input"][1]["call_id"], "c");
    assert_eq!(value["input"][1]["output"], "");
}

#[test]
fn chat_developer_role_survives_responses() {
    let chat = OpenAiChat
        .decode_request(
            br#"{"model":"m","messages":[{"role":"developer","content":"rules"},{"role":"user","content":"hi"}]}"#,
        )
        .expect("chat");
    let wire = OpenAiResponses.encode_request(&chat).expect("responses");
    let again = OpenAiResponses.decode_request(&wire).expect("decode");
    assert_eq!(again.messages[0].role, Role::Developer);
    let back = OpenAiChat.encode_request(&again).expect("chat encode");
    let value: serde_json::Value = serde_json::from_slice(&back).expect("json");
    assert_eq!(value["messages"][0]["role"], "developer");
}

#[test]
fn top_k_is_not_on_the_responses_wire() {
    let mut request = AnthropicMessages
        .decode_request(
            br#"{"model":"m","max_tokens":8,"top_k":3,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("anthropic");
    request.params.top_k = Some(3);
    let wire = OpenAiResponses.encode_request(&request).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&wire).expect("json");
    assert!(value.get("top_k").is_none());
    assert_eq!(value["max_output_tokens"], 8);
}

#[test]
fn malformed_input_is_an_error() {
    assert!(OpenAiResponses.decode_request(b"").is_err());
    assert!(OpenAiResponses.decode_request(b"{}").is_err());
    assert!(
        OpenAiResponses
            .decode_request(b"{\"model\":\"m\"}")
            .is_err()
    );
    assert!(OpenAiResponses.decode_stream(b"data: {\"type\":").is_err());
}

#[test]
fn diverging_argument_done_does_not_append_a_second_copy() {
    let bytes = br#"
event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c","name":"n","arguments":""}}

event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"a\":1}"}

event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"b\":2}"}

event: response.completed
data: {"type":"response.completed","response":{"status":"completed","output":[]}}
"#;
    let events = OpenAiResponses.decode_stream(bytes).expect("decode");
    let response = assemble(&events);
    let arguments = response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call.arguments.to_text()),
            _ => None,
        })
        .expect("call");
    assert_eq!(arguments, "{\"a\":1}");
    let _ = events;
}

#[test]
fn leading_failed_event_with_no_output_stays_unknown() {
    let bytes = br#"event: response.failed
data: {"type":"response.failed","response":{"id":"resp_f","status":"failed","output":[]}}

"#;
    let events = OpenAiResponses.decode_stream(bytes).expect("decode");
    assert!(matches!(
        events.as_slice(),
        [StreamEvent::Unknown(event)] if event.name == "response.failed"
    ));
}

#[test]
fn streamed_encrypted_content_is_kept_on_the_thinking_part() {
    let bytes = br#"event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"seal-1","summary":[]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_s","status":"completed","output":[]}}

"#;
    let events = OpenAiResponses.decode_stream(bytes).expect("decode");
    let response = assemble(&events);
    let sealed = response
        .message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Thinking(thinking) => thinking.extensions.get("sealed_item").cloned(),
            _ => None,
        })
        .expect("thinking");
    assert_eq!(sealed.get("encrypted_content"), Some(&json!("seal-1")));
    assert_eq!(sealed.get("id"), Some(&json!("rs_1")));
    let encoded = OpenAiResponses.encode_stream(&events).expect("encode");
    let text = String::from_utf8(encoded).expect("utf8");
    assert!(text.contains("seal-1"));
}

#[test]
fn failed_after_created_is_a_stop_reason() {
    let bytes = br#"event: response.created
data: {"type":"response.created","response":{"id":"resp_f","status":"in_progress","output":[]}}

event: response.failed
data: {"type":"response.failed","response":{"id":"resp_f","status":"failed","output":[]}}

"#;
    let events = OpenAiResponses.decode_stream(bytes).expect("decode");
    assert!(matches!(events.first(), Some(StreamEvent::MessageStart(_))));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            StreamEvent::MessageDelta(delta)
                if matches!(
                    delta.stop.as_ref().map(|stop| &stop.reason),
                    Some(StopReason::Other(token)) if token == "failed"
                )
        )
    }));
}

#[test]
fn responses_stream_emits_done_events_sequence_and_full_output() {
    use hofund_ir::{
        BlockStart, ContentBlockStart, ContentBlockStop, Extensions, MessageDelta, MessageStart,
        MessageStop, Stop, StopReason, TextDelta, Usage,
    };
    let events = vec![
        StreamEvent::MessageStart(MessageStart {
            id: Some("resp_1".to_owned()),
            model: Some("m".to_owned()),
            usage: None,
            extensions: Extensions::new(),
        }),
        StreamEvent::ContentBlockStart(ContentBlockStart {
            index: 0,
            block: BlockStart::Text,
            extensions: Extensions::new(),
        }),
        StreamEvent::TextDelta(TextDelta {
            index: 0,
            text: "Hello".to_owned(),
            extensions: Extensions::new(),
        }),
        StreamEvent::ContentBlockStop(ContentBlockStop {
            index: 0,
            extensions: Extensions::new(),
        }),
        StreamEvent::MessageDelta(MessageDelta {
            stop: Some(Stop {
                reason: StopReason::EndTurn,
                sequence: None,
            }),
            usage: Some(Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Usage::default()
            }),
            extensions: Extensions::new(),
        }),
        StreamEvent::MessageStop(MessageStop::default()),
    ];
    let encoded = OpenAiResponses.encode_stream(&events).expect("encode");
    let text = String::from_utf8(encoded).expect("utf8");
    let frames: Vec<serde_json::Value> = text
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data");
            serde_json::from_str(data).expect("json")
        })
        .collect();
    let mut previous = None;
    for frame in &frames {
        let sequence = frame["sequence_number"].as_u64().expect("sequence");
        if let Some(seen) = previous {
            assert!(sequence > seen);
        }
        previous = Some(sequence);
    }
    let names: Vec<&str> = frames
        .iter()
        .map(|frame| frame["type"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"response.output_text.done"));
    assert!(names.contains(&"response.content_part.done"));
    assert!(names.contains(&"response.output_item.done"));
    assert!(names.contains(&"response.completed"));
    let item_done = frames
        .iter()
        .find(|frame| frame["type"] == "response.output_item.done")
        .expect("item done");
    assert_eq!(item_done["item"]["id"], json!("msg_0"));
    assert_eq!(item_done["item"]["content"][0]["text"], json!("Hello"));
    let completed = frames
        .iter()
        .find(|frame| frame["type"] == "response.completed")
        .expect("completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        json!("Hello")
    );
    assert_eq!(completed["response"]["output"][0]["id"], json!("msg_0"));
}
