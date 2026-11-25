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
        None
    );
    assert_eq!(
        decode_effort(
            &AnthropicMessages,
            br#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"enabled","budget_tokens":128},"output_config":{"effort":"max"}}"#
        ),
        Some(Effort::Max)
    );
    assert_eq!(
        decode_effort(
            &AnthropicMessages,
            br#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"disabled"}}"#
        ),
        None
    );
    let disabled = AnthropicMessages
        .decode_request(
            br#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"disabled"}}"#,
        )
        .expect("decode");
    let kept = AnthropicMessages.encode_request(&disabled).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&kept).expect("json");
    assert_eq!(value["thinking"]["type"], json!("disabled"));
    assert!(value.get("output_config").is_none());
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
    assert_eq!(request.params.effort, Some(Effort::Low));
    let encoded = codec.encode_request(&request).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
    assert_eq!(value["thinking"]["type"], json!("enabled"));
    assert_eq!(value["thinking"]["budget_tokens"], json!(128));
    assert!(value.get("output_config").is_none());
}

#[test]
fn d246_messages_thinking_budget_sets_effort_and_keeps_the_object() {
    let codec = AnthropicMessages;
    for (budget, effort) in [
        (1, Effort::Low),
        (4096, Effort::Low),
        (4097, Effort::Medium),
        (12000, Effort::Medium),
        (12001, Effort::High),
        (24000, Effort::High),
        (24001, Effort::Xhigh),
        (30000, Effort::Xhigh),
    ] {
        let body = format!(
            r#"{{"model":"demo","max_tokens":32000,"thinking":{{"type":"enabled","budget_tokens":{budget}}},"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        let request = codec.decode_request(body.as_bytes()).expect("decode");
        assert_eq!(request.params.effort, Some(effort), "{budget}");
        let encoded = codec.encode_request(&request).expect("encode");
        let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
        assert_eq!(value["thinking"]["type"], json!("enabled"));
        assert_eq!(value["thinking"]["budget_tokens"], json!(budget));
        assert!(value.get("output_config").is_none());
    }
    let adaptive = codec
        .decode_request(
            br#"{"model":"demo","max_tokens":32000,"thinking":{"type":"adaptive","budget_tokens":30000},"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("adaptive");
    assert_eq!(adaptive.params.effort, Some(Effort::Xhigh));
    let preferred = codec
        .decode_request(
            br#"{"model":"demo","max_tokens":32000,"thinking":{"type":"enabled","budget_tokens":30000},"output_config":{"effort":"low"},"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("output_config");
    assert_eq!(preferred.params.effort, Some(Effort::Low));
    let kept = codec.encode_request(&preferred).expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&kept).expect("json");
    assert_eq!(value["thinking"]["budget_tokens"], json!(30000));
    assert_eq!(value["output_config"]["effort"], json!("low"));
}

#[test]
fn d246_messages_effort_follows_thinking_then_output_config() {
    let codec = AnthropicMessages;
    let rows = [
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"disabled"},"output_config":{"effort":"high"}}"#,
            None,
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"output_config":{"effort":"high"}}"#,
            None,
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"between_tools"},"output_config":{"effort":"high"}}"#,
            None,
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"enabled","budget_tokens":30000},"output_config":{"effort":"high"}}"#,
            Some(Effort::High),
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"adaptive","budget_tokens":30000},"output_config":{"effort":"medium"}}"#,
            Some(Effort::Medium),
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"enabled","budget_tokens":4096}}"#,
            Some(Effort::Low),
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"enabled","budget_tokens":0},"output_config":{"effort":"high"}}"#,
            Some(Effort::High),
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"enabled"},"output_config":{"effort":"none"}}"#,
            Some(Effort::Low),
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"adaptive"},"output_config":{"effort":"minimal"}}"#,
            Some(Effort::Low),
        ),
        (
            r#"{"model":"demo","max_tokens":8,"messages":[],"thinking":{"type":"enabled","budget_tokens":4096},"output_config":{"effort":"nope"}}"#,
            Some(Effort::Low),
        ),
    ];
    for (body, effort) in rows {
        let request = codec.decode_request(body.as_bytes()).expect("decode");
        assert_eq!(request.params.effort, effort, "{body}");
        let encoded = codec.encode_request(&request).expect("encode");
        let value: serde_json::Value = serde_json::from_slice(&encoded).expect("json");
        let original: serde_json::Value = serde_json::from_str(body).expect("original");
        assert_eq!(value.get("thinking"), original.get("thinking"), "{body}");
        assert_eq!(
            value.get("output_config"),
            original.get("output_config"),
            "{body}"
        );
    }
    assert_eq!(
        decode_effort(
            &OpenAiChat,
            br#"{"model":"demo","messages":[],"reasoning_effort":"none"}"#
        ),
        Some(Effort::None)
    );
}

#[test]
fn d13a_thinking_level_is_recorded_as_request_effort() {
    for (level, effort) in [
        ("minimal", Effort::Minimal),
        ("LOW", Effort::Low),
        ("medium", Effort::Medium),
        ("high", Effort::High),
    ] {
        let body = format!(
            r#"{{"contents":[],"generationConfig":{{"thinkingConfig":{{"thinkingLevel":"{level}"}}}}}}"#
        );
        assert_eq!(
            decode_effort(&GenerativeLanguage, body.as_bytes()),
            Some(effort),
            "{level}"
        );
    }
}

#[test]
fn d13b_thinking_budget_maps_to_the_nearest_effort() {
    for (budget, effort) in [
        (1, Effort::Low),
        (512, Effort::Low),
        (4096, Effort::Low),
        (4097, Effort::Medium),
        (12000, Effort::Medium),
        (12001, Effort::High),
        (24000, Effort::High),
        (24001, Effort::Xhigh),
        (32768, Effort::Xhigh),
    ] {
        let body = json!({
            "contents": [],
            "generationConfig": {"thinkingConfig": {"thinkingBudget": budget}}
        })
        .to_string();
        assert_eq!(
            decode_effort(&GenerativeLanguage, body.as_bytes()),
            Some(effort),
            "{budget}"
        );
    }
}

#[test]
fn d13c_a_dynamic_thinking_budget_is_medium_effort() {
    assert_eq!(
        decode_effort(
            &GenerativeLanguage,
            br#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":-1}}}"#
        ),
        Some(Effort::Medium)
    );
}

#[test]
fn d13d_snake_case_thinking_fields_record_effort() {
    assert_eq!(
        decode_effort(
            &GenerativeLanguage,
            br#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinking_level":"LOW"}}}"#
        ),
        Some(Effort::Low)
    );
    assert_eq!(
        decode_effort(
            &GenerativeLanguage,
            br#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinking_budget":32768}}}"#
        ),
        Some(Effort::Xhigh)
    );
}

#[test]
fn d13e_zero_budget_and_include_thoughts_alone_do_not_invent_effort() {
    assert_eq!(
        decode_effort(
            &GenerativeLanguage,
            br#"{"contents":[],"generationConfig":{"thinkingConfig":{"thinkingBudget":0}}}"#
        ),
        Some(Effort::None)
    );
    assert_eq!(
        decode_effort(
            &GenerativeLanguage,
            br#"{"contents":[],"generationConfig":{"thinkingConfig":{"includeThoughts":true}}}"#
        ),
        None
    );
    assert_eq!(
        decode_effort(
            &GenerativeLanguage,
            br#"{"contents":[],"generationConfig":{}}"#
        ),
        None
    );
}
