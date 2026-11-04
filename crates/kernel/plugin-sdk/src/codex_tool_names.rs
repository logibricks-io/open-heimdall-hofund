//! Tool names the `ChatGPT` Codex backend accepts.
//!
//! The backend checks every tool name against `^[a-zA-Z0-9_.-]+$`. A name
//! outside that pattern is rewritten the same on every request; two distinct
//! bad names never collide.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::digest;

static NAME_OK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_.-]+$").expect("tool name pattern"));

const PREFIX_MAX: usize = 55;

/// A name as the `ChatGPT` backend takes it.
#[must_use]
pub fn codex_tool_name(name: &str) -> String {
    if NAME_OK.is_match(name) {
        return name.to_owned();
    }
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 9);
    for byte in bytes {
        if matches!(
            *byte,
            b'_' | b'.' | b'-'
                | b'0'..=b'9'
                | b'a'..=b'z'
                | b'A'..=b'Z'
        ) {
            out.push(*byte);
        } else {
            out.push(b'_');
        }
    }
    if out.len() > PREFIX_MAX {
        out.truncate(PREFIX_MAX);
    }
    let sum = digest::sha256(bytes);
    format!(
        "{}_{}",
        String::from_utf8_lossy(&out),
        hex::encode(&sum[..4])
    )
}

/// Maps each rewritten declared name back to the agent's original; empty when
/// nothing was renamed.
#[must_use]
pub fn codex_tool_names_back(declared: &[String]) -> BTreeMap<String, String> {
    let mut back = BTreeMap::new();
    for name in declared {
        let fixed = codex_tool_name(name);
        if fixed != *name {
            back.insert(fixed, name.clone());
        }
    }
    back
}

/// Whether `body` names a tool the backend refuses, without full decode.
#[must_use]
pub fn codex_names_bad(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    codex_names_bad_object(object)
}

fn codex_names_bad_object(object: &Map<String, Value>) -> bool {
    let mut bad = false;
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        bad |= tools_name_bad(tools);
    }
    if let Some(input) = object.get("input").and_then(Value::as_array) {
        for item in input {
            let Some(item) = item.as_object() else {
                continue;
            };
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "function_call" | "custom_tool_call" => {
                    if name_bad(item.get("name")) {
                        bad = true;
                    }
                }
                "additional_tools" | "tool_search_output" => {
                    if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                        bad |= tools_name_bad(tools);
                    }
                }
                _ => {}
            }
            if bad {
                return true;
            }
        }
    }
    if let Some(choice) = object.get("tool_choice").and_then(Value::as_object) {
        bad |= tool_choice_name_bad(choice);
    }
    bad
}

fn tools_name_bad(tools: &[Value]) -> bool {
    for tool in tools {
        let Some(tool) = tool.as_object() else {
            continue;
        };
        match tool.get("type").and_then(Value::as_str).unwrap_or_default() {
            "function" | "custom" => {
                if name_bad(tool.get("name")) {
                    return true;
                }
            }
            "namespace" => {
                if let Some(nested) = tool.get("tools").and_then(Value::as_array)
                    && tools_name_bad(nested)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn tool_choice_name_bad(choice: &Map<String, Value>) -> bool {
    match choice
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "function" | "custom" => name_bad(choice.get("name")),
        "allowed_tools" => choice
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools_name_bad(tools)),
        _ => false,
    }
}

fn name_bad(name: Option<&Value>) -> bool {
    name.and_then(Value::as_str)
        .is_some_and(|name| !NAME_OK.is_match(name))
}

/// Rewrites every tool name in `object` when needed. Returns whether anything
/// changed.
pub fn codex_tool_names_object(object: &mut Map<String, Value>) -> bool {
    let mut changed = false;
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        changed |= fix_tools(tools);
    }
    if let Some(input) = object.get_mut("input").and_then(Value::as_array_mut) {
        for item in input.iter_mut() {
            let Some(item) = item.as_object_mut() else {
                continue;
            };
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "function_call" | "custom_tool_call" => changed |= fix_name(item),
                "additional_tools" | "tool_search_output" => {
                    if let Some(tools) = item.get_mut("tools").and_then(Value::as_array_mut) {
                        changed |= fix_tools(tools);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(choice) = object.get_mut("tool_choice").and_then(Value::as_object_mut) {
        changed |= fix_tool_choice(choice);
    }
    changed
}

fn fix_tools(tools: &mut [Value]) -> bool {
    let mut changed = false;
    for tool in tools.iter_mut() {
        let Some(tool) = tool.as_object_mut() else {
            continue;
        };
        match tool.get("type").and_then(Value::as_str).unwrap_or_default() {
            "function" | "custom" => changed |= fix_name(tool),
            "namespace" => {
                if let Some(nested) = tool.get_mut("tools").and_then(Value::as_array_mut) {
                    changed |= fix_tools(nested);
                }
            }
            _ => {}
        }
    }
    changed
}

fn fix_tool_choice(choice: &mut Map<String, Value>) -> bool {
    match choice
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "function" | "custom" => fix_name(choice),
        "allowed_tools" => match choice.get_mut("tools").and_then(Value::as_array_mut) {
            Some(tools) => fix_tools(tools),
            None => false,
        },
        _ => false,
    }
}

fn fix_name(object: &mut Map<String, Value>) -> bool {
    let Some(Value::String(name)) = object.get("name") else {
        return false;
    };
    let fixed = codex_tool_name(name);
    if fixed == *name {
        return false;
    }
    object.insert("name".to_owned(), Value::String(fixed));
    true
}

/// Declared tool names in a Responses body: top-level `tools` and
/// `additional_tools` items in `input`.
#[must_use]
pub fn declared_tool_names(body: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if let Some(tools) = object.get("tools").and_then(Value::as_array) {
        collect_tool_names(tools, &mut names);
    }
    if let Some(input) = object.get("input").and_then(Value::as_array) {
        for item in input {
            let Some(item) = item.as_object() else {
                continue;
            };
            if item.get("type").and_then(Value::as_str) == Some("additional_tools")
                && let Some(tools) = item.get("tools").and_then(Value::as_array)
            {
                collect_tool_names(tools, &mut names);
            }
        }
    }
    names
}

fn collect_tool_names(tools: &[Value], names: &mut Vec<String>) {
    for tool in tools {
        let Some(tool) = tool.as_object() else {
            continue;
        };
        match tool.get("type").and_then(Value::as_str).unwrap_or_default() {
            "function" | "custom" => {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    names.push(name.to_owned());
                }
            }
            "namespace" => {
                if let Some(nested) = tool.get("tools").and_then(Value::as_array) {
                    collect_tool_names(nested, names);
                }
            }
            _ => {}
        }
    }
}

/// `body` with every tool name rewritten, or unchanged when each is valid.
#[must_use]
pub fn codex_tool_names_body(body: &[u8]) -> Vec<u8> {
    if !codex_names_bad(body) {
        return body.to_vec();
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(object) = value.as_object_mut() else {
        return body.to_vec();
    };
    if !codex_tool_names_object(object) {
        return body.to_vec();
    }
    serde_json::to_vec(object).unwrap_or_else(|_| body.to_vec())
}

/// Restores original tool names in `value` using `back`.
pub fn restore_tool_names(value: &mut Value, back: &BTreeMap<String, String>) {
    if back.is_empty() {
        return;
    }
    match value {
        Value::Object(object) => restore_object(object, back),
        Value::Array(items) => {
            for item in items {
                restore_tool_names(item, back);
            }
        }
        _ => {}
    }
}

fn restore_object(object: &mut Map<String, Value>, back: &BTreeMap<String, String>) {
    if let Some(Value::String(name)) = object.get_mut("name")
        && let Some(original) = back.get(name.as_str())
    {
        *name = original.clone();
    }
    for (_, value) in object.iter_mut() {
        restore_tool_names(value, back);
    }
}

mod hex {
    pub(super) fn encode(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(out, "{byte:02x}").expect("hex digit");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{
        codex_tool_name, codex_tool_names_back, codex_tool_names_body, declared_tool_names,
        restore_tool_names,
    };
    use serde_json::json;

    #[test]
    fn d57_1_rewrite_shape() {
        let name = "search:web";
        let fixed = codex_tool_name(name);
        assert!(fixed.starts_with("search_web_"));
        assert_eq!(fixed.len(), "search_web_".len() + 8);
        assert!(
            fixed
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-')
        );
    }

    #[test]
    fn d57_2_collision_free() {
        let a = codex_tool_name("bad name");
        let b = codex_tool_name("bad:name");
        assert_ne!(a, b);
    }

    #[test]
    fn d57_3_round_trip_in_reply() {
        let declared = vec!["search:web".to_owned()];
        let back = codex_tool_names_back(&declared);
        let body = json!({
            "model": "m",
            "tools": [{"type": "function", "name": "search:web", "parameters": {"type": "object"}}],
            "input": []
        });
        let sent = codex_tool_names_body(&serde_json::to_vec(&body).unwrap());
        let fixed_name = declared_tool_names(&sent)[0].clone();
        assert_ne!(fixed_name, "search:web");
        let mut reply = json!({
            "output": [{"type": "function_call", "name": fixed_name, "call_id": "c1"}]
        });
        restore_tool_names(&mut reply, &back);
        assert_eq!(reply["output"][0]["name"].as_str(), Some("search:web"));
    }

    #[test]
    fn d57_4_valid_names_unchanged() {
        let name = "my_tool.v2";
        assert_eq!(codex_tool_name(name), name);
        let body = json!({
            "model": "m",
            "tools": [{"type": "function", "name": name, "parameters": {"type": "object"}}],
            "input": [{"type": "function_call", "name": name, "call_id": "c1"}]
        });
        let raw = serde_json::to_vec(&body).unwrap();
        assert_eq!(codex_tool_names_body(&raw), raw);
    }

    #[test]
    fn d57_9_long_bad_name_is_cut_to_the_prefix_cap() {
        let long = format!("{}:x", "a".repeat(80));
        let fixed = super::codex_tool_name(&long);
        assert_eq!(fixed.len(), 55 + 1 + 8);
        assert!(fixed.starts_with(&"a".repeat(55)));
    }
}
