//! OpenAI Responses snapshots, round trips, and stream edges. No network.

use hofund_codec::{AnthropicMessages, Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Extensions, Message,
    MessageStart, MessageStop, Request, Response, Role, StopReason, StreamEvent, ToolArguments,
    ToolArgumentsDelta, ToolCallPart, assemble,
};
use serde_json::json;

const REQUEST: &str = include_str!("fixtures/responses_request.json");
const RESPONSE: &str = include_str!("fixtures/responses_response.json");
const STREAM: &[u8] = include_bytes!("fixtures/responses_stream.sse");

fn drop_item_ids(response: &mut hofund_ir::Response) {
    response.message.extensions.remove("id");
    for part in &mut response.message.content {
        match part {
            ContentPart::Text(part) => {
                part.extensions.remove("id");
            }
            ContentPart::Thinking(part) => {
                part.extensions.remove("id");
            }
            ContentPart::ToolCall(part) => {
                part.extensions.remove("id");
            }
            ContentPart::ToolResult(part) => {
                part.extensions.remove("id");
            }
            ContentPart::Image(part) => {
                part.extensions.remove("id");
            }
            ContentPart::Unknown(part) => {
                part.extensions.remove("id");
            }
        }
    }
}

fn drop_stream_ids(events: &mut [StreamEvent]) {
    for event in events {
        if let StreamEvent::ContentBlockStart(start) = event {
            start.extensions.remove("item_id");
        }
    }
}

fn message_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        if value["type"] == "response.output_item.added"
            && let Some(id) = value["item"]["id"].as_str()
            && id.starts_with("msg_")
        {
            ids.push(id.to_owned());
        }
    }
    ids
}

fn stable_ids(bytes: &[u8]) -> String {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).expect("json");
    let mut next = 0_u32;
    stabilize(&mut value, &mut next);
    serde_json::to_string_pretty(&value).expect("pretty")
}

fn stabilize(value: &mut serde_json::Value, next: &mut u32) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                stabilize(item, next);
            }
        }
        serde_json::Value::Object(object) => {
            if let Some(serde_json::Value::String(id)) = object.get("id").cloned()
                && (id.starts_with("msg_")
                    || id.starts_with("fc_")
                    || id.starts_with("rs_")
                    || id.starts_with("ctc_"))
            {
                object.insert("id".to_owned(), json!(format!("id_{next}")));
                *next += 1;
            }
            for child in object.values_mut() {
                stabilize(child, next);
            }
        }
        _ => {}
    }
}

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
    let mut comparable = second.clone();
    drop_item_ids(&mut comparable);
    assert_eq!(first, comparable);
    let again = OpenAiResponses.encode_response(&second).expect("again");
    assert_eq!(encoded, again);
    insta::assert_snapshot!("responses_response_ir", pretty_value!(&first));
    insta::assert_snapshot!("responses_response_wire", stable_ids(&encoded));
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
    let mut second = OpenAiResponses.decode_stream(&encoded).expect("redecode");
    drop_stream_ids(&mut second);
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
fn codex_tool_types_pass_through_and_map() {
    // Tool types from the 2026-10-25 Codex CLI 0.164.0 smoke (`namespace`
    // `multi_agent_v1`, `web_search`, custom `apply_patch` with a grammar).
    // The body is reconstructed from those types; the original bytes were not saved.
    let body = include_str!("fixtures/codex_0_164_responses.json");
    let request = OpenAiResponses
        .decode_request(body.as_bytes())
        .expect("decode");
    let names: Vec<&str> = request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, vec!["multi_agent_v1__spawn_agent", "apply_patch"]);
    let patch = request
        .tools
        .iter()
        .find(|tool| tool.name == "apply_patch")
        .expect("apply_patch");
    assert_eq!(
        patch.input_schema["properties"]["input"]["type"],
        json!("string")
    );
    assert!(
        patch
            .description
            .as_deref()
            .is_some_and(|text| text.contains("lark") && text.contains("input"))
    );
    let dropped = request
        .extensions
        .get(hofund_codec::UNMAPPED_TOOLS)
        .expect("dropped");
    assert_eq!(dropped, &json!(["web_search"]));

    let same = OpenAiResponses.encode_request(&request).expect("encode");
    let same_value: serde_json::Value = serde_json::from_slice(&same).expect("json");
    let original: serde_json::Value = serde_json::from_str(body).expect("fixture");
    assert_eq!(same_value["tools"], original["tools"]);

    let mut crossed = request.clone();
    crossed.strip_extensions();
    let chat = OpenAiChat.encode_request(&crossed).expect("chat");
    let chat_value: serde_json::Value = serde_json::from_slice(&chat).expect("json");
    let tools = chat_value["tools"].as_array().expect("tools");
    assert!(tools.iter().all(|tool| tool["type"] == "function"));
    let offered: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str())
        .collect();
    assert_eq!(offered, vec!["multi_agent_v1__spawn_agent", "apply_patch"]);
    let chat_text = String::from_utf8(chat).expect("utf8");
    assert!(!chat_text.contains("web_search"));
    assert!(!chat_text.contains("namespace"));
}

#[test]
fn tool_search_for_the_client_becomes_a_function() {
    let request = OpenAiResponses
        .decode_request(
            br#"{"model":"m","input":"hi","tools":[
                {"type":"tool_search","name":"tool_search","execution":"client","parameters":{"type":"object"}},
                {"type":"tool_search","name":"tool_search","execution":"server","parameters":{"type":"object"}},
                {"type":"image_generation"}
            ]}"#,
        )
        .expect("decode");
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "tool_search");
    let dropped = request
        .extensions
        .get(hofund_codec::UNMAPPED_TOOLS)
        .expect("dropped");
    assert_eq!(
        dropped,
        &json!(["tool_search:tool_search", "image_generation"])
    );
}

#[test]
fn response_ids_are_prefixed_and_item_ids_are_not_reused() {
    let mut response = hofund_ir::Response {
        id: Some("chatcmpl-1".to_owned()),
        model: Some("m".to_owned()),
        message: hofund_ir::Message::new(Role::Assistant),
        stop: None,
        usage: None,
        extensions: hofund_ir::Extensions::new(),
    };
    response.message.content.push(ContentPart::text("ok"));
    let encoded = OpenAiResponses.encode_response(&response).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["id"], json!("resp_chatcmpl-1"));
    response.id = None;
    let encoded = OpenAiResponses.encode_response(&response).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["id"], json!("resp_local"));

    let events = vec![
        StreamEvent::MessageStart(hofund_ir::MessageStart {
            id: Some("chatcmpl-1".to_owned()),
            model: Some("m".to_owned()),
            usage: None,
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::ContentBlockStart(hofund_ir::ContentBlockStart {
            index: 0,
            block: hofund_ir::BlockStart::Text,
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::TextDelta(hofund_ir::TextDelta {
            index: 0,
            text: "a".to_owned(),
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::ContentBlockStop(hofund_ir::ContentBlockStop {
            index: 0,
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::ContentBlockStart(hofund_ir::ContentBlockStart {
            index: 0,
            block: hofund_ir::BlockStart::Text,
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::TextDelta(hofund_ir::TextDelta {
            index: 0,
            text: "b".to_owned(),
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::ContentBlockStop(hofund_ir::ContentBlockStop {
            index: 0,
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::MessageStop(hofund_ir::MessageStop::default()),
    ];
    let encoded = OpenAiResponses.encode_stream(&events).expect("stream");
    let text = String::from_utf8(encoded).expect("utf8");
    assert!(text.contains("\"id\":\"resp_chatcmpl-1\""), "{text}");
    let ids = message_ids(&text);
    assert_eq!(ids.len(), 2, "{text}");
    assert!(ids.iter().all(|id| id.starts_with("msg_")), "{ids:?}");
    assert_ne!(ids[0], ids[1]);
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
    let item_id = item_done["item"]["id"].as_str().expect("id");
    assert!(item_id.starts_with("msg_"), "{item_id}");
    assert_eq!(item_done["item"]["content"][0]["text"], json!("Hello"));
    let completed = frames
        .iter()
        .find(|frame| frame["type"] == "response.completed")
        .expect("completed");
    assert_eq!(
        completed["response"]["output"][0]["content"][0]["text"],
        json!("Hello")
    );
    assert_eq!(completed["response"]["output"][0]["id"], json!(item_id));
}

fn codex_request() -> Request {
    OpenAiResponses
        .decode_request(include_str!("fixtures/codex_0_164_responses.json").as_bytes())
        .expect("codex")
}

fn call_response(name: &str, arguments: &str) -> Response {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::ToolCall(ToolCallPart {
        id: "call_1".to_owned(),
        name: name.to_owned(),
        arguments: ToolArguments::from_text(arguments),
        extensions: Extensions::new(),
    }));
    Response {
        id: Some("resp_up".to_owned()),
        model: Some("demo".to_owned()),
        message,
        stop: None,
        usage: None,
        extensions: Extensions::new(),
    }
}

fn frames_of(bytes: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8(bytes.to_vec())
        .expect("utf8")
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .expect("data");
            serde_json::from_str(data).expect("json")
        })
        .collect()
}

fn encode_events(request: &Request, events: &[StreamEvent]) -> Vec<serde_json::Value> {
    let mut encoder = OpenAiResponses.stream_encoder_for(request);
    let mut bytes = Vec::new();
    for event in events {
        bytes.extend(encoder.push(event).expect("push"));
    }
    bytes.extend(encoder.finish().expect("finish"));
    frames_of(&bytes)
}

fn tool_events(index: u32, name: &str, arguments: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::MessageStart(MessageStart {
            id: Some("resp_up".to_owned()),
            model: Some("demo".to_owned()),
            usage: None,
            extensions: Extensions::new(),
        }),
        StreamEvent::ContentBlockStart(ContentBlockStart {
            index,
            block: BlockStart::ToolCall {
                id: "call_1".to_owned(),
                name: name.to_owned(),
            },
            extensions: Extensions::new(),
        }),
        StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
            index,
            partial_json: arguments.to_owned(),
            extensions: Extensions::new(),
        }),
        StreamEvent::ContentBlockStop(ContentBlockStop {
            index,
            extensions: Extensions::new(),
        }),
        StreamEvent::MessageStop(MessageStop::default()),
    ]
}

#[test]
fn messages_encode_of_a_codex_request_fills_max_tokens() {
    let encoded = AnthropicMessages
        .encode_request(&codex_request())
        .expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["max_tokens"], json!(16_384));
}

#[test]
fn codex_namespace_and_custom_calls_round_trip_through_chat_and_messages() {
    let request = codex_request();
    for (name, arguments, kind, field, raw) in [
        (
            "multi_agent_v1__spawn_agent",
            r#"{"task":"list"}"#,
            "function_call",
            "arguments",
            r#"{"task":"list"}"#,
        ),
        (
            "apply_patch",
            r#"{"input":"*** Begin Patch"}"#,
            "custom_tool_call",
            "input",
            "*** Begin Patch",
        ),
    ] {
        let response = call_response(name, arguments);
        restore_call(&OpenAiChat, &request, &response, kind, field, raw);
        restore_call(&AnthropicMessages, &request, &response, kind, field, raw);
    }
}

fn restore_call(
    codec: &dyn Codec,
    request: &Request,
    response: &Response,
    kind: &str,
    field: &str,
    raw: &str,
) {
    let upstream = codec.encode_response(response).expect("upstream");
    let decoded = codec.decode_response(&upstream).expect("decode");
    let client = OpenAiResponses
        .encode_response_for(request, &decoded)
        .expect("client");
    let value: serde_json::Value = serde_json::from_slice(&client).expect("json");
    let item = &value["output"][0];
    assert_eq!(item["type"], json!(kind), "{item}");
    assert_eq!(item[field], json!(raw), "{item}");
    assert_eq!(item["status"], json!("completed"), "{item}");
    let prefix = if kind == "custom_tool_call" {
        "ctc_"
    } else {
        "fc_"
    };
    assert!(item["id"].as_str().unwrap().starts_with(prefix), "{item}");
    if kind == "function_call" {
        assert_eq!(item["name"], json!("spawn_agent"));
        assert_eq!(item["namespace"], json!("multi_agent_v1"));
        assert_eq!(item["encrypted_function_args"], json!([]));
    } else {
        assert_eq!(item["name"], json!("apply_patch"));
        assert!(item.get("arguments").is_none(), "{item}");
    }
}

#[test]
fn codex_calls_restore_on_chat_and_messages_stream_indexes() {
    let request = codex_request();
    for index in [2_u32, 0] {
        let frames = encode_events(
            &request,
            &tool_events(index, "multi_agent_v1__spawn_agent", r#"{"task":"list"}"#),
        );
        let added = frames
            .iter()
            .find(|frame| frame["type"] == "response.output_item.added")
            .expect("added");
        assert_eq!(added["output_index"], json!(0), "{added}");
        assert_eq!(added["item"]["name"], json!("spawn_agent"));
        assert_eq!(added["item"]["namespace"], json!("multi_agent_v1"));
        let completed = frames
            .iter()
            .find(|frame| frame["type"] == "response.completed")
            .expect("completed");
        let item = &completed["response"]["output"][0];
        assert_eq!(item["name"], json!("spawn_agent"));
        assert_eq!(item["namespace"], json!("multi_agent_v1"));
        assert_eq!(item["encrypted_function_args"], json!([]));

        let frames = encode_events(
            &request,
            &tool_events(index, "apply_patch", r#"{"input":"*** Begin Patch"}"#),
        );
        let added = frames
            .iter()
            .find(|frame| frame["type"] == "response.output_item.added")
            .expect("added");
        assert_eq!(added["output_index"], json!(0));
        assert_eq!(added["item"]["type"], json!("custom_tool_call"));
        assert_eq!(added["item"]["name"], json!("apply_patch"));
        assert!(
            frames
                .iter()
                .any(|frame| frame["type"] == "response.custom_tool_call_input.delta")
        );
        let completed = frames
            .iter()
            .find(|frame| frame["type"] == "response.completed")
            .expect("completed");
        assert_eq!(
            completed["response"]["output"][0]["input"],
            json!("*** Begin Patch")
        );
        assert_eq!(
            completed["response"]["output"][0]["type"],
            json!("custom_tool_call")
        );
    }
}

#[test]
fn a_chat_tool_stream_uses_the_completed_output_index() {
    let request = Request::new("demo");
    let frames = encode_events(
        &request,
        &tool_events(2, "get_weather", r#"{"city":"Paris"}"#),
    );
    for frame in &frames {
        if frame.get("output_index").is_some() {
            assert_eq!(frame["output_index"], json!(0), "{frame}");
        }
    }
    let completed = frames
        .iter()
        .find(|frame| frame["type"] == "response.completed")
        .expect("completed");
    assert_eq!(completed["response"]["output"].as_array().unwrap().len(), 1);
    assert_eq!(
        completed["response"]["output"][0]["name"],
        json!("get_weather")
    );
}

#[test]
fn item_ids_differ_across_responses_and_upstream_ids_stay() {
    let mut response = Response {
        id: None,
        model: Some("m".to_owned()),
        message: Message::new(Role::Assistant),
        stop: None,
        usage: None,
        extensions: Extensions::new(),
    };
    response.message.content.push(ContentPart::text("one"));
    let first = OpenAiResponses.encode_response(&response).expect("first");
    response.message.content.clear();
    response.message.content.push(ContentPart::text("two"));
    let second = OpenAiResponses.encode_response(&response).expect("second");
    let first: serde_json::Value = serde_json::from_slice(&first).expect("json");
    let second: serde_json::Value = serde_json::from_slice(&second).expect("json");
    let left = first["output"][0]["id"].as_str().unwrap();
    let right = second["output"][0]["id"].as_str().unwrap();
    assert!(left.starts_with("msg_"), "{left}");
    assert!(right.starts_with("msg_"), "{right}");
    assert_ne!(left, right);
    assert_eq!(first["output"][0]["status"], json!("completed"));

    let kept = OpenAiResponses
        .decode_response(
            br#"{"id":"resp_up","status":"completed","output":[{"type":"message","id":"msg_keep","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hi"}]}]}"#,
        )
        .expect("decode");
    let encoded = OpenAiResponses.encode_response(&kept).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["output"][0]["id"], json!("msg_keep"));

    let events = OpenAiResponses
        .decode_stream(
            b"event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"msg_keep\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_up\",\"status\":\"completed\",\"output\":[]}}\n\n",
        )
        .expect("stream");
    let encoded = OpenAiResponses.encode_stream(&events).expect("encode");
    let text = String::from_utf8(encoded).expect("utf8");
    assert!(text.contains("\"id\":\"msg_keep\""), "{text}");
}
