//! Reasoning effort is one IR field and one mapping table per codec.

use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, OpenAiResponses};
use hofund_ir::{Effort, Request};
use serde_json::json;

fn decode_effort(codec: &impl Codec, bytes: &[u8]) -> Option<Effort> {
    codec.decode_request(bytes).expect("decode").params.effort
}

fn encoded(codec: &impl Codec, effort: Effort) -> serde_json::Value {
    let mut request = Request::new("demo");
    request.params.max_tokens = Some(16);
    request.params.effort = Some(effort);
    let bytes = codec.encode_request(&request).expect("encode");
    serde_json::from_slice(&bytes).expect("json")
}

#[test]
fn each_codec_reads_its_own_spelling() {
    assert_eq!(
        decode_effort(
            &OpenAiChat,
            br#"{"model":"demo","messages":[],"reasoning_effort":"xhigh"}"#
        ),
        Some(Effort::Xhigh)
    );
    assert_eq!(
        decode_effort(
            &OpenAiResponses,
            br#"{"model":"demo","input":"hi","reasoning":{"effort":"ultra"}}"#
        ),
        Some(Effort::Max)
    );
    assert_eq!(
        decode_effort(
            &AnthropicMessages,
            br#"{"model":"demo","max_tokens":8,"messages":[],"output_config":{"effort":"max"}}"#
        ),
        Some(Effort::Max)
    );
    assert_eq!(
        decode_effort(
            &AnthropicMessages,
            br#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"disabled"}}"#
        ),
        Some(Effort::None)
    );
    assert_eq!(
        decode_effort(
            &GenerativeLanguage,
            br#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingLevel":"MINIMAL"}}}"#
        ),
        Some(Effort::Minimal)
    );
}

#[test]
fn a_level_is_rewritten_for_the_outbound_protocol() {
    let chat = encoded(&OpenAiChat, Effort::Max);
    assert_eq!(chat["reasoning_effort"], json!("max"));

    let responses = encoded(&OpenAiResponses, Effort::Low);
    assert_eq!(responses["reasoning"]["effort"], json!("low"));

    let messages = encoded(&AnthropicMessages, Effort::Minimal);
    assert_eq!(messages["output_config"]["effort"], json!("low"));
    let off = encoded(&AnthropicMessages, Effort::None);
    assert_eq!(off["thinking"]["type"], json!("disabled"));

    let generative = encoded(&GenerativeLanguage, Effort::Xhigh);
    assert_eq!(
        generative["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        json!("high")
    );
    let none = encoded(&GenerativeLanguage, Effort::None);
    assert_eq!(
        none["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        json!(0)
    );
}

#[test]
fn the_original_field_stays_when_the_extension_bag_is_kept() {
    let codec = AnthropicMessages;
    let bytes = br#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"enabled","budget_tokens":128}}"#;
    let request = codec.decode_request(bytes).expect("decode");
    assert_eq!(request.params.effort, None);
    let encoded = codec.encode_request(&request).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["thinking"]["budget_tokens"], json!(128));
}
