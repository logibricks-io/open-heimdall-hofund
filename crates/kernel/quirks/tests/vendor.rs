//! Vendor encoded-request parity cases.

use hofund_quirks::{EncodedContext, apply_encoded};
use serde_json::{Value, json};

fn apply(body: &str, preset: Option<&str>, base_url: &str, protocol: &str) -> Vec<u8> {
    apply_encoded(
        EncodedContext {
            base_url,
            preset,
            protocol,
        },
        body.as_bytes(),
    )
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("UTF-8")
}

fn json_body(bytes: Vec<u8>) -> Value {
    serde_json::from_slice(&bytes).expect("JSON")
}

fn sampling(
    model: &str,
    preset: Option<&str>,
    base_url: &str,
    protocol: &str,
    fields: &str,
) -> Value {
    let body = format!(r#"{{"model":"{model}","messages":[],{fields}}}"#);
    json_body(apply(&body, preset, base_url, protocol))
}

fn no_sampling(value: &Value) {
    assert!(value.get("temperature").is_none(), "{value}");
    assert!(value.get("top_p").is_none(), "{value}");
}

#[test]
fn v2_kimi_tool_enum_types() {
    let tools = r#""tools":[{"type":"function","function":{"name":"parse_visual_regions","parameters":{"type":"object","properties":{"options":{"type":"object","properties":{"kinds":{"anyOf":[{"items":{"enum":["text","icon"]},"maxItems":2,"type":"array"},{"type":"null"}]},"level":{"enum":[1,2,3]},"scale":{"enum":[1,1.5]},"on":{"const":true},"mixed":{"enum":["a",1]},"maybe":{"enum":["a",null]},"typed":{"type":"string","enum":["x"]},"enum":{"type":"string"}}}},"$defs":{"mode":{ "enum" : ["fast","slow"]}}}}}]"#;
    let body =
        format!(r#"{{"model":"kimi/m","messages":[{{"role":"user","content":"hi"}}],{tools}}}"#);
    let expected = tools
        .replace(r#""items":{"enum""#, r#""items":{"type":"string","enum""#)
        .replace(r#""level":{"enum""#, r#""level":{"type":"integer","enum""#)
        .replace(r#""scale":{"enum""#, r#""scale":{"type":"number","enum""#)
        .replace(r#""on":{"const""#, r#""on":{"type":"boolean","const""#)
        .replace(r#""mode":{ "enum""#, r#""mode":{"type":"string", "enum""#)
        .replace(
            r#""maybe":{"enum""#,
            r#""maybe":{"type":["string","null"],"enum""#,
        );
    let got = text(apply(
        &body,
        Some("kimi-code-cn"),
        "https://api.kimi.com/coding/v1",
        "openai-chat",
    ));
    assert!(got.contains(&expected), "{got}");
    json_body(got.into_bytes());
    let other = text(apply(&body, None, "https://other.test/v1", "openai-chat"));
    assert!(other.contains(tools), "{other}");
}

#[test]
fn v2_kimi_tool_ref_types() {
    let schema = r##"{"type":"object","properties":{"a":{"$ref":"#/$defs/__schema0"},"b":{"$ref":"#/$defs/__schema1"},"c":{"items":{"$ref":"#/$defs/__schema2"},"type":"array"},"d":{"anyOf":[{"$ref":"#/$defs/P"},{"type":"null"}]},"e":{"$ref":"#/$defs/M"},"f":{"$ref":"#/definitions/D"},"g":{"$ref":"#/$defs/R"},"h":{"$ref":"#/$defs/U"},"i":{"$ref":"#/$defs/S"}},"$defs":{"__schema0":{},"__schema1":{"oneOf":[{"type":"string"},{"type":"number"}]},"__schema2":{"allOf":[{"type":"string"}]},"P":{"properties":{"x":{"type":"string"}}},"M":{"minLength":1},"R":{"oneOf":[{"$ref":"#/$defs/P"},{"$ref":"#/definitions/D"}]},"U":{"anyOf":[{"type":"string"},{"$ref":"#/$defs/U"}]},"S":{"type":"string","enum":["x"]},"V":{"oneOf":[{"type":"string"}]}},"definitions":{"D":{"description":"anything"}}}"##;
    let anything = r#"["string","number","integer","boolean","object","array","null"]"#;
    let expected = schema
        .replace(
            r#""__schema1":{"#,
            r#""__schema1":{"type":["string","number"],"#,
        )
        .replace(r#""__schema2":{"#, r#""__schema2":{"type":"string","#)
        .replace(r#""P":{"#, r#""P":{"type":"object","#)
        .replace(r#""M":{"#, r#""M":{"type":"string","#)
        .replace(r#""R":{"#, &format!(r#""R":{{"type":{anything},"#))
        .replace(r#""D":{"#, &format!(r#""D":{{"type":{anything},"#));
    let routes = [
        (
            r#""tools":[{"type":"function","function":{"name":"automation_update","parameters":%s}}]"#,
            "openai-chat",
        ),
        (
            r#""tools":[{"name":"automation_update","input_schema":%s}]"#,
            "anthropic-messages",
        ),
        (
            r#""tools":[{"type":"function","name":"automation_update","parameters":%s}]"#,
            "openai-responses",
        ),
    ];
    for (tools, protocol) in routes {
        let body = format!(r#"{{"model":"kimi/m",{}}}"#, tools.replace("%s", schema));
        let want = format!(r#"{{"model":"kimi/m",{}}}"#, tools.replace("%s", &expected));
        assert_eq!(
            text(apply(
                &body,
                Some("kimi-code-cn"),
                "https://api.kimi.com/coding/v1",
                protocol
            )),
            want,
            "{protocol}"
        );
        assert_eq!(
            text(apply(&body, None, "https://other.test/v1", protocol)),
            body,
            "other provider, {protocol}"
        );
    }
}

#[test]
fn v2_locked_sampling_refused_values_are_dropped() {
    no_sampling(&sampling(
        "kimi-for-coding",
        Some("kimi-code-cn"),
        "https://api.kimi.com/coding/v1",
        "openai-chat",
        r#""temperature":0.1,"top_p":1,"max_tokens":5"#,
    ));
}

#[test]
fn v2_locked_sampling_absent_stays_absent() {
    no_sampling(&sampling(
        "kimi-for-coding",
        Some("kimi-code-cn"),
        "https://api.kimi.com/coding/v1",
        "openai-chat",
        r#""max_tokens":5"#,
    ));
}

#[test]
fn v2_locked_sampling_accepted_values_are_dropped_too() {
    no_sampling(&sampling(
        "kimi-for-coding",
        Some("kimi-code-cn"),
        "https://api.kimi.com/coding/v1",
        "openai-chat",
        r#""temperature":1,"top_p":0.95"#,
    ));
}

#[test]
fn v2_locked_sampling_kimi_code_k3_drops_fields() {
    no_sampling(&sampling(
        "k3",
        Some("kimi-code-cn"),
        "https://api.kimi.com/coding/v1",
        "openai-chat",
        r#""temperature":0.1,"top_p":1"#,
    ));
}

#[test]
fn v2_locked_sampling_anthropic_endpoint_drops_fields() {
    no_sampling(&sampling(
        "kimi-for-coding",
        Some("kimi-code-cn"),
        "https://api.kimi.com/coding",
        "anthropic-messages",
        r#""temperature":0.1,"top_p":1,"max_tokens":5"#,
    ));
}

#[test]
fn v2_locked_sampling_moonshot_k26_drops_fields() {
    no_sampling(&sampling(
        "kimi-k2.6",
        Some("moonshot-cn"),
        "https://api.moonshot.cn/v1",
        "openai-chat",
        r#""temperature":0.1,"top_p":1"#,
    ));
}

#[test]
fn v2_locked_sampling_moonshot_k25_drops_fields() {
    no_sampling(&sampling(
        "kimi-k2.5",
        Some("moonshot-cn"),
        "https://api.moonshot.cn/v1",
        "openai-chat",
        r#""temperature":0.1,"top_p":1"#,
    ));
}

#[test]
fn v2_locked_sampling_vendor_prefixed_model_drops_fields() {
    no_sampling(&sampling(
        "moonshotai/kimi-k2.6",
        Some("moonshot-cn"),
        "https://api.moonshot.cn/v1",
        "openai-chat",
        r#""temperature":0.1,"top_p":1"#,
    ));
}

#[test]
fn v2_locked_sampling_moonshot_own_model_keeps_temperature() {
    let got = sampling(
        "moonshot-v1-8k",
        Some("moonshot-cn"),
        "https://api.moonshot.cn/v1",
        "openai-chat",
        r#""temperature":0.1,"top_p":1"#,
    );
    assert_eq!(got["temperature"], 0.1);
    assert_eq!(got["top_p"], 1);
}

#[test]
fn v2_locked_sampling_k2_preview_keeps_temperature() {
    let got = sampling(
        "kimi-k2-0905-preview",
        Some("moonshot-cn"),
        "https://api.moonshot.cn/v1",
        "openai-chat",
        r#""temperature":0.1"#,
    );
    assert_eq!(got["temperature"], 0.1);
}

#[test]
fn v2_locked_sampling_k2_thinking_keeps_temperature() {
    let got = sampling(
        "kimi-k2-thinking",
        Some("moonshot-cn"),
        "https://api.moonshot.cn/v1",
        "openai-chat",
        r#""temperature":0.1"#,
    );
    assert_eq!(got["temperature"], 0.1);
}

#[test]
fn v2_locked_sampling_ark_coding_plan_drops_fields() {
    no_sampling(&sampling(
        "kimi-k3.1-pro",
        Some("volcengine"),
        "https://ark.cn-beijing.volces.com/api/coding/v3",
        "openai-chat",
        r#""temperature":0.6,"top_p":0.95"#,
    ));
}

#[test]
fn v2_locked_sampling_ark_anthropic_endpoint_drops_fields() {
    no_sampling(&sampling(
        "kimi-k3.1-pro",
        Some("volcengine"),
        "https://ark.cn-beijing.volces.com/api/coding",
        "anthropic-messages",
        r#""temperature":0.6,"top_p":0.95,"max_tokens":5"#,
    ));
}

#[test]
fn v2_locked_sampling_ark_unlocked_model_keeps_fields() {
    let got = sampling(
        "doubao-seed-2.1-pro",
        Some("volcengine"),
        "https://ark.cn-beijing.volces.com/api/coding/v3",
        "openai-chat",
        r#""temperature":0.6,"top_p":0.95"#,
    );
    assert_eq!(got["temperature"], 0.6);
    assert_eq!(got["top_p"], 0.95);
}

#[test]
fn v2_locked_sampling_custom_ark_host_drops_fields() {
    no_sampling(&sampling(
        "kimi-k3",
        None,
        "https://ark.ap-southeast.volces.com/api/v3",
        "openai-chat",
        r#""temperature":0.6,"top_p":0.95"#,
    ));
}

#[test]
fn v2_locked_sampling_other_provider_is_unchanged() {
    let body = r#"{"model":"m","messages":[],"temperature":0.1,"top_p":1}"#;
    assert_eq!(
        text(apply(body, None, "https://other.test/v1", "openai-chat")),
        body
    );
}

fn pattern_body(pattern: &str) -> String {
    json!({"tools":[{"input_schema":{"pattern":pattern}}]}).to_string()
}

#[test]
fn v2_unicode_null_escape_cases() {
    for (input, expected) in [
        (r#"^[^\0]*$"#, r#"^[^\u0000]*$"#),
        (r#"\\0"#, r#"\\0"#),
        (r#"\01"#, r#"\01"#),
        (r#"\u0000"#, r#"\u0000"#),
        (r#"^https?://"#, r#"^https?://"#),
        (r#"\\\0"#, r#"\\\u0000"#),
    ] {
        let got = json_body(apply(
            &pattern_body(input),
            Some("deepseek"),
            "https://api.deepseek.com/v1",
            "anthropic-messages",
        ));
        assert_eq!(
            got["tools"][0]["input_schema"]["pattern"], expected,
            "{input}"
        );
    }
}

#[test]
fn v2_deepseek_tool_patterns_only_schemas() {
    let input = r#"{"messages":[{"role":"user","content":"\\0"}],"tools":[{"name":"Artifact","input_schema":{"type":"object","properties":{"content":{"type":"string","pattern":"^[^\\0]*$","default":{"pattern":"\\0"}}},"const":{"pattern":"\\0"}}},{"type":"function","function":{"name":"other","parameters":{"anyOf":[{"pattern":"\\0"}]}}}]}"#;
    let got = json_body(apply(
        input,
        Some("deepseek"),
        "https://api.deepseek.com",
        "anthropic-messages",
    ));
    let schema = &got["tools"][0]["input_schema"];
    assert_eq!(
        schema["properties"]["content"]["pattern"],
        r#"^[^\u0000]*$"#
    );
    assert_eq!(
        schema["properties"]["content"]["default"]["pattern"],
        r#"\0"#
    );
    assert_eq!(schema["const"]["pattern"], r#"\0"#);
    assert_eq!(got["messages"][0]["content"], r#"\0"#);
    assert_eq!(
        got["tools"][1]["function"]["parameters"]["anyOf"][0]["pattern"],
        r#"\u0000"#
    );
    let unchanged = r#" {"tools":[{"input_schema":{"pattern":"^https?://"}}]} "#;
    assert_eq!(
        text(apply(
            unchanged,
            Some("deepseek"),
            "https://api.deepseek.com",
            "anthropic-messages"
        )),
        unchanged
    );
}

#[test]
fn v2_deepseek_tool_patterns_byte_identity() {
    let nothing = r#"{
        "z_order": 1,
        "a_order": 2,
        "html_description": "User description with <special> & characters",
        "tools": [
            {"name": "test", "description": "<escapes> & unescaped", "input_schema": {"type": "object", "properties": {"url": {"pattern": "^https?://.*$"}}}}
        ]
    }"#;
    assert_eq!(
        text(apply(
            nothing,
            Some("deepseek"),
            "https://api.deepseek.com",
            "anthropic-messages"
        )),
        nothing
    );
    let input = r#"{
        "b_key": "val",
        "a_key": "val",
        "tools": [
            {
                "name": "Artifact",
                "description": "Writes files with <tags> & &amp; entities",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "pattern": "^[^\\0]*$"}
                    }
                }
            }
        ],
        "messages": [{"role": "user", "content": "hello <world> & fun"}]
    }"#;
    let expected = input.replace(r#""pattern": "^[^\\0]*$""#, r#""pattern": "^[^\\u0000]*$""#);
    assert_eq!(
        text(apply(
            input,
            Some("deepseek"),
            "https://api.deepseek.com",
            "anthropic-messages"
        )),
        expected
    );
}

#[test]
fn v2_deepseek_patterns_responses_and_invalid_bodies() {
    let input = r#"{"id":9007199254740993,"tools":[{"type":"function","name":"Artifact","parameters":{"$defs":{"content":{"pattern":"^[^\\0]*$"}},"properties":{"value":{"items":{"pattern":"\\0"}}}}}]}"#;
    let got = text(apply(
        input,
        Some("deepseek"),
        "https://api.deepseek.com",
        "anthropic-messages",
    ));
    assert!(!got.contains(r#"\\0"#), "{got}");
    assert!(got.contains("9007199254740993"));
    assert!(got.contains(r#"\\u0000"#));
    for invalid in [r#"{"tools":"#, "null", r#"{"tools":[null]}"#] {
        assert_eq!(
            text(apply(
                invalid,
                Some("deepseek"),
                "https://api.deepseek.com",
                "anthropic-messages"
            )),
            invalid
        );
    }
}

#[test]
fn v2_deepseek_forward_normalizes_artifact() {
    let body = r#"{"tools":[{"name":"Artifact","input_schema":{"pattern":"^[^\\0]*$"}}]}"#;
    for (preset, base, wanted) in [
        (Some("deepseek"), "https://relay.test", true),
        (None, "https://api.deepseek.com", true),
        (Some("anthropic"), "https://relay.test", false),
    ] {
        let got = apply(body, preset, base, "anthropic-messages");
        assert_eq!(text(got).contains(r#"\\u0000"#), wanted);
    }
}

#[test]
fn v2_deepseek_patterns_special_property_names() {
    let input = r#"{
        "tools": [
            {
                "name": "SpecialTools",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "a\\b": {"type": "string", "pattern": "^[^\\0]*$"},
                        ":x": {"type": "string", "pattern": "\\0"},
                        "@special": {"type": "string", "pattern": "\\0"},
                        "a|b": {"type": "string", "pattern": "\\0"}
                    }
                }
            }
        ]
    }"#;
    let expected = input.replace(r#"\\0"#, r#"\\u0000"#);
    assert_eq!(
        text(apply(
            input,
            Some("deepseek"),
            "https://api.deepseek.com",
            "anthropic-messages"
        )),
        expected
    );
}

#[test]
fn v2_deepseek_patterns_preserve_caller_spare_capacity() {
    let mut body = Vec::with_capacity(4096);
    body.extend_from_slice(
        br#"{"tools":[{"name":"Artifact","input_schema":{"pattern":"^[^\\0]*$"}}]}"#,
    );
    let original = body.clone();
    let got = apply(
        std::str::from_utf8(&body).expect("body"),
        Some("deepseek"),
        "https://api.deepseek.com",
        "anthropic-messages",
    );
    assert_eq!(body, original);
    assert_ne!(got, body);
    assert!(text(got).contains(r#"\\u0000"#));
}

#[test]
fn v2_ecma_class_for_rust_cases() {
    for (input, expected) in [
        (r#"^[^\s[\]]+$"#, r#"^[^\s\[\]]+$"#),
        (r#"^[^\s\[\]]+$"#, r#"^[^\s\[\]]+$"#),
        (r#"[[:alpha:]]"#, r#"[\[:alpha:]]"#),
        (r#"[a&&b]"#, r#"[a&\&b]"#),
        (r#"[+--]"#, r#"[+-\-]"#),
        (r#"[~~]"#, r#"[~\~]"#),
        (r#"a&&b--c~~d\[x"#, r#"a&&b--c~~d\[x"#),
        (
            r#"^https?://[a-z]+\.[a-z]{2,}$"#,
            r#"^https?://[a-z]+\.[a-z]{2,}$"#,
        ),
        (r#"\\[a[b]"#, r#"\\[a\[b]"#),
    ] {
        let body =
            json!({"model":"grok-4.6","tools":[{"input_schema":{"pattern":input}}]}).to_string();
        let got = json_body(apply(
            &body,
            None,
            "https://relay.example/v1",
            "openai-chat",
        ));
        assert_eq!(
            got["tools"][0]["input_schema"]["pattern"], expected,
            "{input}"
        );
    }
}

const GROK_TOOLS: &str = r#""tools":[{"type":"function","function":{"name":"set_breakpoints","description":"<a> & b","parameters":{"type":"object","properties":{"breakpoints":{"type":"array","items":{"oneOf":[{"type":"object","properties":{"line":{"type":"integer"}}},{"type":"object","properties":{"class_name":{"allOf":[{"type":"string"},{"pattern":"^[^\\s[\\]]+$"}]},"selector":{"allOf":[{"type":"string"},{"pattern":"^[^\\s[\\]]+$"}]}}}]}}}}}}]"#;

#[test]
fn v2_grok_tool_patterns_2317() {
    let body = format!(
        r#"{{"model":"grok-4.6","messages":[{{"role":"user","content":"[^\\s[\\]]"}}],{GROK_TOOLS}}}"#
    );
    let expected = body.replace(r#""^[^\\s[\\]]+$""#, r#""^[^\\s\\[\\]]+$""#);
    assert_eq!(
        text(apply(
            &body,
            None,
            "https://relay.example/v1",
            "openai-chat"
        )),
        expected
    );
    for model in ["xai/grok-4.7", "x-ai/grok-4"] {
        let aliased = body.replacen("grok-4.6", model, 1);
        assert_ne!(
            text(apply(
                &aliased,
                None,
                "https://relay.example/v1",
                "openai-chat"
            )),
            aliased
        );
    }
    let aliased = body.replacen("grok-4.6", "my-alias", 1);
    assert_ne!(
        text(apply(&aliased, None, "https://api.x.ai/v1", "openai-chat")),
        aliased
    );
}

#[test]
fn v2_grok_other_models_are_unchanged() {
    for model in ["gpt-6-astra", "claude-sonnet-5", "deepseek-v4", "kimi-k3"] {
        let body = format!(r#"{{"model":"{model}",{GROK_TOOLS}}}"#);
        assert_eq!(
            text(apply(
                &body,
                None,
                "https://relay.example/v1",
                "openai-chat"
            )),
            body
        );
    }
    let body = r#"{"model":"grok-4.6","tools":[{"name":"x","input_schema":{"properties":{"u":{"pattern":"^https?://[a-z]+$"}}}}]}"#;
    assert_eq!(
        text(apply(
            body,
            None,
            "https://relay.example/v1",
            "anthropic-messages"
        )),
        body
    );
}

#[test]
fn v2_tool_one_of_as_any_of() {
    let tools = r#""tools":[{"type":"function","function":{"name":"automation_update","description":"takes \"oneOf\": a change","parameters":{"type":"object","properties":{"oneOf":{"type":"string"},"change":{"oneOf" : [{"type":"object","properties":{"paused":{"type":"boolean"}}},{"type":"object","properties":{"rule":{"oneOf":[{"type":"string"},{"type":"null"}]}}}]},"list":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"integer"}]}}}}}}]"#;
    let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"hi"}}],{tools}}}"#);
    let expected = tools
        .replace(r#""change":{"oneOf""#, r#""change":{"anyOf""#)
        .replace(r#""rule":{"oneOf""#, r#""rule":{"anyOf""#)
        .replace(r#""items":{"oneOf""#, r#""items":{"anyOf""#);
    for (preset, base) in [
        (Some("clinepass"), "https://api.cline.bot/api/v1"),
        (Some("kilo"), "https://api.kilo.ai/api/openrouter"),
        (None, "https://openrouter.ai/api/v1"),
    ] {
        let got = text(apply(&body, preset, base, "openai-chat"));
        assert!(got.contains(&expected), "{got}");
    }
    let other = text(apply(&body, None, "https://other.test/v1", "openai-chat"));
    assert!(other.contains(tools));
}

#[test]
fn v2_deepseek_token_field() {
    let body = r#" { "max_completion_tokens" : 64, "model":"m" } "#;
    assert_eq!(
        text(apply(
            body,
            Some("deepseek"),
            "https://api.deepseek.com/v1",
            "openai-chat"
        )),
        r#" { "max_tokens" : 64, "model":"m" } "#
    );
    for body in [
        r#"{"max_tokens":32,"model":"m"}"#,
        r#"{"max_tokens":32,"max_completion_tokens":64,"model":"m"}"#,
    ] {
        assert_eq!(
            text(apply(
                body,
                Some("deepseek"),
                "https://api.deepseek.com/v1",
                "openai-chat"
            )),
            body
        );
    }
    assert_eq!(
        text(apply(
            body,
            Some("deepseek"),
            "https://api.deepseek.com/v1",
            "openai-responses"
        )),
        body
    );
}

#[test]
fn v2_is_ark_hosts_match_reference() {
    // Reference draw_volcengine_test isArk hosts, plus a non-Volcengine
    // ark.* host.
    for (base, ark) in [
        ("https://ark.cn-beijing.volces.com/api/v3", true),
        ("https://ark.cn-beijing.volces.com/api/plan/v3", true),
        ("https://ark.ap-southeast.volces.com/api/v3", true),
        ("https://open.volces.com/api/v3", false),
        ("https://ark.example.com/api/v3", false),
    ] {
        let got = sampling(
            "kimi-k3",
            None,
            base,
            "openai-chat",
            r#""temperature":0.6,"top_p":0.95"#,
        );
        if ark {
            no_sampling(&got);
        } else {
            assert_eq!(got["temperature"], 0.6, "{base}");
            assert_eq!(got["top_p"], 0.95, "{base}");
        }
    }
}

#[test]
fn v2_locked_sampling_k24_keeps_temperature() {
    // Reference kimiLockedSamplingModel: K2 locks from minor 5 on.
    let got = sampling(
        "kimi-k2.4",
        Some("moonshot"),
        "https://api.moonshot.ai/v1",
        "openai-chat",
        r#""temperature":0.6"#,
    );
    assert_eq!(got["temperature"], 0.6);
}
