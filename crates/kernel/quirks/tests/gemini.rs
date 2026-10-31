//! Gemini encoded-schema parity cases.

use hofund_quirks::{EncodedContext, apply_encoded};
use serde_json::Value;

fn apply(body: &str, protocol: &str) -> Vec<u8> {
    apply_encoded(
        EncodedContext {
            base_url: "https://relay.test/v1",
            preset: None,
            protocol,
        },
        body.as_bytes(),
    )
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("UTF-8")
}

fn schema_at(body: &str, path: &[&str]) -> String {
    let mut value: Value = serde_json::from_str(body).expect("JSON");
    for key in path {
        value = match value {
            Value::Array(items) => items
                .get(key.parse::<usize>().expect("array index"))
                .cloned()
                .unwrap_or(Value::Null),
            value => value.get(*key).cloned().unwrap_or(Value::Null),
        };
    }
    value.to_string()
}

#[test]
fn v3_gemini_tool_types_cua_driver_drag() {
    let schema = include_str!("cua_driver_drag_untyped.json").trim();
    let expected = schema
        .replace(r#""scope":{"const""#, r#""scope":{"type":"string","const""#)
        .replace(
            r#""target":{"anyOf":[{"description""#,
            r#""target":{"type":"object","anyOf":[{"type":"object","description""#,
        );
    assert_ne!(schema, expected);
    assert_eq!(
        expected.matches(r#""type":"object",""#).count(),
        schema.matches(r#""type":"object",""#).count() + 2
    );
    let routes = [
        (
            r#""tools":[{"type":"function","function":{"name":"mcp__cua-driver__drag","parameters":%s}}]"#,
            "openai-chat",
            &["tools", "0", "function", "parameters"][..],
        ),
        (
            r#""tools":[{"name":"mcp__cua-driver__drag","input_schema":%s}]"#,
            "anthropic-messages",
            &["tools", "0", "input_schema"][..],
        ),
        (
            r#""tools":[{"type":"function","name":"mcp__cua-driver__drag","parameters":%s}]"#,
            "openai-responses",
            &["tools", "0", "parameters"][..],
        ),
    ];
    for (tools, protocol, path) in routes {
        for model in ["google/gemini-3.8-flash", "openai/gpt-5.5"] {
            let body = format!(
                r#"{{"model":"cc/{model}",{}}}"#,
                tools.replace("%s", schema)
            );
            let got = text(apply(&body, protocol));
            let sent = schema_at(&got, path);
            if model.starts_with("google/") {
                assert_eq!(sent, expected, "{protocol}");
            } else {
                assert_eq!(sent, schema, "{protocol}");
                if protocol != "openai-responses" {
                    assert!(got.contains(&tools.replace("%s", schema)));
                }
            }
        }
    }
}

#[test]
fn v3_gemini_tool_types() {
    let params = r##"{"properties":{"any":{},"spaced":{ },"said":{"description":"x"},"n":{"enum":[1,2]},"f":{"enum":[1,2.5]},"b":{"const":true},"maybe":{"enum":["a",null]},"mixed":{"enum":["a",1]},"list":{"items":{}},"obj":{"properties":{"k":{"type":"string"}}},"pat":{"pattern":"^a"},"u":{"anyOf":[{"type":"string"},{"type":"null"}]},"two":{"anyOf":[{"type":"string"},{"type":"integer"}]},"num":{"oneOf":[{"type":"integer"},{"type":"number"}]},"r":{"$ref":"#/$defs/D"},"dflt":{"default":3},"neg":{"type":"string","not":{"enum":["x"]}},"typed":{"type":"array","items":{"type":"integer"}}},"$defs":{"D":{"properties":{}}}}"##;
    let expected = params
        .replace(
            r#"{"properties":{"any":{}"#,
            r#"{"type":"object","properties":{"any":{"type":"string"}"#,
        )
        .replace(r#""spaced":{ }"#, r#""spaced":{"type":"string" }"#)
        .replace(r#""said":{"#, r#""said":{"type":"string","#)
        .replace(r#""n":{"#, r#""n":{"type":"integer","#)
        .replace(r#""f":{"#, r#""f":{"type":"number","#)
        .replace(r#""b":{"#, r#""b":{"type":"boolean","#)
        .replace(r#""maybe":{"#, r#""maybe":{"type":"string","#)
        .replace(
            r#""list":{"items":{}}"#,
            r#""list":{"type":"array","items":{"type":"string"}}"#,
        )
        .replace(r#""obj":{"#, r#""obj":{"type":"object","#)
        .replace(r#""pat":{"#, r#""pat":{"type":"string","#)
        .replace(r#""u":{"#, r#""u":{"type":"string","#)
        .replace(r#""num":{"#, r#""num":{"type":"number","#)
        .replace(r#""r":{"#, r#""r":{"type":"object","#)
        .replace(r#""dflt":{"#, r#""dflt":{"type":"integer","#)
        .replace(r#""D":{"#, r#""D":{"type":"object","#);
    for model in [
        "gemini-3.8-flash",
        "google/gemini-3.8-flash",
        "vertex_ai/Gemini-2.5-pro",
    ] {
        let body = format!(
            r#"{{"model":"{model}","tools":[{{"type":"function","function":{{"name":"t","parameters":{params}}}}}]}}"#
        );
        let got = text(apply(&body, "openai-chat"));
        assert_eq!(
            schema_at(&got, &["tools", "0", "function", "parameters"]),
            serde_json::from_str::<Value>(&expected)
                .expect("expected JSON")
                .to_string(),
            "{model}"
        );
        assert!(got.contains(r#""spaced":{"type":"string" }"#));
    }
    for model in [
        "claude-sonnet-4.6",
        "gpt-5.5",
        "google/gemma-4",
        "my-gemini/m",
        "kimi-k3",
        "",
    ] {
        let body = format!(
            r#"{{"model":"{model}","tools":[{{"type":"function","function":{{"name":"t","parameters":{params}}}}}]}}"#
        );
        assert_eq!(text(apply(&body, "openai-chat")), body, "{model}");
    }
}

#[test]
fn v3_gemini_tool_types_drops_booleans_and_extension_keys() {
    let body = r#"{"model":"google/gemini-3.8-flash","tools":[{"type":"function","function":{"name":"w","parameters":{"type":"object","properties":{"x-region":{"type":"string","x-mcp-header":"Region"},"rows":{"type":"array","items":{"type":"array","items":true}},"gone":false,"u":{"anyOf":[false,{"type":"string"}]}},"additionalProperties":false}}},{"type":"function","function":{"name":"plain","parameters":{"type":"object",  "properties":{"a":{"type":"string"}}}}}]}"#;
    let got = text(apply(body, "openai-chat"));
    let value: Value = serde_json::from_str(&got).expect("JSON");
    let parameters = &value["tools"][0]["function"]["parameters"];
    assert!(parameters.to_string().find("x-mcp-header").is_none());
    assert!(parameters.to_string().find("true").is_none());
    assert!(parameters["properties"].get("gone").is_none());
    assert_eq!(parameters["properties"]["x-region"]["type"], "string");
    assert_eq!(parameters["additionalProperties"], false);
    assert!(parameters["properties"]["rows"]["items"]["items"].is_object());
    assert_eq!(
        parameters["properties"]["u"]["anyOf"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert!(got.contains(r#"{"type":"object",  "properties":{"a":{"type":"string"}}}"#));
    let other = body.replace("google/gemini-3.8-flash", "gpt-5.5");
    assert_eq!(text(apply(&other, "openai-chat")), other);
    let generative = text(apply(body, "generative-language"));
    assert_eq!(generative, body);
}

#[test]
fn v3_gemini_named_by_bare_prefix() {
    // Reference geminiNamed: last path segment, lowercased, starts with
    // "gemini" (no separator required).
    for model in ["gemini", "models/Gemini", "relay/geminipro"] {
        let body = format!(
            r#"{{"model":"{model}","tools":[{{"type":"function","function":{{"name":"f","parameters":{{"properties":{{"any":{{}}}}}}}}}}]}}"#
        );
        let got: Value = serde_json::from_slice(&apply(&body, "openai-chat")).expect("JSON");
        assert_eq!(
            got["tools"][0]["function"]["parameters"]["properties"]["any"]["type"], "string",
            "{model}"
        );
    }
    let body = r#"{"model":"agemini","tools":[{"type":"function","function":{"name":"f","parameters":{"properties":{"any":{}}}}}]}"#;
    assert_eq!(text(apply(body, "openai-chat")), body);
}
