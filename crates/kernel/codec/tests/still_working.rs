//! StillWorking keepalive encoding and empty-stream classification.

use hofund_codec::{AnthropicMessages, Codec, OpenAiChat, OpenAiResponses, responses_empty};
use hofund_ir::{MessageStart, StillWorking, StreamEvent, Usage};

fn start(id: &str, model: &str, input_tokens: Option<u64>) -> StreamEvent {
    StreamEvent::MessageStart(MessageStart {
        id: Some(id.to_owned()),
        model: Some(model.to_owned()),
        usage: input_tokens.map(|n| Usage {
            input_tokens: Some(n),
            ..Usage::default()
        }),
        extensions: Default::default(),
    })
}

fn still() -> StreamEvent {
    StreamEvent::StillWorking(StillWorking::default())
}

fn sse_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn anthropic_emits_comment_before_start_and_ping_after() {
    let first = AnthropicMessages
        .encode_stream(&[still()])
        .expect("still before start");
    let text = sse_text(&first);
    assert!(text.contains(": keepalive"), "{text}");
    assert!(!text.contains("event: ping"), "{text}");

    let after = AnthropicMessages
        .encode_stream(&[start("msg_ph", "placeholder", None), still()])
        .expect("still after start");
    let text = sse_text(&after);
    assert!(text.contains("event: message_start"), "{text}");
    assert!(text.contains("event: ping"), "{text}");
    // A second tick in the same second is an SSE comment, not another ping.
    let twice = AnthropicMessages
        .encode_stream(&[start("msg_ph", "placeholder", None), still(), still()])
        .expect("throttled");
    let text = sse_text(&twice);
    assert_eq!(text.matches("event: ping").count(), 1, "{text}");
    assert!(text.contains(": keepalive"), "{text}");
}

#[test]
fn encoders_suppress_a_second_message_start_and_keep_its_usage() {
    let events = [
        start("msg_ph", "placeholder", None),
        start("msg_real", "claude-sonnet-4-5", Some(21)),
    ];

    let anthropic = sse_text(&AnthropicMessages.encode_stream(&events).expect("anthropic"));
    assert_eq!(anthropic.matches("event: message_start").count(), 1);
    assert!(anthropic.contains("msg_ph"), "{anthropic}");
    assert!(!anthropic.contains("msg_real"), "{anthropic}");
    assert!(!anthropic.contains("claude-sonnet-4-5"), "{anthropic}");

    let chat = sse_text(&OpenAiChat.encode_stream(&events).expect("chat"));
    assert_eq!(chat.matches("\"role\":\"assistant\"").count(), 1, "{chat}");

    let responses = sse_text(&OpenAiResponses.encode_stream(&events).expect("responses"));
    assert_eq!(
        responses.matches("event: response.created").count(),
        1,
        "{responses}"
    );
    assert!(responses.contains("resp_msg_ph"), "{responses}");
    assert!(!responses.contains("msg_real"), "{responses}");
    assert!(responses.contains("21"), "{responses}");
}

#[test]
fn chat_still_working_is_an_sse_comment_only() {
    let text = sse_text(
        &OpenAiChat
            .encode_stream(&[start("c", "m", None), still()])
            .expect("chat"),
    );
    assert!(text.contains(": keepalive"), "{text}");
    assert!(!text.contains("\"type\":\"ping\""), "{text}");
}

#[test]
fn responses_still_working_repeats_in_progress() {
    let text = sse_text(
        &OpenAiResponses
            .encode_stream(&[start("r", "m", None), still()])
            .expect("responses"),
    );
    assert!(text.contains("response.in_progress"), "{text}");
}

#[test]
fn a_buffer_of_only_still_working_and_a_placeholder_start_is_not_an_empty_reply() {
    let events = [start("msg_ph", "placeholder", None), still()];
    assert!(
        !responses_empty::decoded_stream_empty(&events),
        "placeholder + StillWorking must not look like an empty decoded reply"
    );
}
