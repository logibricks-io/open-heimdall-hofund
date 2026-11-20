//! Strict flags on function tools sent to the ChatGPT Codex backend.
//!
//! The backend compiles a function tool's parameters unless the tool says
//! `strict: false`, and refuses the request when one schema does not compile.
//! Codex CLI sends `strict: false` on every function tool. A client that
//! leaves the flag out is held to the compiler. A client that asks for strict
//! keeps it when the schema still compiles, with each `oneOf` keyword written
//! as `anyOf`. A schema the compiler still refuses is sent as written with
//! `strict: false`.
//!
//! The keyword list and the lookaround check are the rules the reference
//! recorded against that backend: refused `oneOf`, `allOf`, `not`, `if`,
//! `uniqueItems`, `minProperties`, `contains`, `propertyNames`,
//! `dependentRequired`, an optional property, `additionalProperties` other
//! than false, a root `anyOf`, a lookaround, and a node with only `const`.
//! Taken: `anyOf` (nested, in `items`, in `$defs`), `$ref`, `enum` alone, a
//! type list, `const` with a type, min/max keywords, `format`, `pattern`,
//! `$schema`, `title`, and `examples`.

use hofund_plugin_sdk::json::{Map, Value};

/// Keywords a strict schema may use. Any other keyword is refused.
const STRICT_KEYS: &[&str] = &[
    "$comment",
    "$defs",
    "$id",
    "$ref",
    "$schema",
    "additionalProperties",
    "anyOf",
    "const",
    "default",
    "definitions",
    "description",
    "enum",
    "examples",
    "exclusiveMaximum",
    "exclusiveMinimum",
    "format",
    "items",
    "maxItems",
    "maxLength",
    "maximum",
    "minItems",
    "minLength",
    "minimum",
    "multipleOf",
    "pattern",
    "properties",
    "required",
    "title",
    "type",
];

/// Keywords whose value is an object of schemas by name.
const SCHEMA_MAPS: &[&str] = &[
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
];

/// Keywords whose value is one schema.
const SCHEMA_ONE: &[&str] = &[
    "items",
    "additionalProperties",
    "not",
    "if",
    "then",
    "else",
    "contains",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contentSchema",
];

/// Keywords whose value is a list of schemas.
const SCHEMA_LIST: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];

/// Gives every function tool in `body` a `strict` flag the backend can
/// compile. Returns whether one tool changed.
///
/// Function tools are read where tool names are read: the top-level `tools`
/// array, including tools nested in a namespace, and the `tools` of
/// `additional_tools` and `tool_search_output` input items.
pub(crate) fn tool_strict(body: &mut Map<String, Value>) -> bool {
    let mut changed = false;
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        changed |= tools_strict(tools);
    }
    if let Some(Value::Array(input)) = body.get_mut("input") {
        for item in input.iter_mut() {
            let Value::Object(item) = item else {
                continue;
            };
            let nested = matches!(
                item.get("type").and_then(Value::as_str),
                Some("additional_tools" | "tool_search_output")
            );
            if nested && let Some(Value::Array(tools)) = item.get_mut("tools") {
                changed |= tools_strict(tools);
            }
        }
    }
    changed
}

fn tools_strict(tools: &mut [Value]) -> bool {
    let mut changed = false;
    for tool in tools.iter_mut() {
        let Value::Object(tool) = tool else {
            continue;
        };
        let function = tool.get("type").and_then(Value::as_str) == Some("function");
        let namespace = tool.get("type").and_then(Value::as_str) == Some("namespace");
        if function {
            changed |= strict_tool(tool);
        } else if namespace && let Some(Value::Array(nested)) = tool.get_mut("tools") {
            changed |= tools_strict(nested);
        }
    }
    changed
}

/// Sets one function tool's `strict` as [`tool_strict`] describes.
fn strict_tool(tool: &mut Map<String, Value>) -> bool {
    match tool.get("strict") {
        Some(Value::Bool(false)) => return false,
        Some(Value::Bool(true)) => {}
        _ => {
            tool.insert("strict".to_owned(), Value::Bool(false));
            return true;
        }
    }
    let Some(params) = tool.get("parameters").and_then(Value::as_object).cloned() else {
        tool.insert("strict".to_owned(), Value::Bool(false));
        return true;
    };
    if !has_key_map(&params, "oneOf") {
        if schema_compiles(&params, true) {
            return false;
        }
        tool.insert("strict".to_owned(), Value::Bool(false));
        return true;
    }
    match one_of_as_any_of(&Value::Object(params)) {
        Value::Object(rewritten) if schema_compiles(&rewritten, true) => {
            tool.insert("parameters".to_owned(), Value::Object(rewritten));
        }
        _ => {
            tool.insert("strict".to_owned(), Value::Bool(false));
        }
    }
    true
}

fn has_key_map(object: &Map<String, Value>, key: &str) -> bool {
    object
        .iter()
        .any(|(name, child)| name == key || has_key(child, key))
}

fn has_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => has_key_map(object, key),
        Value::Array(items) => items.iter().any(|child| has_key(child, key)),
        _ => false,
    }
}

/// A copy of `value` with every `oneOf` keyword renamed to `anyOf`.
///
/// A property of that name is left alone. A schema that already has both
/// keywords keeps `oneOf`: the two say more together than `anyOf` can.
fn one_of_as_any_of(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(one_of_as_any_of).collect()),
        Value::Object(object) => {
            let mut out = object.clone();
            for key in SCHEMA_MAPS {
                if let Some(Value::Object(map)) = object.get(*key) {
                    let mut copied = Map::new();
                    for (name, sub) in map {
                        copied.insert(name.clone(), one_of_as_any_of(sub));
                    }
                    out.insert((*key).to_owned(), Value::Object(copied));
                }
            }
            for key in SCHEMA_ONE.iter().chain(SCHEMA_LIST.iter()) {
                if let Some(child) = object.get(*key) {
                    out.insert((*key).to_owned(), one_of_as_any_of(child));
                }
            }
            if out.contains_key("oneOf")
                && !out.contains_key("anyOf")
                && let Some(one) = out.remove("oneOf")
            {
                out.insert("anyOf".to_owned(), one);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn allowed_strict_key(key: &str) -> bool {
    STRICT_KEYS.contains(&key)
}

/// Whether the backend accepts `schema` inside a strict tool.
///
/// `root` is the tool's parameters, which have to be an object.
fn schema_compiles(schema: &Map<String, Value>, root: bool) -> bool {
    if schema.keys().any(|key| !allowed_strict_key(key)) {
        return false;
    }
    if root && schema.get("type").and_then(Value::as_str) != Some("object") {
        return false;
    }
    if schema
        .get("pattern")
        .and_then(Value::as_str)
        .is_some_and(has_lookaround)
    {
        return false;
    }
    if !names_a_type(schema) {
        return false;
    }
    if is_type(schema.get("type"), "object") {
        if !object_compiles(schema) {
            return false;
        }
    } else if schema.contains_key("additionalProperties") {
        return false;
    }
    if is_type(schema.get("type"), "array") && !array_compiles(schema) {
        return false;
    }
    if schema.contains_key("anyOf") && !union_compiles(schema) {
        return false;
    }
    defs_compile(schema)
}

fn names_a_type(schema: &Map<String, Value>) -> bool {
    ["type", "anyOf", "$ref", "enum"]
        .iter()
        .any(|key| schema.contains_key(*key))
}

fn has_lookaround(pattern: &str) -> bool {
    pattern.contains("(?=")
        || pattern.contains("(?!")
        || pattern.contains("(?<=")
        || pattern.contains("(?<!")
}

fn is_type(value: Option<&Value>, name: &str) -> bool {
    match value {
        Some(Value::String(text)) => text == name,
        Some(Value::Array(items)) => items.iter().any(|item| item.as_str() == Some(name)),
        _ => false,
    }
}

fn object_compiles(schema: &Map<String, Value>) -> bool {
    let Some(Value::Object(properties)) = schema.get("properties") else {
        return false;
    };
    if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
        return false;
    }
    let required = required_names(schema);
    properties.iter().all(|(name, sub)| {
        required.contains(&name.as_str())
            && sub
                .as_object()
                .is_some_and(|sub| schema_compiles(sub, false))
    })
}

fn required_names(schema: &Map<String, Value>) -> Vec<&str> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn array_compiles(schema: &Map<String, Value>) -> bool {
    schema
        .get("items")
        .and_then(Value::as_object)
        .is_some_and(|items| schema_compiles(items, false))
}

fn union_compiles(schema: &Map<String, Value>) -> bool {
    let Some(list) = schema.get("anyOf").and_then(Value::as_array) else {
        return false;
    };
    !list.is_empty()
        && list.iter().all(|branch| {
            branch
                .as_object()
                .is_some_and(|branch| schema_compiles(branch, false))
        })
}

fn defs_compile(schema: &Map<String, Value>) -> bool {
    ["$defs", "definitions"]
        .iter()
        .all(|key| match schema.get(*key) {
            None => true,
            Some(Value::Object(defs)) => defs.values().all(|sub| {
                sub.as_object()
                    .is_some_and(|sub| schema_compiles(sub, false))
            }),
            Some(_) => false,
        })
}

#[cfg(test)]
mod tests {
    use super::{strict_tool, tool_strict};
    use hofund_plugin_sdk::json::{self, Map, Value};

    fn object(raw: &str) -> Map<String, Value> {
        match json_value(raw) {
            Value::Object(object) => object,
            other => panic!("object, got {other}"),
        }
    }

    fn json_value(raw: &str) -> Value {
        json::from_str(raw).unwrap_or_else(|error| panic!("{error}: {raw}"))
    }

    fn closed(properties: &str, required: &str) -> String {
        format!(
            r#"{{"type":"object","additionalProperties":false,"properties":{properties},"required":{required}}}"#
        )
    }

    struct Case {
        name: &'static str,
        tool: String,
        strict: bool,
        /// `None` means the parameters stay as sent.
        params: Option<String>,
    }

    #[test]
    fn d242_function_tool_strict_follows_the_compile_rules() {
        let cases = [
            Case {
                name: "left out",
                tool: format!(
                    r#"{{"type":"function","name":"f","parameters":{}}}"#,
                    closed(
                        r#"{"q":{"oneOf":[{"type":"string"},{"type":"number"}]}}"#,
                        r#"["q"]"#
                    )
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "not a bool",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":"yes","parameters":{}}}"#,
                    closed(r#"{"q":{"type":"string"}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "false kept",
                tool: r#"{"type":"function","name":"f","strict":false,"parameters":{"type":"object","properties":{"q":{"allOf":[{"type":"string"}]}}}}"#.to_owned(),
                strict: false,
                params: None,
            },
            Case {
                name: "true fits",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(
                        r#"{"q":{"anyOf":[{"type":"string"},{"type":"null"}]},"n":{"type":["string","number"]}}"#,
                        r#"["q","n"]"#
                    )
                ),
                strict: true,
                params: None,
            },
            Case {
                name: "true, oneOf as anyOf",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(
                        r#"{"q":{"oneOf":[{"type":"string"},{"type":"number"}]},"oneOf":{"type":"string"}}"#,
                        r#"["q","oneOf"]"#
                    )
                ),
                strict: true,
                params: Some(closed(
                    r#"{"q":{"anyOf":[{"type":"string"},{"type":"number"}]},"oneOf":{"type":"string"}}"#,
                    r#"["q","oneOf"]"#,
                )),
            },
            Case {
                name: "true, oneOf in items and $defs",
                tool: r##"{"type":"function","name":"f","strict":true,"parameters":{"type":"object","additionalProperties":false,"required":["a"],"properties":{"a":{"type":"array","items":{"oneOf":[{"$ref":"#/$defs/x"},{"type":"null"}]}}},"$defs":{"x":{"oneOf":[{"type":"integer","const":1},{"type":"integer","const":2}]}}}}"##.to_owned(),
                strict: true,
                params: Some(r##"{"type":"object","additionalProperties":false,"required":["a"],"properties":{"a":{"type":"array","items":{"anyOf":[{"$ref":"#/$defs/x"},{"type":"null"}]}}},"$defs":{"x":{"anyOf":[{"type":"integer","const":1},{"type":"integer","const":2}]}}}"##.to_owned()),
            },
            Case {
                name: "true, oneOf and an optional property",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(
                        r#"{"q":{"oneOf":[{"type":"string"},{"type":"number"}]},"p":{"type":"number"}}"#,
                        r#"["q"]"#
                    )
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, allOf",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"allOf":[{"type":"string"}]}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, if",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"type":"string","if":{"const":"a"}}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, not",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"not":{"type":"null"}}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, uniqueItems",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(
                        r#"{"q":{"type":"array","items":{"type":"string"},"uniqueItems":true}}"#,
                        r#"["q"]"#
                    )
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, lookaround",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"type":"string","pattern":"^(?!/etc)"}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, more allowed",
                tool: r#"{"type":"function","name":"f","strict":true,"parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}}"#.to_owned(),
                strict: false,
                params: None,
            },
            Case {
                name: "true, open object inside",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"type":"object"}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, a const alone",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(
                        r#"{"q":{"oneOf":[{"const":"a"},{"const":"b"}]}}"#,
                        r#"["q"]"#
                    )
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, an enum alone",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(
                        r#"{"q":{"oneOf":[{"enum":["a"]},{"type":"number"}]}}"#,
                        r#"["q"]"#
                    )
                ),
                strict: true,
                params: Some(closed(
                    r#"{"q":{"anyOf":[{"enum":["a"]},{"type":"number"}]}}"#,
                    r#"["q"]"#,
                )),
            },
            Case {
                name: "true, root anyOf",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{{"anyOf":[{}]}}}}"#,
                    closed(r#"{"q":{"type":"string"}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, no parameters",
                tool: r#"{"type":"function","name":"f","strict":true}"#.to_owned(),
                strict: false,
                params: None,
            },
            Case {
                name: "true, oneOf beside anyOf",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(
                        r#"{"q":{"oneOf":[{"type":"string"}],"anyOf":[{"type":"string"}]}}"#,
                        r#"["q"]"#
                    )
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, lookahead",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"type":"string","pattern":"a(?=b)"}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, lookbehind",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"type":"string","pattern":"(?<=a)b"}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, negative lookbehind",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"type":"string","pattern":"(?<!a)b"}}"#, r#"["q"]"#)
                ),
                strict: false,
                params: None,
            },
            Case {
                name: "true, plain pattern",
                tool: format!(
                    r#"{{"type":"function","name":"f","strict":true,"parameters":{}}}"#,
                    closed(r#"{"q":{"type":"string","pattern":"^[a-z]+$"}}"#, r#"["q"]"#)
                ),
                strict: true,
                params: None,
            },
            Case {
                name: "true, definitions",
                tool: r##"{"type":"function","name":"f","strict":true,"parameters":{"type":"object","additionalProperties":false,"required":["q"],"properties":{"q":{"$ref":"#/definitions/q"}},"definitions":{"q":{"type":"string"}}}}"##.to_owned(),
                strict: true,
                params: None,
            },
            Case {
                name: "true, bad definitions",
                tool: r#"{"type":"function","name":"f","strict":true,"parameters":{"type":"object","additionalProperties":false,"required":["q"],"properties":{"q":{"type":"string"}},"definitions":{"q":{"allOf":[{"type":"string"}]}}}}"#.to_owned(),
                strict: false,
                params: None,
            },
        ];
        for case in cases {
            let mut tool = object(&case.tool);
            let before = tool.get("parameters").cloned();
            strict_tool(&mut tool);
            assert_eq!(
                tool.get("strict"),
                Some(&Value::Bool(case.strict)),
                "{}",
                case.name
            );
            let want = case.params.as_deref().map(json_value).or(before);
            assert_eq!(tool.get("parameters").cloned(), want, "{}", case.name);
        }
    }

    #[test]
    fn d242_strict_walks_every_function_tool_list() {
        let mut body = object(
            r#"{"tools":[{"type":"function","name":"a"},{"type":"namespace","name":"ns","tools":[{"type":"function","name":"b"}]},{"type":"web_search"}],
            "input":[{"type":"additional_tools","tools":[{"type":"function","name":"c"}]},{"type":"tool_search_output","tools":[{"type":"function","name":"d"}]},{"type":"message","role":"user","content":"x"}]}"#,
        );
        assert!(tool_strict(&mut body), "nothing changed");
        let value = Value::Object(body.clone());
        for name in ["a", "b", "c", "d"] {
            let tool = find_function(&value, name);
            assert_eq!(
                tool.get("strict"),
                Some(&Value::Bool(false)),
                "{name} strict"
            );
            assert_eq!(tool.get("type").and_then(Value::as_str), Some("function"));
        }
        let search = find_typed(&value, "web_search");
        assert!(search.get("strict").is_none(), "web_search gained strict");
        assert!(!tool_strict(&mut body), "a second pass changed something");
    }

    fn find_function<'a>(value: &'a Value, name: &str) -> &'a Map<String, Value> {
        find(value, |object| {
            object.get("type").and_then(Value::as_str) == Some("function")
                && object.get("name").and_then(Value::as_str) == Some(name)
        })
        .unwrap_or_else(|| panic!("no function {name}"))
    }

    fn find_typed<'a>(value: &'a Value, kind: &str) -> &'a Map<String, Value> {
        find(value, |object| {
            object.get("type").and_then(Value::as_str) == Some(kind)
        })
        .unwrap_or_else(|| panic!("no {kind}"))
    }

    fn find(
        value: &Value,
        pred: impl Fn(&Map<String, Value>) -> bool + Copy,
    ) -> Option<&Map<String, Value>> {
        match value {
            Value::Array(items) => items.iter().find_map(|item| find(item, pred)),
            Value::Object(object) => {
                if pred(object) {
                    return Some(object);
                }
                object.values().find_map(|child| find(child, pred))
            }
            _ => None,
        }
    }

    #[test]
    fn d242_body_sends_a_strict_the_backend_can_compile() {
        let defintegrate = closed(
            r#"{"expression":{"type":"string"},"lowerBound":{"oneOf":[{"type":"string"},{"type":"number"}]},"upperBound":{"oneOf":[{"type":"string"},{"type":"number"}]},"variable":{"type":"string"}}"#,
            r#"["expression","variable","lowerBound","upperBound"]"#,
        );
        let calculate = closed(
            r#"{"expression":{"type":"string"},"precision":{"type":"number","minimum":0,"maximum":20}}"#,
            r#"["expression"]"#,
        );
        let mut body = object(&format!(
            r#"{{"model":"gpt-5.5","input":"integrate","tools":[
                {{"type":"function","name":"defintegrate","parameters":{defintegrate}}},
                {{"type":"function","name":"calculate","strict":true,"parameters":{calculate}}},
                {{"type":"function","name":"edit","strict":true,"parameters":{defintegrate}}},
                {{"type":"web_search"}}
            ]}}"#
        ));
        let _headers = crate::shape::body(&mut body, None);
        let tools = body.get("tools").and_then(Value::as_array).expect("tools");
        let tool = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
                .unwrap_or_else(|| panic!("no {name}"))
        };
        let loose = tool("defintegrate");
        let kept = json_value(&defintegrate);
        assert_eq!(loose.get("strict"), Some(&Value::Bool(false)));
        assert_eq!(
            loose.get("parameters"),
            Some(&kept),
            "omitted strict keeps the oneOf"
        );
        let optional = tool("calculate");
        assert_eq!(optional.get("strict"), Some(&Value::Bool(false)));
        assert!(
            optional
                .pointer("/parameters/properties/precision")
                .is_some()
        );
        let edit = tool("edit");
        assert_eq!(edit.get("strict"), Some(&Value::Bool(true)));
        assert!(
            !edit.to_string().contains("\"oneOf\""),
            "edit kept oneOf: {edit}"
        );
        assert_eq!(
            edit.pointer("/parameters/properties/lowerBound/anyOf")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
        let search = tools
            .iter()
            .find(|tool| tool.get("type").and_then(Value::as_str) == Some("web_search"))
            .expect("web_search");
        assert!(search.get("strict").is_none());
    }

    /// Chat and Messages declare `strict` only in the tool extension bag.
    /// `encode_outbound` is the body [`hofund_plugin_sdk::wire`] prepares
    /// before a subscription rewrite, including the cross-protocol strip.
    /// The encoder must still omit a flag the client left out. After
    /// [`crate::shape::body`], a compiling `strict: true` stays true and
    /// each `oneOf` is `anyOf`; a refused schema is unchanged with `strict`
    /// false; an omitted flag becomes false with the schema unchanged.
    #[test]
    fn d242_cross_protocol_strict_reaches_shape() {
        let compiles = closed(
            r#"{"q":{"oneOf":[{"type":"string"},{"type":"number"}]}}"#,
            r#"["q"]"#,
        );
        let refused = closed(r#"{"q":{"allOf":[{"type":"string"}]}}"#, r#"["q"]"#);
        let chat = format!(
            r#"{{"model":"gpt-5","messages":[{{"role":"user","content":"go"}}],"tools":[
                {{"type":"function","function":{{"name":"compiles","strict":true,"parameters":{compiles}}}}},
                {{"type":"function","function":{{"name":"refused","strict":true,"parameters":{refused}}}}},
                {{"type":"function","function":{{"name":"omitted","parameters":{compiles}}}}}
            ]}}"#
        );
        let messages = format!(
            r#"{{"model":"gpt-5","max_tokens":16,"messages":[{{"role":"user","content":"go"}}],"tools":[
                {{"name":"compiles","strict":true,"input_schema":{compiles}}},
                {{"name":"refused","strict":true,"input_schema":{refused}}},
                {{"name":"omitted","input_schema":{compiles}}}
            ]}}"#
        );
        hop("chat", &chat, hofund_plugin_api::ProtocolId::OPENAI_CHAT);
        hop(
            "messages",
            &messages,
            hofund_plugin_api::ProtocolId::ANTHROPIC_MESSAGES,
        );
    }

    fn hop(label: &str, raw: &str, inbound: hofund_plugin_api::ProtocolId) {
        let request = hofund_plugin_sdk::wire::decode_inbound(inbound, raw.as_bytes())
            .unwrap_or_else(|error| panic!("{label} decode: {error}"));
        let encoded = hofund_plugin_sdk::wire::encode_outbound(
            hofund_plugin_api::ProtocolId::OPENAI_RESPONSES,
            inbound,
            request,
        )
        .unwrap_or_else(|error| panic!("{label} encode: {error}"));
        let mut body = object(&String::from_utf8(encoded).expect("utf8"));
        let tool = |body: &Map<String, Value>, name: &str| {
            body.get("tools")
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("{label} tools"))
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
                .unwrap_or_else(|| panic!("{label} no {name}"))
                .as_object()
                .unwrap_or_else(|| panic!("{label} {name} object"))
                .clone()
        };
        let omitted = tool(&body, "omitted");
        assert!(
            omitted.get("strict").is_none(),
            "{label} encoder invented strict"
        );
        let compiles_before = tool(&body, "compiles");
        assert_eq!(
            compiles_before.get("strict"),
            Some(&Value::Bool(true)),
            "{label} lost strict before shaping"
        );
        assert!(
            Value::Object(compiles_before.clone())
                .pointer("/parameters/properties/q/oneOf")
                .is_some()
        );
        let refused_before = tool(&body, "refused");
        assert_eq!(refused_before.get("strict"), Some(&Value::Bool(true)));
        let refused_params = refused_before.get("parameters").cloned();
        let omitted_params = omitted.get("parameters").cloned();

        crate::shape::body(&mut body, None);

        let compiles = tool(&body, "compiles");
        assert_eq!(
            compiles.get("strict"),
            Some(&Value::Bool(true)),
            "{label} compiling strict"
        );
        let compiles_value = Value::Object(compiles.clone());
        assert!(
            compiles_value
                .pointer("/parameters/properties/q/oneOf")
                .is_none()
        );
        assert_eq!(
            compiles_value
                .pointer("/parameters/properties/q/anyOf")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2),
            "{label} oneOf was not rewritten"
        );
        let refused = tool(&body, "refused");
        assert_eq!(
            refused.get("strict"),
            Some(&Value::Bool(false)),
            "{label} refused strict"
        );
        assert_eq!(
            refused.get("parameters").cloned(),
            refused_params,
            "{label} refused schema changed"
        );
        let omitted = tool(&body, "omitted");
        assert_eq!(
            omitted.get("strict"),
            Some(&Value::Bool(false)),
            "{label} omitted strict"
        );
        assert_eq!(
            omitted.get("parameters").cloned(),
            omitted_params,
            "{label} omitted schema changed"
        );
    }
}
