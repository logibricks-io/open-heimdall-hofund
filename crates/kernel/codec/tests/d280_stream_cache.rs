//! D-280: a Chat or Generative Language stream that names its prompt on more
//! than one chunk, and the cache on only some of them, tells that cache once.
//!
//! The Responses client is told the whole prompt beside the cache. The prompt
//! is not added to the cache again, and a later zero does not drop the prompt
//! the reply already named.

use hofund_codec::{Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_ir::{StreamEvent, Usage};
use serde_json::{Value, json};

const WHOLE_USAGE: &str = r#"{"prompt_tokens":2006,"completion_tokens":300,"total_tokens":2306,"prompt_tokens_details":{"cached_tokens":1920,"audio_tokens":0},"completion_tokens_details":{"reasoning_tokens":0,"audio_tokens":0,"accepted_prediction_tokens":0,"rejected_prediction_tokens":0}}"#;

fn chat_stream(chunks: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(chunk);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

fn chat_shapes() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "one usage chunk",
            chat_stream(&[
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}],"usage":null}"#,
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":null}"#,
                &format!(
                    r#"{{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[],"usage":{WHOLE_USAGE}}}"#
                ),
            ]),
        ),
        (
            "cache on the first count only",
            chat_stream(&[
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":2006,"completion_tokens":0,"total_tokens":2006,"prompt_tokens_details":{"cached_tokens":1920}}}"#,
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2006,"completion_tokens":300,"total_tokens":2306}}"#,
            ]),
        ),
        (
            "prompt first, cache last",
            chat_stream(&[
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":2006,"completion_tokens":0,"total_tokens":2006}}"#,
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":300,"total_tokens":300,"prompt_tokens_details":{"cached_tokens":1920}}}"#,
            ]),
        ),
        (
            "cache zeroed on the last count",
            chat_stream(&[
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":2006,"completion_tokens":0,"total_tokens":2006,"prompt_tokens_details":{"cached_tokens":1920}}}"#,
                r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":2006,"completion_tokens":300,"total_tokens":2306,"prompt_tokens_details":{"cached_tokens":0}}}"#,
            ]),
        ),
    ]
}

fn merged(events: &[StreamEvent]) -> Usage {
    let mut usage = None;
    for event in events {
        let patch = match event {
            StreamEvent::MessageStart(start) => start.usage.as_ref(),
            StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
            _ => None,
        };
        Usage::merge_patch(&mut usage, patch);
    }
    usage.expect("usage")
}

fn completed_usage(bytes: &[u8]) -> Value {
    let text = String::from_utf8_lossy(bytes);
    for block in text.split("\n\n") {
        let Some(data) = block.lines().find_map(|line| line.strip_prefix("data:")) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if value["type"] == "response.completed" {
            return value["response"]["usage"].clone();
        }
    }
    panic!("no completed usage in {text}");
}

fn assert_responses_prompt(usage: &Value) {
    assert_eq!(usage["input_tokens"], json!(2006), "{usage}");
    assert_eq!(usage["output_tokens"], json!(300), "{usage}");
    assert_eq!(usage["total_tokens"], json!(2306), "{usage}");
    assert_eq!(
        usage["input_tokens_details"]["cached_tokens"],
        json!(1920),
        "{usage}"
    );
}

#[test]
fn d280_chat_stream_tells_the_cache_once_on_responses() {
    for (name, bytes) in chat_shapes() {
        let events = OpenAiChat.decode_stream(&bytes).expect(name);
        let usage = merged(&events);
        assert_eq!(usage.input_tokens, Some(2006), "{name}");
        assert_eq!(usage.output_tokens, Some(300), "{name}");
        assert_eq!(usage.cache_read_tokens, Some(1920), "{name}");
        assert_eq!(
            usage.input_tokens.unwrap_or(0) + usage.cache_read_tokens.unwrap_or(0),
            2006 + 1920,
            "{name} must not add the cache onto the prompt"
        );
        let encoded = OpenAiResponses.encode_stream(&events).expect(name);
        assert_responses_prompt(&completed_usage(&encoded));
    }
}

fn generative_stream(chunks: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for chunk in chunks {
        body.push_str("data: ");
        body.push_str(chunk);
        body.push_str("\n\n");
    }
    body.into_bytes()
}

#[test]
fn d280_generative_stream_tells_the_cache_once_on_responses() {
    let shapes = [
        (
            "cache on the first count only",
            generative_stream(&[
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Read"}]}}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":1,"cachedContentTokenCount":40}}"#,
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ing."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10}}"#,
            ]),
        ),
        (
            "prompt first, cache last",
            generative_stream(&[
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Read"}]}}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":1}}"#,
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ing."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":0,"candidatesTokenCount":10,"cachedContentTokenCount":40}}"#,
            ]),
        ),
        (
            "cache zeroed on the last count",
            generative_stream(&[
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"Read"}]}}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":1,"cachedContentTokenCount":40}}"#,
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"ing."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10,"cachedContentTokenCount":0}}"#,
            ]),
        ),
    ];
    for (name, bytes) in shapes {
        let events = GenerativeLanguage.decode_stream(&bytes).expect(name);
        let usage = merged(&events);
        assert_eq!(usage.input_tokens, Some(100), "{name}");
        assert_eq!(usage.output_tokens, Some(10), "{name}");
        assert_eq!(usage.cache_read_tokens, Some(40), "{name}");
        let encoded = OpenAiResponses.encode_stream(&events).expect(name);
        let told = completed_usage(&encoded);
        assert_eq!(told["input_tokens"], json!(100), "{name}: {told}");
        assert_eq!(told["output_tokens"], json!(10), "{name}: {told}");
        assert_eq!(
            told["input_tokens_details"]["cached_tokens"],
            json!(40),
            "{name}: {told}"
        );
        assert_ne!(told["input_tokens"], json!(140), "{name}");
    }
}

#[test]
fn d280_a_later_zero_does_not_drop_cache_write() {
    let bytes = chat_stream(&[
        r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"}}],"usage":{"prompt_tokens":100,"completion_tokens":0,"total_tokens":100,"prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":20}}}"#,
        r#"{"id":"c1","object":"chat.completion.chunk","model":"m1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":0,"completion_tokens":5,"total_tokens":5}}"#,
    ]);
    let events = OpenAiChat.decode_stream(&bytes).expect("stream");
    let usage = merged(&events);
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.output_tokens, Some(5));
    assert_eq!(usage.cache_read_tokens, Some(30));
    assert_eq!(usage.cache_write_tokens, Some(20));
    assert_eq!(usage.total_tokens, Some(105));
}
