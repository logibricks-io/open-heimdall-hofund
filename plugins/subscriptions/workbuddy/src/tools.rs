use std::collections::{HashMap, HashSet};

use hofund_plugin_sdk::json::{Map, Value, json};

const SUB_ONE: &[&str] = &[
    "items",
    "additionalItems",
    "unevaluatedItems",
    "contains",
    "not",
    "if",
    "then",
    "else",
    "propertyNames",
    "additionalProperties",
    "unevaluatedProperties",
];
const SUB_LIST: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];
const SUB_MAP: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];
const TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "null",
];
const EFFORTLESS_NAME: char = '_';

pub(crate) fn prepare(body: &mut Map<String, Value>, after_tools: bool) -> HashMap<String, String> {
    let names = fit_tools(body);
    normalize_tool_choice(body);
    if after_tools {
        reply_after_tools(body);
    }
    names
}

pub(crate) fn restore(value: &mut Value, names: &HashMap<String, String>) {
    let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    for choice in choices {
        for key in ["message", "delta"] {
            let Some(calls) = choice
                .get_mut(key)
                .and_then(|message| message.get_mut("tool_calls"))
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            for call in calls {
                let Some(name) = call
                    .get_mut("function")
                    .and_then(|function| function.get_mut("name"))
                    .and_then(|value| value.as_str())
                    .and_then(|name| names.get(name))
                    .cloned()
                else {
                    continue;
                };
                call["function"]["name"] = Value::String(name);
            }
        }
    }
}

pub(crate) fn after_tools_refusal(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    if lower.contains("must end with a user message") {
        return true;
    }
    ["\"code\"", "'code'"].into_iter().any(|key| {
        lower.match_indices(key).any(|(at, _)| {
            let rest = lower[at + key.len()..].trim_start();
            let Some(rest) = rest.strip_prefix(':') else {
                return false;
            };
            let rest = rest.trim_start();
            let rest = rest
                .strip_prefix('"')
                .or_else(|| rest.strip_prefix('\''))
                .unwrap_or(rest);
            let Some(rest) = rest.strip_prefix("11153") else {
                return false;
            };
            rest.chars()
                .next()
                .is_none_or(|c| !matches!(c, '.' | '0'..='9'))
        })
    })
}

fn reply_after_tools(body: &mut Map<String, Value>) -> bool {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut at = messages.len();
    while at > 0
        && messages[at - 1]
            .get("role")
            .and_then(|value| value.as_str())
            == Some("user")
    {
        at -= 1;
    }
    if at == messages.len()
        || at == 0
        || messages[at - 1]
            .get("role")
            .and_then(|value| value.as_str())
            != Some("tool")
    {
        return false;
    }
    messages.insert(at, json!({"role": "assistant", "content": ""}));
    true
}

fn fit_tools(body: &mut Map<String, Value>) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return names;
    };
    if tools.is_empty() {
        return names;
    }
    let mut taken = HashSet::new();
    for tool in tools.iter() {
        if let Some(name) = tool
            .pointer("/function/name")
            .and_then(|value| value.as_str())
            && safe_name(name)
        {
            taken.insert(name.to_owned());
        }
    }
    let mut renamed = HashMap::new();
    for tool in tools {
        let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) else {
            continue;
        };
        if let Some(name) = function.get("name").and_then(|value| value.as_str())
            && !safe_name(name)
        {
            let original = name.to_owned();
            let safe = unique_name(&original, &mut taken);
            function.insert("name".to_owned(), Value::String(safe.clone()));
            names.insert(safe.clone(), original.clone());
            renamed.insert(original, safe);
        }
        let Some(parameters) = function.get_mut("parameters") else {
            continue;
        };
        if !parameters.is_object() {
            *parameters = json!({"type": "object", "properties": {}});
            continue;
        }
        fit_schema(parameters, 0);
        fit_shape(parameters, 0);
        type_unions(parameters);
        object_root(parameters);
    }
    if !renamed.is_empty() {
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    for call in calls {
                        rename_field(call.pointer_mut("/function/name"), &renamed);
                    }
                }
                if message.get("role").and_then(|value| value.as_str()) == Some("tool") {
                    rename_field(message.get_mut("name"), &renamed);
                }
            }
        }
        rename_field(
            body.get_mut("tool_choice")
                .and_then(|choice| choice.get_mut("function"))
                .and_then(|function| function.get_mut("name")),
            &renamed,
        );
    }
    names
}

fn rename_field(field: Option<&mut Value>, names: &HashMap<String, String>) {
    let Some(field) = field else {
        return;
    };
    let Some(replacement) = field.as_str().and_then(|name| names.get(name)).cloned() else {
        return;
    };
    *field = Value::String(replacement);
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn unique_name(name: &str, taken: &mut HashSet<String>) -> String {
    let mut base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                EFFORTLESS_NAME
            }
        })
        .collect();
    if base.is_empty() {
        base.push_str("tool");
    }
    let mut candidate = base.clone();
    let mut suffix = 2;
    while taken.contains(&candidate) {
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
    taken.insert(candidate.clone());
    candidate
}

fn fit_schema(schema: &mut Value, depth: usize) {
    if depth > 64 {
        return;
    }
    match schema {
        Value::Array(items) => {
            for item in items {
                fit_schema(item, depth + 1);
            }
        }
        Value::Object(object) => {
            if let Some(pattern) = object.get_mut("pattern").and_then(|value| value.as_str()) {
                let fitted = fit_pattern(pattern);
                if fitted != pattern {
                    object.insert("pattern".to_owned(), Value::String(fitted));
                }
            }
            if let Some(items) = object.get("items").and_then(Value::as_array) {
                let schemas: Vec<Value> = items
                    .iter()
                    .filter(|item| matches!(item, Value::Object(_) | Value::Array(_)))
                    .cloned()
                    .collect();
                let replacement = match schemas.len() {
                    0 => json!({}),
                    1 => schemas.into_iter().next().unwrap_or_else(|| json!({})),
                    _ => json!({"anyOf": schemas}),
                };
                object.insert("items".to_owned(), replacement);
                object.remove("additionalItems");
            }
            for value in object.values_mut() {
                fit_schema(value, depth + 1);
            }
        }
        _ => {}
    }
}

fn fit_pattern(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\\' && at + 1 < bytes.len() {
            if bytes[at + 1] == b'\\' {
                out.extend_from_slice(&bytes[at..at + 2]);
                at += 2;
                continue;
            }
            if bytes[at + 1] == b'0' && bytes.get(at + 2).is_none_or(|byte| !byte.is_ascii_digit())
            {
                out.extend_from_slice(br"\u0000");
                at += 2;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| pattern.to_owned())
}

fn fit_shape(schema: &mut Value, depth: usize) {
    if depth > 64 {
        return;
    }
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if let Some(kind) = object.get("type") {
        let listed: Vec<Value> = match kind {
            Value::Array(items) => items.clone(),
            item => vec![item.clone()],
        };
        let mut types = Vec::new();
        for item in listed {
            let Some(kind) = item.as_str().map(str::to_ascii_lowercase) else {
                continue;
            };
            if TYPES.contains(&kind.as_str()) && !types.contains(&kind) {
                types.push(kind);
            }
        }
        let replacement = match types.len() {
            0 => None,
            1 => Some(Value::String(types.remove(0))),
            _ => Some(Value::Array(types.into_iter().map(Value::String).collect())),
        };
        if replacement.as_ref() != Some(kind) {
            match replacement {
                Some(value) => object.insert("type".to_owned(), value),
                None => object.remove("type"),
            };
        }
    }
    match object.get_mut("required") {
        Some(Value::Array(required)) => required.retain(|item| item.is_string()),
        Some(_) => {
            object.remove("required");
        }
        None => {}
    }
    fit_enum(object);
    for key in SUB_ONE {
        let Some(value) = object.get_mut(*key) else {
            continue;
        };
        boolean_schema(value, key);
        fit_shape(value, depth + 1);
    }
    for key in SUB_LIST {
        let Some(values) = object.get_mut(*key).and_then(Value::as_array_mut) else {
            continue;
        };
        for value in values {
            boolean_schema(value, key);
            fit_shape(value, depth + 1);
        }
    }
    for key in SUB_MAP {
        let Some(values) = object.get_mut(*key).and_then(Value::as_object_mut) else {
            continue;
        };
        for value in values.values_mut() {
            boolean_schema(value, key);
            fit_shape(value, depth + 1);
        }
    }
}

fn boolean_schema(value: &mut Value, key: &str) {
    if matches!(key, "additionalProperties" | "unevaluatedProperties") {
        return;
    }
    match value {
        Value::Bool(true) => *value = json!({}),
        Value::Bool(false) => *value = json!({"not": {}}),
        _ => {}
    }
}

fn fit_enum(object: &mut Map<String, Value>) {
    let Some(enum_value) = object.get("enum") else {
        return;
    };
    let Some(values) = enum_value.as_array().filter(|values| !values.is_empty()) else {
        object.remove("enum");
        return;
    };
    let listed_non_null = match object.get("type") {
        Some(Value::String(kind)) => usize::from(kind != "null"),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter(|kind| kind.as_str() != Some("null"))
            .count(),
        _ => 0,
    };
    if listed_non_null == 1 {
        return;
    }
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    let mut has_null = false;
    for value in values {
        let kind = value_type(value);
        if kind == "null" {
            has_null = true;
        }
        if let Some((_, values)) = groups.iter_mut().find(|(name, _)| *name == kind) {
            values.push(value.clone());
        } else {
            groups.push((kind.to_owned(), vec![value.clone()]));
        }
    }
    if groups.iter().any(|(kind, _)| kind == "number")
        && let Some(at) = groups.iter().position(|(kind, _)| kind == "integer")
    {
        let (_, integers) = groups.remove(at);
        if let Some((_, numbers)) = groups.iter_mut().find(|(kind, _)| kind == "number") {
            numbers.extend(integers);
        }
    }
    groups.retain(|(kind, _)| kind != "null");
    if groups.len() <= 1 {
        let replacement = match groups.first() {
            None => Value::String("null".to_owned()),
            Some((kind, _)) if has_null => json!([kind, "null"]),
            Some((kind, _)) => Value::String(kind.clone()),
        };
        if object.get("type") != Some(&replacement) {
            object.insert("type".to_owned(), replacement);
        }
        return;
    }
    let mut split: Vec<Value> = groups
        .into_iter()
        .map(|(kind, values)| json!({"type": kind, "enum": values}))
        .collect();
    if has_null {
        split.push(json!({"type": "null"}));
    }
    object.remove("enum");
    object.remove("type");
    if object.get("anyOf").and_then(Value::as_array).is_some() {
        let union = json!({"anyOf": split});
        match object.get_mut("allOf").and_then(Value::as_array_mut) {
            Some(all) => all.push(union),
            None => {
                object.insert("allOf".to_owned(), Value::Array(vec![union]));
            }
        }
    } else {
        object.insert("anyOf".to_owned(), Value::Array(split));
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.as_f64().is_some_and(|n| n.fract() == 0.0) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_unions(schema: &mut Value) {
    let root = schema.clone();
    type_unions_at(schema, &root, 0);
}

fn type_unions_at(schema: &mut Value, root: &Value, depth: usize) {
    if depth > 64 {
        return;
    }
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let object_union = !object.contains_key("type")
        && object
            .get("oneOf")
            .and_then(Value::as_array)
            .is_some_and(|branches| {
                !branches.is_empty()
                    && branches
                        .iter()
                        .all(|branch| object_branch(root, branch, &mut Vec::new()))
            });
    if object_union {
        object.insert("type".to_owned(), Value::String("object".to_owned()));
    }
    for key in SUB_ONE {
        if let Some(value) = object.get_mut(*key) {
            type_unions_at(value, root, depth + 1);
        }
    }
    for key in SUB_LIST {
        if let Some(values) = object.get_mut(*key).and_then(Value::as_array_mut) {
            for value in values {
                type_unions_at(value, root, depth + 1);
            }
        }
    }
    for key in SUB_MAP {
        if let Some(values) = object.get_mut(*key).and_then(Value::as_object_mut) {
            for value in values.values_mut() {
                type_unions_at(value, root, depth + 1);
            }
        }
    }
}

fn object_branch(root: &Value, branch: &Value, seen: &mut Vec<String>) -> bool {
    let Some(object) = branch.as_object() else {
        return false;
    };
    if let Some(kind) = object.get("type") {
        return kind.as_str() == Some("object");
    }
    let Some(reference) = object.get("$ref").and_then(|value| value.as_str()) else {
        return false;
    };
    if seen.iter().any(|item| item == reference) || seen.len() > 16 {
        return false;
    }
    let Some(target) = local_ref(root, branch) else {
        return false;
    };
    if target == branch {
        return false;
    }
    seen.push(reference.to_owned());
    object_branch(root, target, seen)
}

fn local_ref<'a>(root: &'a Value, schema: &'a Value) -> Option<&'a Value> {
    let Some(reference) = schema.get("$ref").and_then(|value| value.as_str()) else {
        return Some(schema);
    };
    if !reference.starts_with("#/") {
        return Some(schema);
    }
    let mut target = root;
    for token in reference[2..].split('/') {
        let token = token.replace("~1", "/").replace("~0", "~");
        target = target.get(token)?;
    }
    (target.is_object() || target.is_array()).then_some(target)
}

fn object_root(schema: &mut Value) {
    let root = schema.clone();
    object_root_at(schema, &root, 0);
}

fn object_root_at(schema: &mut Value, root: &Value, depth: usize) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let unions: Vec<&str> = ["allOf", "anyOf", "oneOf"]
        .into_iter()
        .filter(|key| object.get(*key).and_then(Value::as_array).is_some())
        .collect();
    if object.get("type").and_then(|value| value.as_str()) == Some("object") && unions.is_empty() {
        return;
    }
    let mut properties = object
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut required = object
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(branches) = object.get("allOf").and_then(Value::as_array) {
        for original in branches {
            let Some(branch) = local_ref(root, original)
                .and_then(Value::as_object)
                .cloned()
            else {
                continue;
            };
            let mut branch = Value::Object(branch);
            if depth < 8 && has_union(&branch) {
                object_root_at(&mut branch, root, depth + 1);
            }
            merge_properties(&mut properties, &branch);
            if let Some(items) = branch.get("required").and_then(Value::as_array) {
                required.extend(items.clone());
            }
        }
    }
    let mut branches = Vec::new();
    for key in ["anyOf", "oneOf"] {
        if let Some(items) = object.get(key).and_then(Value::as_array) {
            for original in items {
                let Some(branch) = local_ref(root, original)
                    .and_then(Value::as_object)
                    .cloned()
                else {
                    continue;
                };
                let mut branch = Value::Object(branch);
                if depth < 8 && has_union(&branch) {
                    object_root_at(&mut branch, root, depth + 1);
                }
                if branch.get("type").and_then(|value| value.as_str()) != Some("object")
                    && branch.get("properties").is_none()
                {
                    continue;
                }
                branches.push(branch);
            }
        }
    }
    let mut from: Vec<(String, Vec<Value>)> = Vec::new();
    for branch in &branches {
        if let Some(items) = branch.get("properties").and_then(Value::as_object) {
            for (name, value) in items {
                if properties.contains_key(name) {
                    continue;
                }
                if let Some((_, values)) = from.iter_mut().find(|(key, _)| key == name) {
                    if !values.contains(value) {
                        values.push(value.clone());
                    }
                } else {
                    from.push((name.clone(), vec![value.clone()]));
                }
            }
        }
    }
    for (name, mut values) in from {
        let value = if values.len() == 1 {
            values.remove(0)
        } else {
            json!({"anyOf": values})
        };
        properties.insert(name, value);
    }
    if let Some(first) = branches
        .first()
        .and_then(|branch| branch.get("properties"))
        .and_then(Value::as_object)
    {
        for name in first.keys() {
            if branches.iter().all(|branch| {
                branch
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|required| required.iter().any(|item| item.as_str() == Some(name)))
            }) {
                required.push(Value::String(name.clone()));
            }
        }
    }
    for key in unions {
        object.remove(key);
    }
    object.insert("type".to_owned(), Value::String("object".to_owned()));
    required.retain(|name| {
        name.as_str()
            .is_some_and(|name| properties.contains_key(name))
    });
    let mut seen = HashSet::new();
    required.retain(|name| {
        name.as_str()
            .is_some_and(|name| seen.insert(name.to_owned()))
    });
    object.insert("properties".to_owned(), Value::Object(properties));
    if required.is_empty() {
        object.remove("required");
    } else {
        object.insert("required".to_owned(), Value::Array(required));
    }
}

fn has_union(schema: &Value) -> bool {
    ["allOf", "anyOf", "oneOf"]
        .iter()
        .any(|key| schema.get(*key).and_then(Value::as_array).is_some())
}

fn merge_properties(properties: &mut Map<String, Value>, branch: &Value) {
    if let Some(items) = branch.get("properties").and_then(Value::as_object) {
        for (name, value) in items {
            properties
                .entry(name.clone())
                .or_insert_with(|| value.clone());
        }
    }
}

fn normalize_tool_choice(body: &mut Map<String, Value>) {
    let Some(choice) = body.get("tool_choice").cloned() else {
        return;
    };
    match choice {
        Value::String(choice) => {
            if choice.trim().eq_ignore_ascii_case("none") {
                remove_tools(body);
            }
        }
        Value::Object(choice) => {
            let kind = choice
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            match kind.as_str() {
                "none" => remove_tools(body),
                "auto" | "required" => {
                    body.insert("tool_choice".to_owned(), Value::String(kind));
                }
                "function" => {
                    let name = choice
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    body.insert(
                        "tool_choice".to_owned(),
                        Value::String(if name.is_empty() {
                            "auto".to_owned()
                        } else {
                            name
                        }),
                    );
                }
                _ => {
                    body.remove("tool_choice");
                }
            }
        }
        _ => {
            body.remove("tool_choice");
        }
    }
}

fn remove_tools(body: &mut Map<String, Value>) {
    body.remove("tool_choice");
    body.remove("tools");
    body.remove("functions");
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::{Value, json};

    use super::{after_tools_refusal, prepare, restore};

    fn fitted(parameters: Value) -> Value {
        let mut body = json!({
            "model": "deepseek-v4.1-flash",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "t", "description": "d", "parameters": parameters}}],
        });
        let object = body.as_object_mut().expect("body");
        prepare(object, false);
        body["tools"][0]["function"]["parameters"].clone()
    }

    #[test]
    fn r55c_tool_patterns_and_tuples_fit() {
        let schema = fitted(json!({"type": "object", "properties": {
            "a": {"type": "string", "pattern": "^[^\\0]*$"},
            "b": {"type": "array", "items": [{"type": "number"}, {"type": "string"}], "additionalItems": false},
            "c": {"type": "string", "pattern": "\\\\0|\\01"},
        }}));
        assert_eq!(schema["properties"]["a"]["pattern"], "^[^\\u0000]*$");
        assert_eq!(schema["properties"]["c"]["pattern"], "\\\\0|\\01");
        assert_eq!(
            schema["properties"]["b"]["items"],
            json!({"anyOf": [{"type": "number"}, {"type": "string"}]})
        );
        assert!(schema["properties"]["b"].get("additionalItems").is_none());
    }

    #[test]
    fn r55c_tool_root_unions_and_refs_fold() {
        let schema = fitted(json!({
            "$defs": {"A": {"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]}},
            "allOf": [{"$ref": "#/$defs/A"}, {"type": "object", "properties": {"b": {"type": "string"}}}],
        }));
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["a"]));
        assert!(schema.get("allOf").is_none());
        assert_eq!(
            schema["properties"].as_object().expect("properties").len(),
            2
        );

        let schema = fitted(json!({"anyOf": [
            {"type": "object", "properties": {"kind": {"const": "a"}, "x": {"type": "string"}}, "required": ["kind", "x"]},
            {"type": "object", "properties": {"kind": {"const": "b"}, "y": {"type": "number"}}, "required": ["kind"]},
        ]}));
        assert_eq!(schema["required"], json!(["kind"]));
        assert_eq!(
            schema["properties"]["kind"],
            json!({"anyOf": [{"const": "a"}, {"const": "b"}]})
        );
        assert!(schema.get("anyOf").is_none());
        assert_eq!(
            fitted(json!({})),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            fitted(Value::Null),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn r55c_tool_enums_and_types_fit() {
        let property = |schema: Value| {
            fitted(json!({"type": "object", "properties": {"a": schema}}))["properties"]["a"]
                .clone()
        };
        assert_eq!(
            property(json!({"enum": ["x", "y"], "description": "d"})),
            json!({"enum": ["x", "y"], "description": "d", "type": "string"})
        );
        assert_eq!(
            property(json!({"enum": [1, 2.5]})),
            json!({"enum": [1, 2.5], "type": "number"})
        );
        assert_eq!(
            property(json!({"enum": [true, null]})),
            json!({"enum": [true, null], "type": ["boolean", "null"]})
        );
        assert_eq!(
            property(json!({"enum": [null]})),
            json!({"enum": [null], "type": "null"})
        );
        assert_eq!(
            property(json!({"enum": ["x", 1, null], "description": "d"})),
            json!({"description": "d", "anyOf": [
                {"type": "string", "enum": ["x"]}, {"type": "integer", "enum": [1]}, {"type": "null"},
            ]})
        );
        assert_eq!(
            property(json!({"type": "string", "enum": []})),
            json!({"type": "string"})
        );
        assert_eq!(
            property(json!({"type": "String"})),
            json!({"type": "string"})
        );
        assert_eq!(property(json!({"type": "any"})), json!({}));
        assert_eq!(
            property(json!({"type": ["Integer", "foo", "null"]})),
            json!({"type": ["integer", "null"]})
        );
        assert_eq!(
            property(json!({"type": "object", "required": "x"})),
            json!({"type": "object"})
        );
        assert_eq!(property(Value::Bool(true)), json!({}));
        assert_eq!(property(Value::Bool(false)), json!({"not": {}}));
        assert_eq!(
            property(json!({"type": "array", "items": true})),
            json!({"type": "array", "items": {}})
        );
    }

    #[test]
    fn r55c_tool_one_of_typing_is_conservative() {
        let branches = json!([
            {"type": "object", "properties": {"mode": {"type": "string", "const": "a"}}},
            {"type": "object", "properties": {"mode": {"type": "string", "const": "b"}}},
        ]);
        let schema = fitted(
            json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/X"}}, "$defs": {"X": {"oneOf": branches}}}),
        );
        assert_eq!(schema["$defs"]["X"]["type"], "object");
        assert_eq!(schema["properties"]["x"], json!({"$ref": "#/$defs/X"}));

        let unchanged = json!({"type": "object", "properties": {
            "mixed": {"oneOf": [{"type": "object"}, {"type": "string"}]},
            "any": {"anyOf": branches},
            "missing": {"oneOf": [{"type": "object"}, {"$ref": "#/$defs/Nope"}]},
            "loop": {"oneOf": [{"type": "object"}, {"$ref": "#/$defs/A"}]},
        }, "$defs": {"A": {"$ref": "#/$defs/B"}, "B": {"$ref": "#/$defs/A"}}});
        assert_eq!(
            fitted(unchanged.clone()),
            json!({"type": "object", "properties": unchanged["properties"].clone(), "$defs": unchanged["$defs"].clone()})
        );
    }

    #[test]
    fn r55c_tool_names_restore_in_answers() {
        let mut body = json!({
            "messages": [{"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "fs.read", "arguments": "{}"}}]}],
            "tools": [
                {"type": "function", "function": {"name": "fs.read", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "fs_read", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "a/b c:d", "parameters": {"type": "object"}}},
            ],
            "tool_choice": {"type": "function", "function": {"name": "fs.read"}},
        });
        let names = prepare(body.as_object_mut().expect("body"), false);
        assert_eq!(body["tools"][0]["function"]["name"], "fs_read_2");
        assert_eq!(body["tools"][1]["function"]["name"], "fs_read");
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["name"],
            "fs_read_2"
        );
        assert_eq!(body["tool_choice"], "fs_read_2");
        for tool in body["tools"].as_array().expect("tools") {
            let name = tool["function"]["name"].as_str().expect("name");
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            );
        }

        let mut unary = json!({"choices": [{"message": {"tool_calls": [{"function": {"name": "fs_read_2"}}]}}]});
        restore(&mut unary, &names);
        assert_eq!(
            unary["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "fs.read"
        );
        let mut stream = json!({"choices": [{"delta": {"tool_calls": [{"function": {"name": "fs_read_2", "arguments": ""}}]}}]});
        restore(&mut stream, &names);
        assert_eq!(
            stream["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "fs.read"
        );
    }

    #[test]
    fn r55c_tool_result_reply_is_narrow_and_refusal_is_exact() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "List."},
            {"role": "assistant", "content": null, "tool_calls": []},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            {"role": "user", "content": "Now?"},
            {"role": "user", "content": "Please."},
        ]});
        prepare(body.as_object_mut().expect("body"), true);
        assert_eq!(body["messages"].as_array().expect("messages").len(), 6);
        assert_eq!(
            body["messages"][3],
            json!({"role": "assistant", "content": ""})
        );
        assert_eq!(body["messages"][4]["role"], "user");

        for messages in [
            json!([{"role": "user", "content": "hi"}]),
            json!([{"role": "tool", "content": "ok"}]),
            json!([{"role": "tool", "content": "ok"}, {"role": "assistant", "content": "There are"}]),
            json!([{"role": "tool", "content": "ok"}, {"role": "assistant", "content": "ok"}, {"role": "user", "content": "hi"}]),
        ] {
            let mut body = json!({"messages": messages});
            let before = body.clone();
            prepare(body.as_object_mut().expect("body"), true);
            assert_eq!(body, before);
        }
        assert!(after_tools_refusal(r#"{"code":11153,"msg":"invalid"}"#));
        assert!(after_tools_refusal(
            "the conversation must end with a user message"
        ));
        assert!(!after_tools_refusal(r#"{"code":111530}"#));
        assert!(!after_tools_refusal(r#"{"code":11153.0}"#));
        assert!(!after_tools_refusal(r#"{"requestId":"11153"}"#));
    }
}
