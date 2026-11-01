//! U-1a usage extraction per protocol.

use hofund_codec::{
    AnthropicMessages, Codec, Converse, GenerativeLanguage, OpenAiChat, OpenAiResponses,
};
use hofund_ir::normalize_cache_write;

fn usage_of(codec: &dyn Codec, body: &[u8]) -> hofund_ir::Usage {
    codec
        .decode_response(body)
        .expect("decode")
        .usage
        .expect("usage")
}

#[test]
fn u1a_3_anthropic_splits_cache_creation_ttl() {
    let body = br#"{"id":"m","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":4,"cache_creation_input_tokens":3000,"cache_creation":{"ephemeral_5m_input_tokens":1000,"ephemeral_1h_input_tokens":2000}}}"#;
    let usage = usage_of(&AnthropicMessages, body);
    assert_eq!(usage.cache_read_tokens, Some(4));
    assert_eq!(usage.cache_write_tokens, Some(3000));
    assert_eq!(usage.cache_write_5m_tokens, Some(1000));
    assert_eq!(usage.cache_write_1h_tokens, Some(2000));
}

#[test]
fn u1a_4_anthropic_total_cache_write_counts_as_five_minute() {
    let body = br#"{"id":"m","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":300}}"#;
    let usage = usage_of(&AnthropicMessages, body);
    assert_eq!(usage.cache_write_tokens, Some(300));
    assert_eq!(usage.cache_write_5m_tokens, Some(300));
    assert_eq!(usage.cache_write_1h_tokens, Some(0));
}

#[test]
fn u1a_5_openai_chat_reads_reasoning_and_cache_read() {
    let body = br#"{"id":"c","object":"chat.completion","model":"m","choices":[],"usage":{"prompt_tokens":25,"completion_tokens":12,"total_tokens":37,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":3}}}"#;
    let usage = usage_of(&OpenAiChat, body);
    assert_eq!(usage.cache_read_tokens, Some(4));
    assert_eq!(usage.reasoning_tokens, Some(3));
    assert!(usage.cache_write_tokens.is_none());
}

#[test]
fn u1a_6_openai_responses_reads_cache_write_and_reasoning() {
    let body = br#"{"id":"r","model":"m","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18,"input_tokens_details":{"cached_tokens":4,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":3}}}"#;
    let usage = usage_of(&OpenAiResponses, body);
    assert_eq!(usage.cache_read_tokens, Some(4));
    assert_eq!(usage.cache_write_tokens, Some(2));
    assert_eq!(usage.cache_write_5m_tokens, Some(2));
    assert_eq!(usage.cache_write_1h_tokens, Some(0));
    assert_eq!(usage.reasoning_tokens, Some(3));
}

#[test]
fn u1a_7_generative_reads_thoughts_as_reasoning() {
    let body = br#"{"responseId":"g","modelVersion":"m","candidates":[],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7,"totalTokenCount":18,"cachedContentTokenCount":4,"thoughtsTokenCount":2}}"#;
    let usage = usage_of(&GenerativeLanguage, body);
    assert_eq!(usage.cache_read_tokens, Some(4));
    assert_eq!(usage.reasoning_tokens, Some(2));
}

#[test]
fn u1a_8_converse_cache_write_counts_as_five_minute() {
    let body = br#"{"output":{"message":{"role":"assistant","content":[{"text":"hi"}]}},"stopReason":"end_turn","usage":{"inputTokens":3,"outputTokens":4,"totalTokens":7,"cacheReadInputTokens":1,"cacheWriteInputTokens":2}}"#;
    let usage = usage_of(&Converse, body);
    assert_eq!(usage.cache_read_tokens, Some(1));
    assert_eq!(usage.cache_write_tokens, Some(2));
    assert_eq!(usage.cache_write_5m_tokens, Some(2));
    assert_eq!(usage.cache_write_1h_tokens, Some(0));
}

#[test]
fn u1a_9_normalize_cache_write_hour_only() {
    let (total, five, hour) = normalize_cache_write(None, Some(0), Some(4000));
    assert_eq!(total, Some(4000));
    assert_eq!(five, Some(0));
    assert_eq!(hour, Some(4000));
}

#[test]
fn u1a_11_normalize_cache_write_total_never_below_its_parts() {
    let (total, five, hour) = normalize_cache_write(Some(10), Some(20), Some(30));
    assert_eq!(total, Some(50));
    assert_eq!(five, Some(20));
    assert_eq!(hour, Some(30));
}

#[test]
fn u1a_12_anthropic_encode_writes_the_ttl_split() {
    let body = br#"{"id":"m","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":3000,"cache_creation":{"ephemeral_5m_input_tokens":1000,"ephemeral_1h_input_tokens":2000}}}"#;
    let response = AnthropicMessages.decode_response(body).expect("decode");
    let encoded = AnthropicMessages
        .encode_response(&response)
        .expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["usage"]["cache_creation_input_tokens"], 3000);
    assert_eq!(
        value["usage"]["cache_creation"]["ephemeral_5m_input_tokens"],
        1000
    );
    assert_eq!(
        value["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
        2000
    );
}
