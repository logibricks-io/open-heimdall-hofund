//! D-263: a provider that reports top-level `reasoning_tokens` on Chat usage.

use hofund_codec::{Codec, OpenAiChat};
use hofund_ir::{StreamEvent, Usage};
use serde_json::json;

fn whole(usage: &str) -> Usage {
    let body = format!(
        r#"{{"id":"c","object":"chat.completion","model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":"hello"}},"finish_reason":"stop"}}],"usage":{usage}}}"#
    );
    OpenAiChat
        .decode_response(body.as_bytes())
        .expect("decode")
        .usage
        .expect("usage")
}

fn stream_usage(usage: &str) -> Usage {
    let bytes = format!(
        "data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"hello\"}}}}]}}\n\ndata: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{usage}}}\n\ndata: [DONE]\n\n"
    );
    let events = OpenAiChat
        .decode_stream(bytes.as_bytes())
        .expect("decode stream");
    events
        .into_iter()
        .find_map(|event| match event {
            StreamEvent::MessageDelta(delta) => delta.usage,
            _ => None,
        })
        .expect("stream usage")
}

const TOP_LEVEL: &str = r#"{"completion_tokens":2359,"prompt_tokens":107,"prompt_tokens_details":{"cached_tokens":0},"total_tokens":2466,"reasoning_tokens":2351}"#;

#[test]
fn d263_whole_reply_counts_top_level_reasoning_tokens() {
    let usage = whole(TOP_LEVEL);
    assert_eq!(usage.output_tokens, Some(2359));
    assert_eq!(usage.input_tokens, Some(107));
    assert_eq!(usage.reasoning_tokens, Some(2351));
    assert_eq!(usage.cache_read_tokens, Some(0));
    assert!(usage.extensions.get("reasoning_tokens").is_none());
}

#[test]
fn d263_stream_counts_top_level_reasoning_tokens() {
    let usage = stream_usage(TOP_LEVEL);
    assert_eq!(usage.output_tokens, Some(2359));
    assert_eq!(usage.reasoning_tokens, Some(2351));
    assert!(usage.extensions.get("reasoning_tokens").is_none());
}

#[test]
fn d263_top_level_without_prompt_details_counts_as_reasoning() {
    let usage = whole(
        r#"{"prompt_tokens":11,"completion_tokens":88,"total_tokens":99,"reasoning_tokens":48}"#,
    );
    assert_eq!(usage.output_tokens, Some(88));
    assert_eq!(usage.reasoning_tokens, Some(48));
    let streamed = stream_usage(
        r#"{"prompt_tokens":11,"completion_tokens":88,"total_tokens":99,"reasoning_tokens":48}"#,
    );
    assert_eq!(streamed.reasoning_tokens, Some(48));
    assert_eq!(streamed.output_tokens, Some(88));
}

#[test]
fn d263_details_win_and_are_not_added_to_the_top_level() {
    let raw = r#"{"prompt_tokens":3,"completion_tokens":12,"total_tokens":15,"reasoning_tokens":48,"completion_tokens_details":{"reasoning_tokens":3}}"#;
    let usage = whole(raw);
    assert_eq!(usage.reasoning_tokens, Some(3));
    assert_eq!(usage.output_tokens, Some(12));
    assert!(usage.extensions.get("reasoning_tokens").is_none());
    let streamed = stream_usage(raw);
    assert_eq!(streamed.reasoning_tokens, Some(3));
}

#[test]
fn d263_zero_details_falls_back_to_top_level_reasoning_tokens() {
    let raw = r#"{"prompt_tokens":3,"completion_tokens":12,"total_tokens":15,"reasoning_tokens":48,"prompt_tokens_details":{"cached_tokens":1},"completion_tokens_details":{"reasoning_tokens":0,"accepted_prediction_tokens":2}}"#;
    let usage = whole(raw);
    assert_eq!(usage.reasoning_tokens, Some(48));
    assert_eq!(usage.cache_read_tokens, Some(1));
    assert_eq!(
        usage.extensions.get("accepted_prediction_tokens"),
        Some(&json!(2))
    );
    assert_eq!(stream_usage(raw).reasoning_tokens, Some(48));
}

#[test]
fn d263_encode_writes_the_count_only_under_details() {
    let body = format!(
        r#"{{"id":"c","object":"chat.completion","model":"m","choices":[{{"index":0,"message":{{"role":"assistant","content":"hello"}},"finish_reason":"stop"}}],"usage":{TOP_LEVEL}}}"#
    );
    let response = OpenAiChat.decode_response(body.as_bytes()).expect("decode");
    let encoded = OpenAiChat.encode_response(&response).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert!(value["usage"].get("reasoning_tokens").is_none());
    assert_eq!(
        value["usage"]["completion_tokens_details"]["reasoning_tokens"],
        json!(2351)
    );
    let again = OpenAiChat
        .decode_response(&encoded)
        .expect("decode again")
        .usage
        .expect("usage");
    assert_eq!(again.reasoning_tokens, Some(2351));
    assert_eq!(again.output_tokens, Some(2359));
}
