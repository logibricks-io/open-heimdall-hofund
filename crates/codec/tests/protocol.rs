//! Snapshot, round-trip, cross-protocol, and streaming tests. No network.

use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_ir::{
    ContentPart, Extensions, ImageDetail, ImageSource, Message, Request, Response, Role, Stop,
    StopReason, StreamEvent, ToolArguments, ToolChoiceMode, Usage, assemble,
};
use serde::Deserialize;
use serde_json::json;

const ANTHROPIC_REQUEST: &str = include_str!("fixtures/anthropic_request.json");
const ANTHROPIC_RESPONSE: &str = include_str!("fixtures/anthropic_response.json");
const ANTHROPIC_STREAM: &[u8] = include_bytes!("fixtures/anthropic_stream.sse");
const OPENAI_REQUEST: &str = include_str!("fixtures/openai_request.json");
const OPENAI_RESPONSE: &str = include_str!("fixtures/openai_response.json");
const OPENAI_STREAM: &[u8] = include_bytes!("fixtures/openai_stream.sse");

fn pretty_json(bytes: &[u8]) -> String {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("json");
    serde_json::to_string_pretty(&value).expect("pretty")
}

macro_rules! pretty_value {
    ($value:expr) => {
        serde_json::to_string_pretty($value).expect("pretty")
    };
}

fn stable_request(codec: &impl Codec, bytes: &[u8]) -> Request {
    let first = codec.decode_request(bytes).expect("decode request");
    let encoded = codec.encode_request(&first).expect("encode request");
    let second = codec
        .decode_request(&encoded)
        .expect("decode encoded request");
    assert_eq!(first, second);
    first
}

fn stable_response(codec: &impl Codec, bytes: &[u8]) -> Response {
    let first = codec.decode_response(bytes).expect("decode response");
    let encoded = codec.encode_response(&first).expect("encode response");
    let second = codec
        .decode_response(&encoded)
        .expect("decode encoded response");
    assert_eq!(first, second);
    first
}

fn stable_stream(codec: &impl Codec, bytes: &[u8]) -> Vec<StreamEvent> {
    let first = codec.decode_stream(bytes).expect("decode stream");
    let encoded = codec.encode_stream(&first).expect("encode stream");
    let second = codec
        .decode_stream(&encoded)
        .expect("decode encoded stream");
    assert_eq!(first, second);
    first
}

#[test]
fn anthropic_request_round_trip_and_snapshot() {
    let request = stable_request(&AnthropicMessages, ANTHROPIC_REQUEST.as_bytes());
    insta::assert_snapshot!("anthropic_request_ir", pretty_value!(&request));
    let encoded = AnthropicMessages.encode_request(&request).expect("encode");
    insta::assert_snapshot!("anthropic_request_wire", pretty_json(&encoded));
    assert_eq!(
        request.extensions.get("metadata"),
        Some(&json!({"user_id": "user_1"}))
    );
    assert_eq!(request.params.top_k, Some(40));
}

#[test]
fn anthropic_response_round_trip_and_snapshot() {
    let response = stable_response(&AnthropicMessages, ANTHROPIC_RESPONSE.as_bytes());
    insta::assert_snapshot!("anthropic_response_ir", pretty_value!(&response));
    let encoded = AnthropicMessages
        .encode_response(&response)
        .expect("encode");
    insta::assert_snapshot!("anthropic_response_wire", pretty_json(&encoded));
    assert_eq!(
        response.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
    assert_eq!(
        response
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_write_tokens),
        Some(3)
    );
    assert_eq!(
        response
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_tokens),
        Some(4)
    );
}

#[test]
fn anthropic_stream_round_trip_and_snapshot() {
    let events = stable_stream(&AnthropicMessages, ANTHROPIC_STREAM);
    insta::assert_snapshot!("anthropic_stream_ir", pretty_value!(&events));
    let encoded = AnthropicMessages.encode_stream(&events).expect("encode");
    let text = String::from_utf8(encoded).expect("utf-8");
    insta::assert_snapshot!("anthropic_stream_wire", text);
    let response = assemble(&events);
    assert_eq!(response.message.content.len(), 4);
    match &response.message.content[2] {
        ContentPart::ToolCall(call) => {
            assert_eq!(call.name, "get_weather");
            assert_eq!(
                call.arguments,
                ToolArguments::Json {
                    value: json!({"city": "Paris"})
                }
            );
        }
        other => panic!("expected a tool call, got {other:?}"),
    }
}

#[test]
fn openai_request_round_trip_and_snapshot() {
    let request = stable_request(&OpenAiChat, OPENAI_REQUEST.as_bytes());
    insta::assert_snapshot!("openai_request_ir", pretty_value!(&request));
    let encoded = OpenAiChat.encode_request(&request).expect("encode");
    insta::assert_snapshot!("openai_request_wire", pretty_json(&encoded));
    assert_eq!(request.messages[1].role, Role::Developer);
    assert!(request.stream);
    assert_eq!(request.params.seed, Some(7));
    assert_eq!(
        request.messages[3].extensions.get("function_call"),
        Some(&serde_json::Value::Null)
    );
}

#[test]
fn openai_response_round_trip_and_snapshot() {
    let response = stable_response(&OpenAiChat, OPENAI_RESPONSE.as_bytes());
    insta::assert_snapshot!("openai_response_ir", pretty_value!(&response));
    let encoded = OpenAiChat.encode_response(&response).expect("encode");
    insta::assert_snapshot!("openai_response_wire", pretty_json(&encoded));
    assert_eq!(
        response.extensions.get("system_fingerprint"),
        Some(&json!("fp_1"))
    );
    assert_eq!(
        response
            .usage
            .as_ref()
            .and_then(|usage| usage.reasoning_tokens),
        Some(3)
    );
    assert_eq!(
        response
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_tokens),
        Some(4)
    );
    assert!(response.extensions.get("object").is_none());
}

#[test]
fn chat_usage_chunk_follows_the_client_flag() {
    let events = chat_turn();
    let quiet = Request::new("m");
    let mut encoder = OpenAiChat.stream_encoder_for(&quiet);
    let mut out = Vec::new();
    for event in &events {
        out.extend(encoder.push(event).expect("push"));
    }
    let text = String::from_utf8(out).expect("utf8");
    assert!(!text.contains("\"usage\""), "{text}");
    for line in text.lines().filter(|line| line.starts_with("data: {")) {
        assert!(line.contains("\"id\":\"chatcmpl-1\""), "{line}");
        assert!(line.contains("\"model\":\"m\""), "{line}");
    }

    let mut asking = Request::new("m");
    asking
        .extensions
        .insert("stream_options", json!({"include_usage": true}));
    let mut encoder = OpenAiChat.stream_encoder_for(&asking);
    let mut out = Vec::new();
    for event in &events {
        out.extend(encoder.push(event).expect("push"));
    }
    let text = String::from_utf8(out).expect("utf8");
    let frames: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("data: {"))
        .collect();
    let finish = frames
        .iter()
        .find(|line| line.contains("finish_reason"))
        .expect("finish");
    assert!(!finish.contains("\"usage\""), "{finish}");
    let usage = frames
        .iter()
        .find(|line| line.contains("\"usage\""))
        .expect("usage");
    assert!(usage.contains("\"choices\":[]"), "{usage}");
    assert!(usage.contains("\"id\":\"chatcmpl-1\""), "{usage}");
    assert!(usage.contains("\"model\":\"m\""), "{usage}");
}

fn chat_turn() -> Vec<StreamEvent> {
    vec![
        StreamEvent::MessageStart(hofund_ir::MessageStart {
            id: Some("chatcmpl-1".to_owned()),
            model: Some("m".to_owned()),
            usage: None,
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::TextDelta(hofund_ir::TextDelta {
            index: 0,
            text: "ok".to_owned(),
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::MessageDelta(hofund_ir::MessageDelta {
            stop: Some(hofund_ir::Stop {
                reason: StopReason::EndTurn,
                sequence: None,
            }),
            usage: Some(hofund_ir::Usage {
                input_tokens: Some(2),
                output_tokens: Some(1),
                total_tokens: Some(3),
                ..hofund_ir::Usage::default()
            }),
            extensions: hofund_ir::Extensions::new(),
        }),
        StreamEvent::MessageStop(hofund_ir::MessageStop::default()),
    ]
}

#[test]
fn anthropic_unary_id_uses_the_message_prefix() {
    let mut response = Response {
        id: None,
        model: Some("m".to_owned()),
        message: hofund_ir::Message::new(Role::Assistant),
        stop: None,
        usage: None,
        extensions: hofund_ir::Extensions::new(),
    };
    response.message.content.push(ContentPart::text("ok"));
    let encoded = AnthropicMessages
        .encode_response(&response)
        .expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["id"], json!("msg_local"));
    response.id = Some("abc".to_owned());
    let encoded = AnthropicMessages
        .encode_response(&response)
        .expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["id"], json!("msg_abc"));
}

#[test]
fn openai_stream_round_trip_and_snapshot() {
    let events = stable_stream(&OpenAiChat, OPENAI_STREAM);
    insta::assert_snapshot!("openai_stream_ir", pretty_value!(&events));
    let encoded = OpenAiChat.encode_stream(&events).expect("encode");
    let text = String::from_utf8(encoded).expect("utf-8");
    insta::assert_snapshot!("openai_stream_wire", text);
    let response = assemble(&events);
    assert_eq!(
        response.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
    match response.message.content.last() {
        Some(ContentPart::ToolCall(call)) => assert_eq!(call.id, "call_1"),
        other => panic!("expected a tool call, got {other:?}"),
    }
}

#[test]
fn cross_anthropic_request_through_chat() {
    let mut request = AnthropicMessages
        .decode_request(ANTHROPIC_REQUEST.as_bytes())
        .expect("decode");
    assert_eq!(request.params.top_k, Some(40));
    request.strip_extensions();
    let encoded = OpenAiChat.encode_request(&request).expect("encode");
    let via = OpenAiChat.decode_request(&encoded).expect("decode");
    assert!(via.params.top_k.is_none());
    assert_eq!(via.params.max_tokens, Some(256));
    assert_eq!(via.params.stop, vec!["END".to_owned()]);
    assert_eq!(via.messages[0].role, Role::System);
    assert!(matches!(
        &via.messages[3].content[0],
        ContentPart::ToolResult(result) if !result.is_error && result.id == "toolu_1"
    ));
    assert_eq!(via.messages[3].role, Role::Tool);
    match &via.messages[2].content[0] {
        ContentPart::Thinking(part) => {
            assert_eq!(part.text, "Check the tool.");
            assert!(part.signature.is_none());
            assert!(!part.redacted);
        }
        other => panic!("expected thinking, got {other:?}"),
    }
    assert!(
        via.messages[2]
            .content
            .iter()
            .all(|part| { !matches!(part, ContentPart::Thinking(thinking) if thinking.redacted) })
    );
    match &via.tool_choice {
        Some(choice) => assert_eq!(
            choice.mode,
            ToolChoiceMode::Named {
                name: "get_weather".to_owned()
            }
        ),
        None => panic!("missing tool choice"),
    }
}

#[test]
fn cross_chat_request_through_anthropic() {
    let mut request = OpenAiChat
        .decode_request(OPENAI_REQUEST.as_bytes())
        .expect("decode");
    assert_eq!(request.messages[1].role, Role::Developer);
    assert_eq!(request.params.seed, Some(7));
    request.strip_extensions();
    let encoded = AnthropicMessages.encode_request(&request).expect("encode");
    let via = AnthropicMessages.decode_request(&encoded).expect("decode");
    assert!(
        via.messages
            .iter()
            .all(|message| message.role != Role::Developer)
    );
    assert_eq!(via.messages[0].role, Role::System);
    assert_eq!(via.messages[0].content.len(), 2);
    assert!(via.params.seed.is_none());
    assert!(via.params.presence_penalty.is_none());
    assert!(via.params.frequency_penalty.is_none());
    assert_eq!(via.params.max_tokens, Some(256));
    assert!(via.stream);
    match &via.messages[1].content[1] {
        ContentPart::Image(image) => assert!(image.detail.is_none()),
        other => panic!("expected an image, got {other:?}"),
    }
    match &via.messages[1].content[2] {
        ContentPart::Image(image) => {
            assert_eq!(
                image.source,
                ImageSource::Base64 {
                    media_type: "image/png".to_owned(),
                    data: "aGVsbG8=".to_owned(),
                }
            );
        }
        other => panic!("expected a data image, got {other:?}"),
    }
    assert_eq!(via.messages[3].role, Role::User);
    assert!(matches!(
        via.messages[3].content[0],
        ContentPart::ToolResult(_)
    ));
    assert_eq!(
        via.tool_choice.as_ref().map(|choice| &choice.mode),
        Some(&ToolChoiceMode::Required)
    );
}

#[test]
fn cross_response_drops_protocol_only_usage_fields() {
    let mut anthropic = AnthropicMessages
        .decode_response(ANTHROPIC_RESPONSE.as_bytes())
        .expect("decode");
    anthropic.strip_extensions();
    let via = OpenAiChat
        .decode_response(&OpenAiChat.encode_response(&anthropic).expect("encode"))
        .expect("decode");
    let usage = via.usage.expect("usage");
    assert_eq!(usage.input_tokens, Some(25));
    assert_eq!(usage.output_tokens, Some(12));
    assert_eq!(usage.cache_read_tokens, Some(4));
    assert!(usage.cache_write_tokens.is_none());
    assert_eq!(usage.total_tokens, Some(37));
    match &via.message.content[0] {
        ContentPart::Thinking(part) => assert!(part.signature.is_none()),
        other => panic!("expected thinking, got {other:?}"),
    }

    let mut chat = OpenAiChat
        .decode_response(OPENAI_RESPONSE.as_bytes())
        .expect("decode");
    chat.strip_extensions();
    let via = AnthropicMessages
        .decode_response(&AnthropicMessages.encode_response(&chat).expect("encode"))
        .expect("decode");
    let usage = via.usage.expect("usage");
    assert_eq!(usage.cache_read_tokens, Some(4));
    assert!(usage.reasoning_tokens.is_none());
    assert!(usage.total_tokens.is_none());
    assert_eq!(
        via.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
}

#[test]
fn known_lossy_spellings() {
    let body = br#"{"id":"1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"no"},"finish_reason":"function_call"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
    let response = OpenAiChat.decode_response(body).expect("decode");
    assert_eq!(
        response.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::ToolUse)
    );
    let encoded = OpenAiChat.encode_response(&response).expect("encode");
    let wire: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(wire["choices"][0]["finish_reason"], json!("tool_calls"));

    let stopped = AnthropicMessages
        .decode_response(br#"{"role":"assistant","content":[{"type":"text","text":"x"}],"stop_reason":"pause_turn","stop_sequence":"END"}"#)
        .expect("decode");
    assert_eq!(
        stopped.stop,
        Some(hofund_ir::Stop {
            reason: StopReason::Other("pause_turn".to_owned()),
            sequence: Some("END".to_owned()),
        })
    );
    let encoded = AnthropicMessages.encode_response(&stopped).expect("encode");
    let wire: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(wire["stop_reason"], json!("pause_turn"));
    assert_eq!(wire["stop_sequence"], json!("END"));

    let both = br#"{"model":"m","messages":[],"max_tokens":10,"max_completion_tokens":20}"#;
    let request = OpenAiChat.decode_request(both).expect("decode");
    assert_eq!(request.params.max_tokens, Some(20));
    let encoded = OpenAiChat.encode_request(&request).expect("encode");
    let wire: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert!(wire.get("max_tokens").is_none());
    assert_eq!(wire["max_completion_tokens"], json!(20));
}

#[test]
fn chat_joins_assistant_text_and_stop_sequence_becomes_end_turn() {
    let mut message = hofund_ir::Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ab"));
    message.content.push(ContentPart::text("c"));
    let mut request = Request::new("m");
    request.messages.push(message);
    let encoded = OpenAiChat.encode_request(&request).expect("encode");
    let decoded = OpenAiChat.decode_request(&encoded).expect("decode");
    assert_eq!(decoded.messages[0].content, vec![ContentPart::text("abc")]);

    let mut response = Response {
        id: None,
        model: None,
        message: hofund_ir::Message::new(Role::Assistant),
        stop: Some(hofund_ir::Stop {
            reason: StopReason::StopSequence,
            sequence: Some("END".to_owned()),
        }),
        usage: None,
        extensions: hofund_ir::Extensions::new(),
    };
    response.message.content.push(ContentPart::text("x"));
    let encoded = OpenAiChat.encode_response(&response).expect("encode");
    let decoded = OpenAiChat.decode_response(&encoded).expect("decode");
    assert_eq!(
        decoded.stop.as_ref().map(|stop| &stop.reason),
        Some(&StopReason::EndTurn)
    );
    assert!(
        decoded
            .stop
            .as_ref()
            .is_some_and(|stop| stop.sequence.is_none())
    );
}

#[test]
fn image_detail_round_trip_is_chat_only() {
    let mut message = hofund_ir::Message::new(Role::User);
    message
        .content
        .push(ContentPart::Image(hofund_ir::ImagePart {
            source: ImageSource::Url {
                url: "https://example.com/a.png".to_owned(),
            },
            detail: Some(ImageDetail::High),
            extensions: hofund_ir::Extensions::new(),
        }));
    let mut request = Request::new("m");
    request.params.max_tokens = Some(8);
    request.messages.push(message);
    let chat = OpenAiChat
        .decode_request(&OpenAiChat.encode_request(&request).expect("encode"))
        .expect("decode");
    match &chat.messages[0].content[0] {
        ContentPart::Image(image) => assert_eq!(image.detail, Some(ImageDetail::High)),
        other => panic!("expected an image, got {other:?}"),
    }
    let anthropic = AnthropicMessages
        .decode_request(&AnthropicMessages.encode_request(&request).expect("encode"))
        .expect("decode");
    match &anthropic.messages[0].content[0] {
        ContentPart::Image(image) => assert!(image.detail.is_none()),
        other => panic!("expected an image, got {other:?}"),
    }
}

#[test]
fn streams_survive_byte_splits() {
    assert_eq!(
        replay(&AnthropicMessages, ANTHROPIC_STREAM, 1),
        AnthropicMessages.decode_stream(ANTHROPIC_STREAM).unwrap()
    );
    assert_eq!(
        replay(&OpenAiChat, OPENAI_STREAM, 1),
        OpenAiChat.decode_stream(OPENAI_STREAM).unwrap()
    );
    let third = ANTHROPIC_STREAM.len() / 3;
    let half = ANTHROPIC_STREAM.len() / 2;
    assert_eq!(
        replay_at(&AnthropicMessages, ANTHROPIC_STREAM, &[third, half]),
        AnthropicMessages.decode_stream(ANTHROPIC_STREAM).unwrap()
    );
    assert_eq!(
        replay_at(
            &OpenAiChat,
            OPENAI_STREAM,
            &[OPENAI_STREAM.len() / 3, OPENAI_STREAM.len() / 2]
        ),
        OpenAiChat.decode_stream(OPENAI_STREAM).unwrap()
    );
}

fn replay(codec: &impl Codec, bytes: &[u8], size: usize) -> Vec<StreamEvent> {
    let mut decoder = codec.stream_decoder();
    let mut events = Vec::new();
    for chunk in bytes.chunks(size) {
        events.extend(decoder.push(chunk).expect("push"));
    }
    events.extend(decoder.finish().expect("finish"));
    events
}

fn replay_at(codec: &impl Codec, bytes: &[u8], cuts: &[usize]) -> Vec<StreamEvent> {
    let mut decoder = codec.stream_decoder();
    let mut events = Vec::new();
    let mut start = 0;
    for cut in cuts {
        events.extend(decoder.push(&bytes[start..*cut]).expect("push"));
        start = *cut;
    }
    events.extend(decoder.push(&bytes[start..]).expect("push"));
    events.extend(decoder.finish().expect("finish"));
    events
}

#[test]
fn malformed_input_is_an_error_and_does_not_panic() {
    let samples: [&[u8]; 6] = [b"", b"   ", b"null", b"[]", b"{\"messages\":[]}", b"{"];
    for sample in samples {
        assert_err(|| AnthropicMessages.decode_request(sample));
        assert_err(|| OpenAiChat.decode_request(sample));
    }
    assert_err(|| {
        AnthropicMessages
            .decode_request(br#"{"model":"m","messages":[{"role":"user","content":1}]}"#)
    });
    assert_err(|| {
        OpenAiChat.decode_request(br#"{"model":"m","messages":[{"role":"nope","content":"x"}]}"#)
    });
    assert_err(|| AnthropicMessages.decode_stream(b"data: not-json\n\n"));
    assert_err(|| OpenAiChat.decode_stream(b"data: {]\n\n"));
    assert_err(|| OpenAiChat.decode_response(br#"{"choices":[{"index":0},{"index":1}]}"#));
    assert_err(|| {
        OpenAiChat.decode_request(
            br#"{"model":"m","messages":[],"tools":[{"type":"custom","function":{"name":"a"}}]}"#,
        )
    });
    let mut request = Request::new("m");
    request.messages.push(hofund_ir::Message::new(Role::User));
    let encoded = AnthropicMessages
        .encode_request(&request)
        .expect("default cap");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["max_tokens"], json!(16_384));
    let mut call = hofund_ir::Message::new(Role::Assistant);
    call.content
        .push(ContentPart::ToolCall(hofund_ir::ToolCallPart {
            id: "1".to_owned(),
            name: "a".to_owned(),
            arguments: ToolArguments::Raw {
                text: "not-json".to_owned(),
            },
            extensions: hofund_ir::Extensions::new(),
        }));
    request.params.max_tokens = Some(1);
    request.messages.push(call);
    assert!(AnthropicMessages.encode_request(&request).is_err());
}

#[test]
fn anthropic_stream_puts_usage_on_start_and_one_delta_after_stops() {
    use hofund_ir::{
        BlockStart, ContentBlockStart, ContentBlockStop, Extensions, MessageDelta, MessageStart,
        MessageStop, Stop, StopReason, TextDelta, Usage,
    };
    let events = vec![
        StreamEvent::MessageStart(MessageStart {
            id: Some("chatcmpl-abc".to_owned()),
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
            text: "ok".to_owned(),
            extensions: Extensions::new(),
        }),
        StreamEvent::MessageDelta(MessageDelta {
            stop: Some(Stop {
                reason: StopReason::EndTurn,
                sequence: None,
            }),
            usage: Some(Usage {
                input_tokens: Some(3),
                output_tokens: Some(2),
                ..Usage::default()
            }),
            extensions: Extensions::new(),
        }),
        StreamEvent::ContentBlockStop(ContentBlockStop {
            index: 0,
            extensions: Extensions::new(),
        }),
        StreamEvent::MessageStop(MessageStop::default()),
    ];
    let encoded = AnthropicMessages.encode_stream(&events).expect("encode");
    let text = String::from_utf8(encoded).expect("utf8");
    let names: Vec<&str> = text
        .split("\n\n")
        .filter_map(|frame| frame.lines().find_map(|line| line.strip_prefix("event: ")))
        .collect();
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "message_delta")
            .count(),
        1
    );
    let start = text
        .split("\n\n")
        .find(|frame| frame.contains("event: message_start"))
        .expect("start");
    let delta = text
        .split("\n\n")
        .find(|frame| frame.contains("event: message_delta"))
        .expect("delta");
    assert!(start.contains("\"id\":\"msg_chatcmpl-abc\""));
    assert!(start.contains("\"usage\":{\"input_tokens\":0,\"output_tokens\":0}"));
    assert!(delta.contains("\"usage\":{\"input_tokens\":3,\"output_tokens\":2}"));
    assert!(delta.contains("\"stop_reason\":\"end_turn\""));
}

fn assert_err<T: std::fmt::Debug, E: std::fmt::Debug>(
    decode: impl FnOnce() -> Result<T, E> + std::panic::UnwindSafe,
) {
    let caught = std::panic::catch_unwind(decode);
    let result = caught.expect("malformed input panicked");
    assert!(result.is_err());
}

#[test]
fn anthropic_stream_usage_is_merged_for_every_client() {
    let sse = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"m","usage":{"input_tokens":21,"output_tokens":0,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}

event: message_stop
data: {"type":"message_stop"}

"#;
    let events = AnthropicMessages
        .decode_stream(sse.as_bytes())
        .expect("decode");
    let start = events.iter().find_map(|event| match event {
        StreamEvent::MessageStart(start) => start.usage.as_ref(),
        _ => None,
    });
    let delta = events.iter().find_map(|event| match event {
        StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
        _ => None,
    });
    assert_eq!(start.and_then(|usage| usage.input_tokens), Some(21));
    assert_eq!(start.and_then(|usage| usage.output_tokens), Some(0));
    assert_eq!(start.and_then(|usage| usage.cache_read_tokens), Some(4));
    assert_eq!(start.and_then(|usage| usage.cache_write_tokens), Some(2));
    assert_eq!(delta.and_then(|usage| usage.output_tokens), Some(5));
    assert!(delta.and_then(|usage| usage.input_tokens).is_none());

    let responses = data_values(&OpenAiResponses.encode_stream(&events).expect("responses"));
    let created = responses
        .iter()
        .find(|frame| frame["type"] == "response.created")
        .expect("created");
    assert!(created["response"].get("usage").is_none(), "{created}");
    let completed = responses
        .iter()
        .find(|frame| frame["type"] == "response.completed")
        .expect("completed");
    assert_codex_usage(&completed["response"]["usage"], 21, 5, 26, 4, 0);

    let chat = data_values(&OpenAiChat.encode_stream(&events).expect("chat"));
    let role = chat
        .iter()
        .find(|frame| frame["choices"][0]["delta"]["role"] == "assistant")
        .expect("role");
    assert!(role.get("usage").is_none(), "{role}");
    let usages: Vec<_> = chat
        .iter()
        .filter(|frame| frame.get("usage").is_some())
        .collect();
    assert_eq!(usages.len(), 1, "{usages:?}");
    assert!(usages[0]["choices"].as_array().expect("choices").is_empty());
    assert_eq!(usages[0]["usage"]["prompt_tokens"], json!(21));
    assert_eq!(usages[0]["usage"]["completion_tokens"], json!(5));
    assert_eq!(usages[0]["usage"]["total_tokens"], json!(26));
    assert_eq!(
        usages[0]["usage"]["prompt_tokens_details"]["cached_tokens"],
        json!(4)
    );
    assert!(
        usages[0]["usage"]
            .get("completion_tokens_details")
            .is_none()
    );

    let gemini = data_values(&GenerativeLanguage.encode_stream(&events).expect("gemini"));
    let metas: Vec<_> = gemini
        .iter()
        .filter(|frame| frame.get("usageMetadata").is_some())
        .collect();
    assert_eq!(metas.len(), 1, "{metas:?}");
    assert_eq!(metas[0]["usageMetadata"]["promptTokenCount"], json!(21));
    assert_eq!(metas[0]["usageMetadata"]["candidatesTokenCount"], json!(5));
    assert_eq!(metas[0]["usageMetadata"]["totalTokenCount"], json!(26));
    assert_eq!(
        metas[0]["usageMetadata"]["cachedContentTokenCount"],
        json!(4)
    );
    assert!(
        metas[0]["usageMetadata"]
            .get("thoughtsTokenCount")
            .is_none()
    );

    let mut quiet = events;
    for event in &mut quiet {
        if let StreamEvent::MessageDelta(delta) = event {
            delta.usage = None;
        }
    }
    let completed = data_values(&OpenAiResponses.encode_stream(&quiet).expect("responses"))
        .into_iter()
        .find(|frame| frame["type"] == "response.completed")
        .expect("completed");
    assert_codex_usage(&completed["response"]["usage"], 21, 0, 21, 4, 0);
    let chat = data_values(&OpenAiChat.encode_stream(&quiet).expect("chat"));
    let usage = chat
        .iter()
        .find(|frame| frame.get("usage").is_some())
        .expect("usage");
    assert_eq!(usage["usage"]["prompt_tokens"], json!(21));
    assert_eq!(usage["usage"]["completion_tokens"], json!(0));
    assert_eq!(usage["usage"]["total_tokens"], json!(21));

    let fixture = AnthropicMessages
        .decode_stream(ANTHROPIC_STREAM)
        .expect("fixture");
    let completed = data_values(&OpenAiResponses.encode_stream(&fixture).expect("responses"))
        .into_iter()
        .find(|frame| frame["type"] == "response.completed")
        .expect("completed");
    assert_codex_usage(&completed["response"]["usage"], 25, 12, 37, 0, 0);
}

#[test]
fn partial_usage_fills_the_fields_each_protocol_requires() {
    let response = Response {
        id: Some("resp_local".to_owned()),
        model: Some("m".to_owned()),
        message: {
            let mut message = Message::new(Role::Assistant);
            message.content.push(ContentPart::text("ok"));
            message
        },
        stop: Some(Stop {
            reason: StopReason::EndTurn,
            sequence: None,
        }),
        usage: Some(Usage {
            output_tokens: Some(5),
            ..Usage::default()
        }),
        extensions: Extensions::new(),
    };
    let responses = OpenAiResponses
        .encode_response(&response)
        .expect("responses");
    let responses: serde_json::Value = serde_json::from_slice(&responses).expect("json");
    assert_codex_usage(&responses["usage"], 0, 5, 5, 0, 0);
    let chat = OpenAiChat.encode_response(&response).expect("chat");
    let chat: serde_json::Value = serde_json::from_slice(&chat).expect("json");
    assert_eq!(chat["usage"]["prompt_tokens"], json!(0));
    assert_eq!(chat["usage"]["completion_tokens"], json!(5));
    assert_eq!(chat["usage"]["total_tokens"], json!(5));
    let gemini = GenerativeLanguage
        .encode_response(&response)
        .expect("gemini");
    let gemini: serde_json::Value = serde_json::from_slice(&gemini).expect("json");
    assert_eq!(gemini["usageMetadata"]["promptTokenCount"], json!(0));
    assert_eq!(gemini["usageMetadata"]["candidatesTokenCount"], json!(5));
    assert_eq!(gemini["usageMetadata"]["totalTokenCount"], json!(5));

    let mut absent = response;
    absent.usage = None;
    let responses = OpenAiResponses.encode_response(&absent).expect("responses");
    let responses: serde_json::Value = serde_json::from_slice(&responses).expect("json");
    assert!(responses.get("usage").is_none(), "{responses}");
}

fn assert_codex_usage(
    usage: &serde_json::Value,
    input: u64,
    output: u64,
    total: u64,
    cached: u64,
    reasoning: u64,
) {
    let parsed: CodexUsage = serde_json::from_value(usage.clone()).unwrap_or_else(|error| {
        panic!("Codex rejects this usage ({error}): {usage}");
    });
    assert_eq!(parsed.input_tokens, input);
    assert_eq!(parsed.output_tokens, output);
    assert_eq!(parsed.total_tokens, total);
    assert_eq!(parsed.input_tokens_details.cached_tokens, cached);
    assert!(
        usage["input_tokens_details"]
            .get("cache_write_tokens")
            .is_none(),
        "cache_write_tokens is not a Responses field: {usage}"
    );
    assert_eq!(parsed.input_tokens_details.cache_write_tokens, 0);
    assert_eq!(parsed.output_tokens_details.reasoning_tokens, reasoning);
    assert!(
        serde_json::from_value::<CodexUsage>(json!({
            "output_tokens": 1,
            "total_tokens": 1,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<CodexUsage>(json!({
            "input_tokens": 1,
            "output_tokens": 1,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens_details": {"reasoning_tokens": 0}
        }))
        .is_err()
    );
}

fn data_values(bytes: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    text.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap_or_else(|error| panic!("{error}: {data}")))
        .collect()
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    input_tokens_details: CodexInputDetails,
    output_tokens_details: CodexOutputDetails,
}

#[derive(Debug, Deserialize)]
struct CodexInputDetails {
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CodexOutputDetails {
    reasoning_tokens: u64,
}

proptest::proptest! {
    #[test]
    fn user_text_round_trips(
        text in "[a-zA-Z0-9 ]{0,40}",
        max_tokens in 1u64..400,
        temperature in proptest::prop_oneof![
            proptest::prelude::Just(0.0),
            proptest::prelude::Just(0.5),
            proptest::prelude::Just(1.0),
        ],
    ) {
        let mut request = Request::new("demo");
        request.params.max_tokens = Some(max_tokens);
        request.params.temperature = Some(temperature);
        let mut message = hofund_ir::Message::new(Role::User);
        message.content.push(ContentPart::text(text));
        request.messages.push(message);
        assert_sampled_request(&AnthropicMessages, &request)?;
        assert_sampled_request(&OpenAiChat, &request)?;
    }

    #[test]
    fn extension_key_survives_both_codecs(suffix in "[a-z]{1,12}") {
        let key = format!("x_{suffix}");
        let mut request = Request::new("demo");
        request.params.max_tokens = Some(8);
        request.messages.push(hofund_ir::Message::new(Role::User));
        request.extensions.insert(key.clone(), json!(true));
        assert_extension(&AnthropicMessages, &request, &key)?;
        assert_extension(&OpenAiChat, &request, &key)?;
    }

    #[test]
    fn stream_fixtures_survive_random_splits(
        anthropic_mask in proptest::collection::vec(proptest::prelude::any::<bool>(), ANTHROPIC_STREAM.len()),
        openai_mask in proptest::collection::vec(proptest::prelude::any::<bool>(), OPENAI_STREAM.len()),
    ) {
        proptest::prop_assert_eq!(replay_mask(&AnthropicMessages, ANTHROPIC_STREAM, &anthropic_mask), AnthropicMessages.decode_stream(ANTHROPIC_STREAM).unwrap());
        proptest::prop_assert_eq!(replay_mask(&OpenAiChat, OPENAI_STREAM, &openai_mask), OpenAiChat.decode_stream(OPENAI_STREAM).unwrap());
    }
}

fn assert_sampled_request(
    codec: &impl Codec,
    request: &Request,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let encoded = codec.encode_request(request).unwrap();
    let decoded = codec.decode_request(&encoded).unwrap();
    proptest::prop_assert_eq!(&decoded.messages, &request.messages);
    proptest::prop_assert_eq!(decoded.params.max_tokens, request.params.max_tokens);
    proptest::prop_assert_eq!(decoded.params.temperature, request.params.temperature);
    Ok(())
}

fn assert_extension(
    codec: &impl Codec,
    request: &Request,
    key: &str,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let encoded = codec.encode_request(request).unwrap();
    let decoded = codec.decode_request(&encoded).unwrap();
    proptest::prop_assert_eq!(decoded.extensions.get(key), Some(&json!(true)));
    Ok(())
}

fn replay_mask(codec: &impl Codec, bytes: &[u8], mask: &[bool]) -> Vec<StreamEvent> {
    let mut cuts = Vec::new();
    for (index, split) in mask.iter().copied().enumerate() {
        if split && index > 0 {
            cuts.push(index);
        }
    }
    replay_at(codec, bytes, &cuts)
}
