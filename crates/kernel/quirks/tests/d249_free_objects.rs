//! Gemini free-form tool objects as JSON text (D-249).

use hofund_ir::{Extensions, Tool};
use hofund_quirks::{EncodedContext, apply_encoded, gemini_free_object_paths};
use serde_json::{Value, json};

const HINT: &str = "A JSON object, written as JSON text.";
const SCHEMA: &str = r#"{"type":"object","properties":{"calls":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string","description":"Exact tool name to invoke."},"arguments":{"type":"object","description":" Arguments matching the tool schema. ","title":"Arguments"}},"required":["name","arguments"]}}},"required":["calls"]}"#;

fn apply(body: &str, protocol: &str, base_url: &str) -> Vec<u8> {
    apply_encoded(
        EncodedContext {
            base_url,
            preset: None,
            protocol,
        },
        body.as_bytes(),
    )
}

fn schema(body: &[u8], protocol: &str) -> Value {
    let value: Value = serde_json::from_slice(body).expect("JSON");
    match protocol {
        "openai-chat" => value["tools"][0]["function"]["parameters"].clone(),
        "openai-responses" => value["tools"][0]["parameters"].clone(),
        "anthropic-messages" => value["tools"][0]["input_schema"].clone(),
        _ => panic!("protocol"),
    }
}

fn tools(protocol: &str) -> String {
    match protocol {
        "openai-chat" => format!(
            r#"[{{"type":"function","function":{{"name":"tool_call","parameters":{SCHEMA}}}}}]"#
        ),
        "openai-responses" => {
            format!(r#"[{{"type":"function","name":"tool_call","parameters":{SCHEMA}}}]"#)
        }
        "anthropic-messages" => {
            format!(r#"[{{"name":"tool_call","input_schema":{SCHEMA}}}]"#)
        }
        _ => panic!("protocol"),
    }
}

#[test]
fn d249_free_objects_are_sent_as_json_text_for_relays_and_copilot() {
    for base_url in [
        "https://relay.test/v1",
        "https://api.individual.githubcopilot.com",
    ] {
        for protocol in ["openai-chat", "openai-responses", "anthropic-messages"] {
            let body = format!(
                r#"{{"model":"relay/Gemini-4-hydrogen","tools":{}}}"#,
                tools(protocol)
            );
            let out = apply(&body, protocol, base_url);
            let schema = schema(&out, protocol);
            let arguments = &schema["properties"]["calls"]["items"]["properties"]["arguments"];
            assert_eq!(arguments["type"], "string", "{base_url} {protocol}");
            assert_eq!(
                arguments["description"],
                format!("Arguments matching the tool schema. ({HINT})")
            );
            assert_eq!(arguments["title"], "Arguments");
            assert!(arguments.get("properties").is_none());
            assert_eq!(
                schema["properties"]["calls"]["items"]["properties"]["name"],
                json!({"type": "string", "description": "Exact tool name to invoke."})
            );
            assert_eq!(
                schema["properties"]["calls"]["items"]["required"],
                json!(["name", "arguments"])
            );
        }
    }
}

#[test]
fn d249_other_models_and_native_protocol_keep_the_schema_bytes() {
    for protocol in ["openai-chat", "openai-responses", "anthropic-messages"] {
        let body = format!(r#"{{"model":"gpt-5.5","tools":{}}}"#, tools(protocol));
        assert_eq!(
            apply(&body, protocol, "https://relay.test/v1"),
            body.as_bytes()
        );
    }
    let body = format!(
        r#"{{"model":"gemini-4-hydrogen","tools":{}}}"#,
        tools("openai-chat")
    );
    assert_eq!(
        apply(&body, "generative-language", "https://relay.test/v1"),
        body.as_bytes()
    );
}

#[test]
fn d249_only_unrestricted_objects_below_the_tool_root_are_rewritten() {
    let schema = json!({
        "type": "object",
        "properties": {
            "free": {"type": "object"},
            "empty": {"type": "object", "properties": {}},
            "more": {"type": "object", "additionalProperties": true},
            "none": {"type": "object", "additionalProperties": false},
            "shaped": {"type": "object", "properties": {"nested": {"type": "object"}}},
            "pattern": {"type": "object", "patternProperties": {"^x": {"type": "string"}}},
            "union": {"type": "object", "anyOf": [{"type": "object"}]},
            "reference": {"type": "object", "$ref": "#/$defs/F"},
            "list": {"type": "array", "items": {"type": "object"}},
            "untyped": {"description": "not a declared object"}
        }
    });
    let body = format!(
        r#"{{"model":"gemini-4-hydrogen","tools":[{{"type":"function","function":{{"name":"call","parameters":{schema}}}}}]}}"#
    );
    let out = apply(&body, "openai-chat", "https://relay.test/v1");
    let value: Value = serde_json::from_slice(&out).expect("JSON");
    let properties = &value["tools"][0]["function"]["parameters"]["properties"];
    for name in ["free", "empty", "more"] {
        assert_eq!(properties[name]["type"], "string", "{name}");
    }
    assert_eq!(properties["list"]["type"], "array");
    assert_eq!(properties["list"]["items"]["type"], "string");
    assert_eq!(
        properties["shaped"]["properties"]["nested"]["type"],
        "string"
    );
    assert_eq!(properties["none"]["type"], "object");
    assert_eq!(properties["pattern"]["type"], "object");
    assert_eq!(properties["union"]["type"], "object");
    assert_eq!(properties["reference"]["type"], "object");
    assert_eq!(properties["untyped"]["type"], "string");
    assert_eq!(
        value["tools"][0]["function"]["parameters"]["type"],
        "object"
    );
}

#[test]
fn d249_reply_paths_use_the_same_model_and_protocol_selection() {
    let tools = vec![Tool {
        name: "tool_call".to_owned(),
        description: None,
        input_schema: serde_json::from_str(SCHEMA).expect("schema"),
        extensions: Extensions::new(),
    }];
    for protocol in ["openai-chat", "openai-responses", "anthropic-messages"] {
        assert!(
            gemini_free_object_paths(protocol, "relay/gemini-4-hydrogen", &tools).is_some(),
            "{protocol}"
        );
        assert!(gemini_free_object_paths(protocol, "gpt-5.5", &tools).is_none());
    }
    assert!(gemini_free_object_paths("generative-language", "gemini-4-hydrogen", &tools).is_none());
}
