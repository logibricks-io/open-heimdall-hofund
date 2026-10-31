//! Chat token-field parity cases.

use hofund_quirks::{
    EncodedContext, RequestState, TokenField, apply_encoded, apply_encoded_with_state,
    token_field_wanted, with_token_field,
};
use serde_json::Value;

const REFUSAL: &str = r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","type":"invalid_request_error","param":"max_tokens","code":"unsupported_parameter"}}"#;

fn apply(body: &str, base_url: &str, protocol: &str) -> Vec<u8> {
    apply_encoded(
        EncodedContext {
            base_url,
            preset: None,
            protocol,
        },
        body.as_bytes(),
    )
}

#[test]
fn v5_openai_chat_gets_max_completion_tokens() {
    let body = r#"{"model":"m","max_tokens":4096,"messages":[{"role":"user","content":"what is \"max_tokens\"?"}]}"#;
    for (base, wanted) in [
        ("https://api.openai.com/v1", "max_completion_tokens"),
        ("https://api.deepseek.com/v1", "max_tokens"),
        ("https://open.bigmodel.cn/api/paas/v4", "max_tokens"),
        ("https://relay.example/v1", "max_tokens"),
        ("https://notopenai.com/v1", "max_tokens"),
    ] {
        let got = apply(body, base, "openai-chat");
        let value: Value = serde_json::from_slice(&got).expect("JSON");
        assert_eq!(value[wanted], 4096, "{base}");
        if wanted == "max_tokens" {
            assert_eq!(got, body.as_bytes(), "{base}");
        }
        assert!(String::from_utf8_lossy(&got).contains(r#"what is \"max_tokens\"?"#));
    }
    assert_eq!(
        apply(body, "https://api.openai.com/v1", "anthropic-messages"),
        body.as_bytes()
    );
}

#[test]
fn v5_chat_token_field_retried_when_named() {
    assert_eq!(
        token_field_wanted(REFUSAL.as_bytes()),
        Some(TokenField::CompletionTokens)
    );
    assert_eq!(
        token_field_wanted(br#"{"error":{"message":"max_tokens must be at most 32768"}}"#),
        None
    );
    let body = br#"{"model":"gpt-6.1-sol","stream":true,"max_tokens":4096,"messages":[{"role":"user","content":"hi"}]}"#;
    let (retry, changed) = with_token_field(body, TokenField::CompletionTokens);
    assert!(changed);
    let value: Value = serde_json::from_slice(&retry).expect("JSON");
    assert!(value.get("max_tokens").is_none());
    assert_eq!(value["max_completion_tokens"], 4096);

    let state = RequestState::default();
    state.learn_token_field(TokenField::CompletionTokens);
    let learned = apply_encoded_with_state(
        EncodedContext {
            base_url: "https://relay.example/v1",
            preset: None,
            protocol: "openai-chat",
        },
        &state,
        body,
    );
    let value: Value = serde_json::from_slice(&learned).expect("JSON");
    assert!(value.get("max_tokens").is_none());
    assert!(value.get("max_completion_tokens").is_some());

    let other = apply_encoded_with_state(
        EncodedContext {
            base_url: "https://relay.example/v1",
            preset: None,
            protocol: "openai-chat",
        },
        &RequestState::default(),
        body,
    );
    assert_eq!(other, body);
}

#[test]
fn v5_learned_completion_tokens_refusal_wins_on_openai_host() {
    // Reference chatTokenField: a refused max_completion_tokens is checked
    // before the OpenAI host default.
    let body = br#"{"model":"m","max_completion_tokens":64,"messages":[]}"#;
    let state = RequestState::default();
    state.learn_token_field(TokenField::MaxTokens);
    let got = apply_encoded_with_state(
        EncodedContext {
            base_url: "https://api.openai.com/v1",
            preset: None,
            protocol: "openai-chat",
        },
        &state,
        body,
    );
    let value: Value = serde_json::from_slice(&got).expect("JSON");
    assert_eq!(value["max_tokens"], 64);
    assert!(value.get("max_completion_tokens").is_none());
}

#[test]
fn v5_deepseek_token_field_runs_before_learned_chat_token_field() {
    // Reference forwardOnce order: deepseekTokenField, then chatTokenField.
    let body = br#"{"model":"deepseek-chat","max_completion_tokens":64,"messages":[]}"#;
    let state = RequestState::default();
    state.learn_token_field(TokenField::CompletionTokens);
    let got = apply_encoded_with_state(
        EncodedContext {
            base_url: "https://api.deepseek.com/v1",
            preset: Some("deepseek"),
            protocol: "openai-chat",
        },
        &state,
        body,
    );
    let value: Value = serde_json::from_slice(&got).expect("JSON");
    assert_eq!(value["max_completion_tokens"], 64);
    assert!(value.get("max_tokens").is_none());
}
