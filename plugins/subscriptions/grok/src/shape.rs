//! Reshapes OpenAI Responses bodies the CLI backend accepts.

use hofund_plugin_sdk::json::{Map, Value};

const OUTPUT_REPLAY: &str = "output.encrypted_content";

const TOOLS: [&str; 11] = [
    "function",
    "web_search",
    "x_search",
    "image_generation",
    "collections_search",
    "file_search",
    "code_execution",
    "code_interpreter",
    "mcp",
    "shell",
    "tool_search",
];

/// Rewrites `body` for the CLI backend. When `x_search` is true, an automatic
/// tool choice may gain an `x_search` tool.
pub(crate) fn body(body: &mut Map<String, Value>, x_search: bool) {
    let auto_choice = body
        .get("tool_choice")
        .is_none_or(|value| value.is_null() || value.as_str() == Some("auto"));
    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let valid_tools = body
        .get("tools")
        .is_none_or(|tools| tools.is_null() || tools.is_array());
    let search = x_search
        && auto_choice
        && valid_tools
        && (body.get("tool_choice").and_then(Value::as_str) == Some("auto") || has_tools);
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        let incoming = std::mem::take(tools);
        for mut tool in incoming {
            let Some(object) = tool.as_object_mut() else {
                tools.push(tool);
                continue;
            };
            let ty = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if !TOOLS.contains(&ty.as_str()) {
                continue;
            }
            object.remove("external_web_access");
            if ty == "function" {
                let mut parameters = object
                    .get_mut("function")
                    .and_then(Value::as_object_mut)
                    .and_then(|function| function.get_mut("parameters"));
                if parameters.is_none() {
                    parameters = object.get_mut("parameters");
                }
                if let Some(parameters) = parameters {
                    fold_object_root(parameters);
                }
            }
            tools.push(tool);
        }
        if let Some(Value::Object(choice)) = body.get("tool_choice") {
            let ty = choice
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !TOOLS.contains(&ty) {
                body.remove("tool_choice");
            }
        }
    }
    if search
        && (body.get("tool_choice") == Some(&Value::String("auto".to_owned()))
            || body
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| !tools.is_empty()))
        && body
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(|tools| !tools.iter().any(|tool| tool_matches_type(tool, "x_search")))
    {
        if !body.get("tools").is_some_and(Value::is_array) {
            body.insert("tools".to_owned(), Value::Array(Vec::new()));
        }
        body.get_mut("tools")
            .and_then(Value::as_array_mut)
            .expect("tools array")
            .push(Value::Object(Map::from_iter([(
                "type".to_owned(),
                Value::String("x_search".to_owned()),
            )])));
    }
    if body.contains_key("tool_choice")
        && body
            .get("tools")
            .and_then(Value::as_array)
            .is_none_or(|tools| tools.is_empty())
    {
        body.remove("tool_choice");
    }
    if let Some(Value::Array(input)) = body.get_mut("input") {
        for item in input.iter_mut() {
            if let Some(message) = agent_message(item) {
                *item = message;
            }
            if let Some(object) = item.as_object_mut()
                && object.get("type").and_then(Value::as_str) == Some("reasoning")
                && object.get("content") == Some(&Value::Null)
            {
                object.remove("content");
            }
        }
    }
    if let Some(Value::Array(include)) = body.get_mut("include") {
        include.retain(|entry| entry.as_str() != Some(OUTPUT_REPLAY));
        if include.is_empty() {
            body.remove("include");
        }
    }
}

fn tool_matches_type(tool: &Value, ty: &str) -> bool {
    tool.as_object()
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str)
        == Some(ty)
}

fn agent_message(item: &Value) -> Option<Value> {
    let object = item.as_object()?;
    if object.get("type").and_then(Value::as_str)? != "agent_message" {
        return None;
    }
    if object.contains_key("encrypted_content") {
        return None;
    }
    let parts = object.get("content")?.as_array()?;
    if parts.is_empty() {
        return None;
    }
    for part in parts {
        let part = part.as_object()?;
        if part.contains_key("encrypted_content") {
            return None;
        }
        let is_text = part.get("type").and_then(Value::as_str) == Some("input_text")
            && part.get("text").and_then(Value::as_str).is_some();
        let is_image = part.get("type").and_then(Value::as_str) == Some("input_image")
            && part.get("image_url").and_then(Value::as_str).is_some();
        if !(is_text || is_image) {
            return None;
        }
    }
    let from = object.get("author").and_then(Value::as_str).unwrap_or("");
    let to = object
        .get("recipient")
        .and_then(Value::as_str)
        .unwrap_or("");
    let header = match (from.is_empty(), to.is_empty()) {
        (false, false) => format!("From {from} to {to}"),
        (false, true) => format!("From {from}"),
        (true, false) => format!("To {to}"),
        (true, true) => String::new(),
    };
    let mut content = parts.clone();
    if !header.is_empty() {
        content.insert(
            0,
            Value::Object(Map::from_iter([
                ("type".to_owned(), Value::String("input_text".to_owned())),
                ("text".to_owned(), Value::String(format!("{header}\n\n"))),
            ])),
        );
    }
    Some(Value::Object(Map::from_iter([
        ("type".to_owned(), Value::String("message".to_owned())),
        ("role".to_owned(), Value::String("user".to_owned())),
        ("content".to_owned(), Value::Array(content)),
    ])))
}

/// Splits a requested model id into the API model and optional context window.
pub(crate) fn model_of(id: &str) -> (String, u64) {
    let Some(at) = id.rfind('@') else {
        return (id.to_owned(), 0);
    };
    let base = &id[..at];
    let suffix = &id[at + 1..];
    let (digits, scale) = if let Some(stem) = suffix.strip_suffix('k') {
        (stem, 1000_u64)
    } else {
        (suffix, 1_u64)
    };
    let Ok(window) = digits.parse::<u64>() else {
        return (id.to_owned(), 0);
    };
    if window == 0 {
        return (id.to_owned(), 0);
    }
    (base.to_owned(), window * scale)
}

fn fold_object_root(schema: &mut Value) {
    let root = schema.clone();
    fold(&root, schema, 0);
}

fn fold(root: &Value, schema: &mut Value, depth: u32) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    if object.get("type").and_then(Value::as_str) == Some("object")
        && !["anyOf", "oneOf", "allOf"]
            .iter()
            .any(|key| object.contains_key(*key))
    {
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
    if let Some(members) = object.get("allOf").and_then(Value::as_array) {
        for member in members {
            let Some(branch) = resolve(root, member) else {
                continue;
            };
            if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
                for (name, schema) in branch_properties {
                    properties
                        .entry(name.clone())
                        .or_insert_with(|| schema.clone());
                }
            }
            if let Some(branch_required) = branch.get("required").and_then(Value::as_array) {
                required.extend(branch_required.clone());
            }
        }
    }
    object.remove("allOf");
    let mut branches = Vec::new();
    for key in ["anyOf", "oneOf"] {
        if let Some(members) = object.get(key).and_then(Value::as_array) {
            for member in members {
                let Some(mut branch) = resolve(root, member) else {
                    continue;
                };
                if nested_union(&branch) && depth < 8 {
                    let mut folded = Value::Object(branch.clone());
                    fold(root, &mut folded, depth + 1);
                    if let Value::Object(folded) = folded {
                        branch = folded;
                    }
                }
                if branch.get("type").and_then(Value::as_str) != Some("object")
                    && !branch.contains_key("properties")
                {
                    continue;
                }
                branches.push(branch);
            }
        }
        object.remove(key);
    }
    let mut common = Vec::new();
    let mut from: std::collections::BTreeMap<String, Vec<Value>> =
        std::collections::BTreeMap::new();
    for (index, branch) in branches.iter().enumerate() {
        if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
            for (name, schema) in branch_properties {
                if properties.contains_key(name) {
                    continue;
                }
                let values = from.entry(name.clone()).or_default();
                if !values.iter().any(|existing| existing == schema) {
                    values.push(schema.clone());
                }
            }
        }
        let branch_required = branch
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if index == 0 {
            common = branch_required;
        } else {
            common.retain(|name| branch_required.contains(name));
        }
    }
    for (name, values) in from {
        properties.insert(
            name,
            if values.len() == 1 {
                values.into_iter().next().unwrap_or_default()
            } else {
                Value::Array(values)
            },
        );
    }
    required.extend(common);
    required.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
    required.dedup_by(|left, right| left == right);
    object.insert("type".to_owned(), Value::String("object".to_owned()));
    object.insert("properties".to_owned(), Value::Object(properties));
    if required.is_empty() {
        object.remove("required");
    } else {
        object.insert("required".to_owned(), Value::Array(required));
    }
}

fn nested_union(branch: &Map<String, Value>) -> bool {
    ["anyOf", "oneOf", "allOf"]
        .iter()
        .any(|key| branch.contains_key(*key))
}

fn resolve(root: &Value, branch: &Value) -> Option<Map<String, Value>> {
    let object = branch.as_object()?;
    let Some(reference) = object.get("$ref").and_then(Value::as_str) else {
        return Some(object.clone());
    };
    for key in ["$defs", "definitions"] {
        let prefix = format!("#/{key}/");
        if let Some(name) = reference.strip_prefix(&prefix) {
            return root.get(key)?.get(name)?.as_object().cloned();
        }
    }
    None
}
