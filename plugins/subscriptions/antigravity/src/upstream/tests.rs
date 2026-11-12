use hofund_plugin_api::{Request, Role};
use hofund_plugin_sdk::json::{Value, json};

use super::{lift, session_id};

fn request(text: &str) -> Request {
    hofund_plugin_sdk::json::from_value(json!({
        "model": "gemini-3-flash",
        "messages": [{"role":"user","content":[{"type":"text","text":text}]}]
    }))
    .expect("request")
}

#[test]
fn r49c_session_comes_from_the_first_user_text() {
    let first = session_id(&request("hello")).expect("session");
    assert_eq!(first, session_id(&request("hello")).expect("same session"));
    assert_ne!(first, session_id(&request("other")).expect("other session"));
    assert!(first.starts_with('-'));
    assert!(first[1..].parse::<u64>().expect("numeric") <= i64::MAX as u64);

    let mut empty_first = request("hello");
    empty_first.messages.insert(
        0,
        hofund_plugin_sdk::json::from_value(json!({
            "role":"user", "content":[{"type":"text","text":""}]
        }))
        .expect("empty message"),
    );
    assert_eq!(session_id(&empty_first).expect("session"), first);
    assert_eq!(request("hello").messages[0].role, Role::User);
}

#[test]
fn r49c_lift_leaves_an_unwrapped_reply_unchanged() {
    let value = json!({"candidates": []});
    assert_eq!(lift(value.clone()), value);
    assert_eq!(
        lift(json!({"response": {"candidates": []}, "traceId": "t"})),
        json!({"candidates": []})
    );
    assert_eq!(lift(Value::Null), Value::Null);
}

#[test]
fn r49c_turned_away_hints_and_status_are_prompt_specific() {
    use hofund_plugin_api::{PluginError, PluginErrorKind};

    for (prompt, expected) in [
        (
            "You are Codex, an agent based on GPT-5.",
            "start a new chat",
        ),
        (
            "You are Hermes Agent, an intelligent AI assistant created by Nous Research.",
            "SOUL.md",
        ),
        ("x-anthropic-billing-header: test", "use another provider"),
    ] {
        let mut request = request("hello");
        request.messages.insert(
            0,
            hofund_plugin_sdk::json::from_value(json!({
                "role":"system", "content":[{"type":"text","text":prompt}]
            }))
            .expect("system"),
        );
        let error = super::classify_turned_away(
            &request,
            PluginError::quota_status(429, "Resource has been exhausted (e.g. check quota)."),
        );
        assert_eq!(error.status(), Some(400));
        assert!(!error.cools());
        assert!(error.message().contains(expected), "{}", error.message());
    }

    let error = super::classify_turned_away(
        &request("hello"),
        PluginError::status_error(503, "Resource has been exhausted"),
    );
    assert_eq!(error.status(), Some(503));
    let error = super::classify_turned_away(
        &request("hello"),
        PluginError::status_error(429, "You have exhausted your capacity on this model."),
    );
    assert_eq!(error.kind(), PluginErrorKind::Quota);
    assert!(error.cools());
}
