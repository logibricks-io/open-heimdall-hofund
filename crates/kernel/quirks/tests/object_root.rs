//! Object-root folding parity cases.

use hofund_quirks::{RequestState, fold_encoded, fold_object_roots, root_union_refusal};
use serde_json::{Value, json};

const XAI_REFUSAL: &str = r#"{"error":{"message":"Failed to start sampling: [invalid_client_tool_schema] mcp__codex_app__automation_update: tool parameter root must be an object type (root schema is an anyOf/oneOf union with a non-object branch) (tid: 2026100808425178410684649237042)","type":"upstream_error","param":"400","code":"bad_response_status_code"}}"#;
const COMPATIBLE_REFUSAL: &str = r#"{"error":{"message":"Invalid schema for function 'mcp__codex_app__automation_update': schema must have type 'object' and not have 'oneOf'/'anyOf'/'allOf'/'enum'/'const'/'not' at the top level.","type":"invalid_request_error","param":"tools[1].parameters","code":"invalid_function_parameters"}}"#;
const OPENAI_REFUSAL: &str = "{\n  \"error\": {\n    \"message\": \"Invalid schema for function 'mcp__codex_app__automation_update': schema must be a JSON Schema of 'type: \\\"object\\\"', got 'type: \\\"None\\\"'.\",\n    \"type\": \"invalid_request_error\",\n    \"param\": \"tools[0].parameters\",\n    \"code\": \"invalid_function_parameters\"\n  }\n}";

#[test]
fn v4_root_union_refusal_wordings() {
    for body in [
        XAI_REFUSAL,
        COMPATIBLE_REFUSAL,
        OPENAI_REFUSAL,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"tools.1.custom.input_schema: input_schema does not support oneOf, allOf, or anyOf at the top level"}}"#,
    ] {
        assert!(root_union_refusal(body.as_bytes()), "{body}");
    }
    for body in [
        r#"{"error":{"message":"Invalid schema for function 'read': In context=('properties', 'path'), 'format' is not permitted.","type":"invalid_request_error","code":"invalid_function_parameters"}}"#,
        r#"{"error":{"message":"Invalid 'tools[0].name': string does not match pattern.","type":"invalid_request_error"}}"#,
    ] {
        assert!(!root_union_refusal(body.as_bytes()), "{body}");
    }
}

#[test]
fn v4_object_schema_root_one_of_keeps_object() {
    let mut schema = json!({"type":"object","properties":{"pattern":{"type":"string"},"cursor":{"type":"string"}},"oneOf":[{"required":["pattern"],"not":{"required":["cursor"]}},{"required":["cursor"],"not":{"required":["pattern"]}}]});
    assert!(hofund_ir::fold_object_root(&mut schema));
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["pattern"]["type"], "string");
    assert_eq!(schema["properties"]["cursor"]["type"], "string");
}

#[test]
fn v4_root_union_folded_for_xai() {
    let state = RequestState::default();
    let body = br#"{"model":"grok-4.7","tools":[{"type":"function","parameters":{"anyOf":[{"type":"object","properties":{"prompt":{"type":"string"}}}]}}]}"#;
    let (_, changed) = fold_object_roots(
        "https://api.x.ai/v1",
        "openai-responses",
        "grok-4.7",
        &state,
        body,
    );
    assert!(changed);
    let (_, changed) = fold_object_roots(
        "https://api.example.com/v1",
        "openai-chat",
        "m1",
        &state,
        body,
    );
    assert!(!changed);
}

#[test]
fn v4_root_union_folded_where_refused() {
    let schema = include_str!("codex_automation_update_schema.json");
    for protocol in ["openai-responses", "openai-chat"] {
        let tools = if protocol == "openai-chat" {
            format!(
                r#"[{{"type":"function","function":{{"name":"mcp__codex_app__automation_update","parameters":{schema}}}}}]"#
            )
        } else {
            format!(
                r#"[{{"type":"function","name":"mcp__codex_app__automation_update","parameters":{schema}}}]"#
            )
        };
        let body = format!(
            r#"{{"model":"relay/grok-4.7","stream":false,"store":false,"tools":{tools},"input":[]}}"#
        );
        let (folded, changed) = fold_encoded(protocol, body.as_bytes());
        assert!(changed, "{protocol}");
        let value: Value = serde_json::from_slice(&folded).expect("folded JSON");
        let parameters = if protocol == "openai-chat" {
            &value["tools"][0]["function"]["parameters"]
        } else {
            &value["tools"][0]["parameters"]
        };
        assert_eq!(parameters["type"], "object");
        assert!(parameters.get("anyOf").is_none());
        assert!(parameters["properties"].get("prompt").is_some());
        assert!(parameters["properties"].get("mode").is_some());

        let state = RequestState::default();
        let (_, changed) = fold_object_roots(
            "https://relay.test/v1",
            protocol,
            "grok-4.7",
            &state,
            body.as_bytes(),
        );
        assert!(!changed);
        state.learn_root_union("grok-4.7");
        let (_, changed) = fold_object_roots(
            "https://relay.test/v1",
            protocol,
            "grok-4.7",
            &state,
            body.as_bytes(),
        );
        assert!(changed);
        let (_, changed) = fold_object_roots(
            "https://relay.test/v1",
            protocol,
            "lax/m1",
            &state,
            body.as_bytes(),
        );
        assert!(!changed);
    }
}

#[test]
fn v4_responses_namespace_tools_folded() {
    // Reference foldTools: a Responses namespace's own tools are folded.
    let body = br#"{"model":"m","tools":[{"type":"namespace","name":"ns","tools":[{"type":"function","name":"f","parameters":{"anyOf":[{"type":"object","properties":{"a":{"type":"string"}}}]}}]}]}"#;
    let (folded, changed) = fold_encoded("openai-responses", body);
    assert!(changed);
    let value: Value = serde_json::from_slice(&folded).expect("JSON");
    let parameters = &value["tools"][0]["tools"][0]["parameters"];
    assert_eq!(parameters["type"], "object");
    assert!(parameters.get("anyOf").is_none());
}
