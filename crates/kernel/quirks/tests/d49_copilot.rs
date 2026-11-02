//! Copilot Gemini and Grok encoded-request quirks (D-49).

use hofund_quirks::{EncodedContext, apply_encoded};

const COPILOT: &str = "https://api.individual.githubcopilot.com";

#[test]
fn d49_4_non_copilot_gemini_is_untouched() {
    let body = br#"{"model":"gpt-4.1","tools":[{"type":"function","function":{"name":"t","parameters":{"anyOf":[{"type":"object","properties":{"kind":{"type":"string","enum":["a"]}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"type":"string","enum":["b"]}},"required":["kind"],"additionalProperties":false}]}}}]} "#;
    let trimmed = &body[..body.len() - 1];
    let out = apply_encoded(
        EncodedContext {
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            preset: Some("google"),
            protocol: "openai-chat",
        },
        trimmed,
    );
    assert_eq!(out, trimmed);
}

#[test]
fn d49_5_copilot_gemini_disjoint_one_of() {
    let body = br#"{"model":"gemini-2.5","tools":[{"type":"function","function":{"name":"t","parameters":{"anyOf":[{"type":"object","properties":{"kind":{"type":"string","enum":["a"]}},"required":["kind"],"additionalProperties":false},{"type":"object","properties":{"kind":{"type":"string","enum":["b"]}},"required":["kind"],"additionalProperties":false}]}}}]}"#;
    let out = apply_encoded(
        EncodedContext {
            base_url: COPILOT,
            preset: None,
            protocol: "openai-chat",
        },
        body,
    );
    assert!(out.windows(7).any(|window| window == br#""oneOf""#));
    assert_eq!(out.len(), body.len());
}

#[test]
fn d49_6_copilot_grok_custom_tool_becomes_function() {
    let body = br#"{"model":"grok-3","tools":[{"type":"custom","name":"apply_patch","input_schema":{"type":"object"}}]}"#;
    let out = apply_encoded(
        EncodedContext {
            base_url: COPILOT,
            preset: None,
            protocol: "openai-responses",
        },
        body,
    );
    let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
    assert_eq!(value["tools"][0]["type"], "function");
    assert!(value["tools"][0].get("function").is_some());
}

#[test]
fn d49_7_copilot_gpt_keeps_custom_tools() {
    let body = br#"{"model":"gpt-4.1","tools":[{"type":"custom","name":"apply_patch"}]}"#;
    let out = apply_encoded(
        EncodedContext {
            base_url: COPILOT,
            preset: None,
            protocol: "openai-responses",
        },
        body,
    );
    assert_eq!(out, body);
}
