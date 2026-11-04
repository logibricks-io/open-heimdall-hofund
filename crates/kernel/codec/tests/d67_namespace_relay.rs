//! D-67: namespaced function_call relay marks plain arguments unsealed.

use hofund_codec::namespace_tidy;
use hofund_codec::{Codec, OpenAiResponses};
use serde_json::json;

#[test]
fn d67_1_namespaced_call_gets_empty_encrypted_function_args() {
    let call = json!({
        "type": "function_call",
        "id": "fc_1",
        "call_id": "c9",
        "name": "spawn_agent",
        "namespace": "collaboration",
        "arguments": r#"{"message":"go"}"#,
        "status": "completed",
    });
    let whole = json!({
        "id": "r1",
        "object": "response",
        "status": "completed",
        "output": [call],
    });
    let tidied = namespace_tidy::tidy_whole(whole.to_string().into_bytes());
    let value: serde_json::Value = serde_json::from_slice(&tidied).expect("json");
    let item = &value["output"][0];
    assert_eq!(item["encrypted_function_args"], json!([]));
}

#[test]
fn d67_2_sealed_and_plain_calls_are_unchanged() {
    let sealed = json!({
        "type": "function_call",
        "name": "spawn_agent",
        "namespace": "collaboration",
        "encrypted_function_args": [{"content":"sealed"}],
    });
    let plain = json!({
        "type": "function_call",
        "name": "exec_command",
        "arguments": "{}",
    });
    let whole = json!({"object":"response","output":[sealed.clone(), plain.clone()]});
    let tidied = namespace_tidy::tidy_whole(whole.to_string().into_bytes());
    let value: serde_json::Value = serde_json::from_slice(&tidied).expect("json");
    assert_eq!(
        value["output"][0]["encrypted_function_args"],
        sealed["encrypted_function_args"]
    );
    assert!(value["output"][1].get("encrypted_function_args").is_none());
}

#[test]
fn d67_3_non_function_call_with_namespace_is_unchanged() {
    let item = json!({
        "type": "message",
        "role": "assistant",
        "namespace": "collaboration",
        "content": [{"type":"output_text","text":"hi"}],
    });
    // A sibling function_call keeps the byte prefilter from short-circuiting.
    let call = json!({"type":"function_call","name":"f","arguments":"{}","call_id":"c1"});
    let whole = json!({"object":"response","output":[item, call]});
    let tidied = namespace_tidy::tidy_whole(whole.to_string().into_bytes());
    let value: serde_json::Value = serde_json::from_slice(&tidied).expect("json");
    assert!(value["output"][0].get("encrypted_function_args").is_none());
    assert_eq!(value["output"][0]["namespace"], "collaboration");
}

#[test]
fn d67_4_relay_tap_applies_only_for_routed_models() {
    let codec = OpenAiResponses;
    let call = r#"{"type":"function_call","name":"spawn_agent","namespace":"collaboration","arguments":"{}"}"#;
    let line = format!("data: {{\"type\":\"response.output_item.done\",\"item\":{call}}}\n\n");
    let mut routed = codec.raw_tap("vendor/m", "m");
    let out = String::from_utf8(routed.push(line.as_bytes())).expect("utf-8");
    assert!(out.contains("encrypted_function_args"), "{out}");
    let mut direct = codec.raw_tap("gpt-5", "gpt-5");
    let kept = String::from_utf8(direct.push(line.as_bytes())).expect("utf-8");
    assert!(!kept.contains("encrypted_function_args"), "{kept}");
}
