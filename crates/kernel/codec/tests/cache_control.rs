//! Client cache marks and final request-parse checks on Messages translation.

use hofund_codec::{AnthropicMessages, Codec};
use serde_json::{Value, json};

fn translated(body: &str) -> Value {
    let request = AnthropicMessages
        .decode_request(body.as_bytes())
        .expect("decode");
    let encoded = AnthropicMessages.encode_request(&request).expect("encode");
    serde_json::from_slice(&encoded).expect("JSON")
}

#[test]
fn d12a_client_marks_and_ttls_survive_translation() {
    let wire = translated(
        r#"{"model":"m1","max_tokens":64,"system":[{"type":"text","text":"fixed","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[
        {"role":"user","content":[
            {"type":"text","text":"plain"},
            {"type":"text","text":"history","cache_control":{"type":"ephemeral","ttl":"1h"}},
            {"type":"image","source":{"type":"url","url":"https://example.test/image.png"},"cache_control":{"type":"ephemeral","ttl":"1h"}}
        ]},
        {"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{},"cache_control":{"type":"ephemeral","ttl":"1h"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok","cache_control":{"type":"ephemeral","ttl":"1h"}}]}
        ]}"#,
    );
    let mark = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!(wire.pointer("/system/0/cache_control"), Some(&mark));
    assert_eq!(wire.pointer("/messages/0/content/0/cache_control"), None);
    for pointer in [
        "/messages/0/content/1/cache_control",
        "/messages/0/content/2/cache_control",
        "/messages/1/content/0/cache_control",
        "/messages/2/content/0/cache_control",
    ] {
        assert_eq!(wire.pointer(pointer), Some(&mark), "{pointer}");
    }
}

#[test]
fn d12b_no_client_marks_keeps_current_unmarked_behavior() {
    let wire = translated(
        r#"{"model":"m1","max_tokens":64,"system":[{"type":"text","text":"fixed"}],"messages":[{"role":"user","content":[{"type":"text","text":"what does cache_control do?"}]}]}"#,
    );
    assert!(wire["system"].is_string());
    assert_eq!(wire.pointer("/messages/0/content/0/cache_control"), None);
}

#[test]
fn d12c_translated_parse_takes_signatures_off_call_ids() {
    let signed = "toolu_1__ts12__c2lnbmF0dXJl";
    let body = format!(
        r#"{{"model":"m1","max_tokens":64,"messages":[
        {{"role":"assistant","content":[{{"type":"tool_use","id":"{signed}","name":"Bash","input":{{}}}}]}},
        {{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{signed}","content":"ok","cache_control":{{"type":"ephemeral","ttl":"1h"}}}}]}}
        ]}}"#
    );
    let request = AnthropicMessages
        .decode_request(body.as_bytes())
        .expect("decode");
    let call = &request.messages[0].content[0];
    let result = &request.messages[1].content[0];
    assert!(matches!(call, hofund_ir::ContentPart::ToolCall(call) if call.id == "toolu_1"));
    assert!(matches!(result, hofund_ir::ContentPart::ToolResult(result) if result.id == "toolu_1"));
}

#[test]
fn d12d_thinking_blocks_do_not_take_cache_marks() {
    let wire = translated(
        r#"{"model":"m1","max_tokens":64,"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"think","signature":"sig","cache_control":{"type":"ephemeral","ttl":"1h"}},
        {"type":"redacted_thinking","data":"sealed","cache_control":{"type":"ephemeral","ttl":"1h"}},
        {"type":"text","text":"answer","cache_control":{"type":"ephemeral","ttl":"1h"}}
        ]}]}"#,
    );
    assert_eq!(wire.pointer("/messages/0/content/0/cache_control"), None);
    assert_eq!(wire.pointer("/messages/0/content/1/cache_control"), None);
    assert_eq!(
        wire.pointer("/messages/0/content/2/cache_control"),
        Some(&json!({"type": "ephemeral", "ttl": "1h"}))
    );
}

#[test]
fn d12e_required_tool_choice_still_requires_a_tool() {
    let error = AnthropicMessages
        .decode_request(
            br#"{"model":"m1","max_tokens":64,"messages":[],"tool_choice":{"type":"any"},"cache_control":{"type":"ephemeral"}}"#,
        )
        .expect_err("required choice without tools");
    assert!(error.to_string().contains("requires at least one tool"));
}

#[test]
fn d12f_a_truncated_signature_suffix_stays_on_the_id() {
    let signed = "toolu_1__ts12__c2lnbmF0dXJ";
    let body = format!(
        r#"{{"model":"m1","max_tokens":64,"messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"{signed}","name":"Bash","input":{{}}}}]}}]}}"#
    );
    let request = AnthropicMessages
        .decode_request(body.as_bytes())
        .expect("decode");
    assert!(
        matches!(&request.messages[0].content[0], hofund_ir::ContentPart::ToolCall(call) if call.id == signed)
    );
}

#[test]
fn d12h_kept_signature_marks_follow_the_reference_reading() {
    for (id, expected) in [
        ("toolu_1__tsk4__abcd", "toolu_1__tsk4__abcd"),
        ("toolu_1__tsk__x__ts4__c2ln", "toolu_1__tsk__x"),
        ("toolu_1__tsr4__c2ln", "toolu_1"),
        ("\u{e9}__ts4__c2ln", "\u{e9}"),
        ("__ts4__c2ln", "__ts4__c2ln"),
    ] {
        let body = format!(
            r#"{{"model":"m1","max_tokens":64,"messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{}}}}]}}]}}"#
        );
        let request = AnthropicMessages
            .decode_request(body.as_bytes())
            .expect("decode");
        assert!(
            matches!(&request.messages[0].content[0], hofund_ir::ContentPart::ToolCall(call) if call.id == expected),
            "{id}"
        );
    }
}

#[test]
fn d12g_an_invalid_mark_sequence_falls_back_without_rejecting() {
    let wire = translated(
        r#"{"model":"m1","max_tokens":64,"system":[{"type":"text","text":"fixed","cache_control":{"type":"ephemeral","ttl":"5m"}}],"messages":[{"role":"user","content":[
        {"type":"text","text":"history","cache_control":{"type":"ephemeral","ttl":"1h"}},
        {"type":"text","text":"current","cache_control":{"type":"ephemeral","ttl":"1h"}}
        ]}]}"#,
    );
    assert_eq!(wire.pointer("/system/0/cache_control"), None);
    assert_eq!(wire.pointer("/messages/0/content/0/cache_control"), None);
    assert_eq!(wire.pointer("/messages/0/content/1/cache_control"), None);
    assert_eq!(
        wire.pointer("/messages/0/content/1/text"),
        Some(&json!("current"))
    );
}
