//! Code Assist plain-schema parity cases.

use hofund_codec::{
    AnthropicMessages, Codec, GenerativeLanguage, OpenAiChat, plain_code_assist_tools, plain_schema,
};
use serde_json::{Value, json};

fn value(text: &str) -> Value {
    serde_json::from_str(text).expect("JSON fixture")
}

fn assert_plain(input: &str, expected: &str) {
    assert_eq!(plain_schema(&value(input)), value(expected));
}

fn code_assist_body(request: hofund_ir::Request) -> Value {
    let encoded = GenerativeLanguage
        .encode_request(&request)
        .expect("Generative Language request");
    let mut body: Value = serde_json::from_slice(&encoded).expect("encoded JSON");
    plain_code_assist_tools(body.as_object_mut().expect("request object"));
    body
}

#[test]
fn v1_resolves_pointer_into_properties() {
    let input = r##"{"type":"object",
    "properties":{
        "func":{"type":"object","properties":{
            "name":{"type":"string"},
            "triggers":{"type":"array","items":{
                "type":"object",
                "properties":{
                    "kind":{"type":"string","enum":["command","prompt"]},
                    "self":{"$ref":"#/properties/func/properties/triggers/items"}
                },
                "required":["kind"]
            }}
        }},
        "again":{"$ref":"#/properties/func/properties/triggers/items"}
    }}"##;
    let expected = r##"{"type":"object",
    "properties":{
        "func":{"type":"object","properties":{
            "name":{"type":"string"},
            "triggers":{"type":"array","items":{
                "type":"object",
                "properties":{
                    "kind":{"type":"string","enum":["command","prompt"]},
                    "self":{"type":"object"}
                },
                "required":["kind"]
            }}
        }},
        "again":{"type":"object","properties":{
            "kind":{"type":"string","enum":["command","prompt"]},
            "self":{"type":"object"}
        },"required":["kind"]}
    }}"##;
    assert_plain(input, expected);
}

#[test]
fn v1_self_reference_uses_whole_segments_and_definition_fallback() {
    let input = r##"{"type":"object",
    "$defs":{"Mode":{"type":"string","enum":["x","y"]}},
    "properties":{
        "a":{"type":"object","properties":{"n":{"type":"integer"}}},
        "ab":{"type":"object","properties":{
            "inner":{"$ref":"#/properties/a"},
            "mode":{"$ref":"#/components/schemas/Mode"}
        }}
    }}"##;
    let expected = r##"{"type":"object",
    "properties":{
        "a":{"type":"object","properties":{"n":{"type":"integer"}}},
        "ab":{"type":"object","properties":{
            "inner":{"type":"object","properties":{"n":{"type":"integer"}}},
            "mode":{"type":"string","enum":["x","y"]}
        }}
    }}"##;
    assert_plain(input, expected);
}

#[test]
fn v1_keeps_only_supported_schema_keywords() {
    let input = r##"{"type":"object","$schema":"http://json-schema.org/draft-07/schema#","additionalProperties":false,
    "properties":{
        "limit":{"type":"integer","exclusiveMinimum":0,"maximum":100,"description":"how many"},
        "offset":{"type":"integer","exclusiveMinimum":0,"minimum":1},
        "ratio":{"type":"number","exclusiveMaximum":1},
        "url":{"type":"string","format":"uri","title":"URL","minLength":1,"pattern":"^https?://"},
        "tags":{"type":"array","items":{"type":"string","uniqueItems":true},"minItems":1,"uniqueItems":true,"contains":{"type":"string"}},
        "opts":{"type":"object","properties":{"deep":{"type":["boolean","null"],"default":false}},"minProperties":1,"dependentRequired":{"a":["b"]},"not":{"type":"null"}}
    },"required":["limit","gone"]}"##;
    let expected = r##"{"type":"object",
    "properties":{
        "limit":{"type":"integer","minimum":0,"maximum":100,"description":"how many"},
        "offset":{"type":"integer","minimum":1},
        "ratio":{"type":"number","maximum":1},
        "url":{"type":"string","minLength":1,"pattern":"^https?://"},
        "tags":{"type":"array","items":{"type":"string"},"minItems":1},
        "opts":{"type":"object","properties":{"deep":{"type":"boolean","nullable":true}},"minProperties":1}
    },"required":["limit"]}"##;
    assert_plain(input, expected);
}

const ARRAY_ITEMS_ASK: &str = r##"{"model":"antigravity/gemini-3.8-flash","max_tokens":32,"messages":[{"role":"user","content":"reply with OK only"}],
 "tools":[{"name":"art_batch","description":"batch","input_schema":{"type":"object","properties":{"batch":{"type":"array"},"verbose":{"type":"boolean"}}}},
          {"name":"art_guide","description":"read topics","input_schema":{"type":"object","properties":{"items":{"type":"array","description":"topics"}}}}]}"##;

#[test]
fn v1_code_assist_arrays_get_items() {
    let request = AnthropicMessages
        .decode_request(ARRAY_ITEMS_ASK.as_bytes())
        .expect("Messages request");
    let body = code_assist_body(request);
    let declarations = body["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("declarations");
    assert_eq!(declarations.len(), 2);
    for (index, property) in ["batch", "items"].iter().enumerate() {
        let schema = &declarations[index]["parameters"]["properties"][property];
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"], json!({}));
    }
    assert_eq!(
        declarations[1]["parameters"]["properties"]["items"]["description"],
        "topics"
    );
}

#[test]
fn v1_all_arrays_have_items() {
    let input = r##"{"type":"object","$defs":{"Tags":{"type":"array","description":"tags"}},
    "properties":{
        "grid":{"type":"array","items":{"type":"array"}},
        "maybe":{"anyOf":[{"type":"null"},{"type":"array"}]},
        "one":{"oneOf":[{"type":"array","minItems":1},{"type":"string"}]},
        "both":{"allOf":[{"type":"object","properties":{"list":{"type":"array"}}}]},
        "tags":{"$ref":"#/$defs/Tags"},
        "opt":{"type":["array","null"]},
        "kept":{"type":"array","items":{"type":"string"}},
        "deep":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"cells":{"type":"array"}}}}}}
    }}"##;
    let expected = r##"{"type":"object","properties":{
        "grid":{"type":"array","items":{"type":"array","items":{}}},
        "maybe":{"type":"array","items":{},"nullable":true},
        "one":{"type":"array","items":{},"minItems":1},
        "both":{"type":"object","properties":{"list":{"type":"array","items":{}}}},
        "tags":{"type":"array","items":{},"description":"tags"},
        "opt":{"type":"array","items":{},"nullable":true},
        "kept":{"type":"array","items":{"type":"string"}},
        "deep":{"type":"object","properties":{"rows":{"type":"array","items":{"type":"object","properties":{"cells":{"type":"array","items":{}}}}}}}
    }}"##;
    assert_plain(input, expected);
    let plain = plain_schema(&value(
        r##"{"type":"object","properties":{"s":{"type":"string"}}}"##,
    ));
    assert!(!plain.to_string().contains("items"));
}

const RG_SCHEMA: &str = r##"{"type":"object","properties":{"pattern":{"type":"string"},"cursor":{"type":"string"}},"oneOf":[{"required":["pattern"],"not":{"required":["cursor"]}},{"required":["cursor"],"not":{"required":["pattern"]}}]}"##;

#[test]
fn v1_root_union_keeps_object() {
    let plain = plain_schema(&value(RG_SCHEMA));
    assert_eq!(plain["type"], "object");
    assert!(plain["properties"].get("pattern").is_some());
    assert!(plain["properties"].get("cursor").is_some());
}

#[test]
fn v1_chat_tool_root_union_keeps_object() {
    let input = format!(
        r##"{{"model":"m","messages":[{{"role":"user","content":"hi"}}],"tools":[{{"type":"function","function":{{"name":"rg","description":"search","parameters":{RG_SCHEMA}}}}}]}}"##
    );
    let request = OpenAiChat
        .decode_request(input.as_bytes())
        .expect("Chat request");
    let body = code_assist_body(request);
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["parameters"],
        value(
            r##"{"type":"object","properties":{"pattern":{"type":"string"},"cursor":{"type":"string"}}}"##
        )
    );
}

#[test]
fn v1_union_keeps_own_shape() {
    let cases = [
        (
            "root oneOf",
            RG_SCHEMA,
            r##"{"type":"object","properties":{"pattern":{"type":"string"},"cursor":{"type":"string"}}}"##,
        ),
        (
            "root anyOf",
            r##"{"type":"object","properties":{"a":{"type":"string"}},"anyOf":[{"required":["a"]},{"not":{"required":["a"]}}]}"##,
            r##"{"type":"object","properties":{"a":{"type":"string"}}}"##,
        ),
        (
            "properties only, no type",
            r##"{"properties":{"a":{"type":"string"}},"oneOf":[{"required":["a"]}]}"##,
            r##"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"]}"##,
        ),
        (
            "nested oneOf",
            r##"{"type":"object","properties":{"q":{"type":"object","description":"query","properties":{"pattern":{"type":"string"},"cursor":{"type":"string"}},"oneOf":[{"required":["pattern"],"not":{"required":["cursor"]}},{"required":["cursor"],"not":{"required":["pattern"]}}]}},"required":["q"]}"##,
            r##"{"type":"object","properties":{"q":{"type":"object","description":"query","properties":{"pattern":{"type":"string"},"cursor":{"type":"string"}}}},"required":["q"]}"##,
        ),
        (
            "nested anyOf",
            r##"{"type":"object","properties":{"q":{"type":"object","properties":{"a":{"type":"integer"}},"anyOf":[{"required":["a"]},{"type":"null"}]}}}"##,
            r##"{"type":"object","properties":{"q":{"type":"object","properties":{"a":{"type":"integer"}},"required":["a"],"nullable":true}}}"##,
        ),
        (
            "branches add fields",
            r##"{"type":"object","properties":{"kind":{"type":"string"}},"required":["kind"],"oneOf":[{"properties":{"kind":{"const":"a"},"x":{"type":"string"}},"required":["kind","x"]},{"properties":{"kind":{"const":"b"},"y":{"type":"number"}},"required":["kind","y"]}]}"##,
            r##"{"type":"object","properties":{"kind":{"type":"string"},"x":{"type":"string"},"y":{"type":"number"}},"required":["kind"]}"##,
        ),
        (
            "every branch requires it",
            r##"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"anyOf":[{"required":["a","b"]},{"required":["a"]}]}"##,
            r##"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":["a"]}"##,
        ),
        (
            "root allOf",
            r##"{"type":"object","properties":{"a":{"type":"string"}},"required":["a"],"allOf":[{"properties":{"b":{"type":"string"}},"required":["b"]},{"not":{"required":["c"]}}]}"##,
            r##"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":["a","b"]}"##,
        ),
        (
            "nested allOf",
            r##"{"type":"object","properties":{"q":{"type":"object","properties":{"a":{"type":"string"}},"allOf":[{"required":["a"]}]}}}"##,
            r##"{"type":"object","properties":{"q":{"type":"object","properties":{"a":{"type":"string"}},"required":["a"]}}}"##,
        ),
        (
            "allOf beside oneOf",
            r##"{"type":"object","properties":{"a":{"type":"string"}},"allOf":[{"properties":{"b":{"type":"string"}}}],"oneOf":[{"required":["a"]},{"required":["b"]}]}"##,
            r##"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}}}"##,
        ),
        (
            "typed scalar",
            r##"{"type":"string","description":"d","anyOf":[{"format":"date"},{"format":"uuid"}]}"##,
            r##"{"type":"string","description":"d"}"##,
        ),
        (
            "bare union still its first branch",
            r##"{"type":"object","properties":{"v":{"description":"value","anyOf":[{"type":"null"},{"type":"integer"},{"type":"string"}]}}}"##,
            r##"{"type":"object","properties":{"v":{"type":"integer","description":"value","nullable":true}}}"##,
        ),
    ];
    for (name, input, expected) in cases {
        assert_eq!(plain_schema(&value(input)), value(expected), "{name}");
    }
}

const BOOL_SCHEMA_ASK: &str = r##"{"model":"antigravity/gemini-3.8-flash","max_tokens":32,"messages":[{"role":"user","content":"reply with OK only"}],
 "tools":[{"name":"mcp__sheets__write_rows","description":"write rows","input_schema":{"type":"object","$schema":"https://json-schema.org/draft/2020-12/schema",
   "properties":{
     "spreadsheet":{"type":"string"},
     "sheet":{"type":"string"},
     "region":{"type":"string","x-mcp-header":"Region"},
     "rows":{"type":"array","items":{"type":"array","items":true},"description":"rows of cells"}
   },"required":["spreadsheet","rows"],"additionalProperties":false}}]}"##;

#[test]
fn v1_code_assist_boolean_schemas_and_extension_keys() {
    let request = AnthropicMessages
        .decode_request(BOOL_SCHEMA_ASK.as_bytes())
        .expect("Messages request");
    let body = code_assist_body(request);
    let parameters = &body["tools"][0]["functionDeclarations"][0]["parameters"];
    assert_schema_subset(parameters);
    let rows = &parameters["properties"]["rows"];
    assert_eq!(rows["type"], "array");
    assert_eq!(rows["items"]["type"], "array");
    assert_eq!(rows["items"]["items"], json!({}));
    assert_eq!(parameters["properties"]["region"]["type"], "string");
}

#[test]
fn v1_boolean_schemas_and_extension_keys_everywhere() {
    let input = r##"{"type":"object","x-root":1,"$defs":{"Any":true,"None":false,"Cell":{"type":"array","items":true,"x-def":"d"}},
    "properties":{
        "any":true,
        "gone":false,
        "x-region":{"type":"string","x-mcp-header":"Region"},
        "grid":{"type":"array","items":{"type":"array","items":{"type":"array","items":true}}},
        "empty":{"type":"array","items":false},
        "ref":{"$ref":"#/$defs/Any"},
        "cell":{"$ref":"#/$defs/Cell"},
        "never":{"$ref":"#/$defs/None"},
        "either":{"anyOf":[true,{"type":"null"}]},
        "first":{"anyOf":[false,{"type":"integer","x-k":1}]},
        "merged":{"allOf":[true,{"type":"object","properties":{"a":true,"b":{"type":"string","x-b":2}}}]},
        "obj":{"type":"object","properties":{"deep":{"type":"array","items":{"type":"object","properties":{"v":true},"x-n":{"a":1}}}}}
    },"required":["any","gone","never","x-region"]}"##;
    let out = plain_schema(&value(input));
    assert_schema_subset(&out);
    assert!(out["properties"].get("gone").is_none());
    assert!(out["properties"].get("never").is_none());
    let required = out["required"].to_string();
    assert!(!required.contains("gone"));
    assert!(!required.contains("never"));
    assert!(required.contains("x-region"));
    assert_eq!(out["properties"]["x-region"]["type"], "string");
    assert_eq!(out["properties"]["empty"]["items"], json!({}));
    assert_eq!(out["properties"]["empty"]["maxItems"], 0);
    assert_eq!(
        out["properties"]["grid"]["items"]["items"]["items"],
        json!({})
    );
    assert_eq!(out["properties"]["first"]["type"], "integer");
}

fn assert_schema_subset(schema: &Value) {
    const FIELDS: &[&str] = &[
        "type",
        "format",
        "title",
        "description",
        "nullable",
        "enum",
        "maxItems",
        "minItems",
        "properties",
        "required",
        "minProperties",
        "maxProperties",
        "minLength",
        "maxLength",
        "pattern",
        "example",
        "anyOf",
        "propertyOrdering",
        "default",
        "items",
        "minimum",
        "maximum",
    ];
    let object = schema.as_object().expect("schema object");
    for (key, value) in object {
        assert!(FIELDS.contains(&key.as_str()), "unexpected key {key}");
        match key.as_str() {
            "items" => assert_schema_subset(value),
            "properties" => {
                for property in value.as_object().expect("properties").values() {
                    assert_schema_subset(property);
                }
            }
            "anyOf" => {
                for alternative in value.as_array().expect("anyOf") {
                    assert_schema_subset(alternative);
                }
            }
            _ => {}
        }
    }
}
