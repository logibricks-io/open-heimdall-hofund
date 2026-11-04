//! Fit OpenAI Responses bodies to what Zed cloud accepts.

use hofund_plugin_sdk::json::{self, Map, Value};

/// Edits a Responses request map in place for Zed cloud.
pub fn transform_map(root: &mut Map<String, Value>) {
    if root.contains_key("messages") {
        return;
    }
    let Some(input_value) = root.get("input").cloned() else {
        return;
    };
    let Value::Array(input) = input_value else {
        return;
    };
    root.remove("input");
    let mut dirty = false;
    let mut kept = Vec::new();
    for item in input {
        let Some(mut map) = item.as_object().cloned() else {
            kept.push(item);
            continue;
        };
        let mut ty = map
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if ty.is_empty() && map.contains_key("role") {
            map.insert("type".to_owned(), Value::String("message".to_owned()));
            ty = "message".to_owned();
            dirty = true;
        }
        if flat_call(&mut map) {
            dirty = true;
        }
        match ty.as_str() {
            "message" => {
                if map.get("role") == Some(&Value::String("developer".to_owned())) {
                    map.insert("role".to_owned(), Value::String("system".to_owned()));
                    dirty = true;
                }
                if let Some(Value::String(text)) = map.get("content").cloned() {
                    let part = if map.get("role") == Some(&Value::String("assistant".to_owned())) {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    map.insert(
                        "content".to_owned(),
                        Value::Array(vec![json::json!({"type": part, "text": text})]),
                    );
                    dirty = true;
                }
            }
            "agent_message" => {
                map = Map::from_iter([
                    ("type".to_owned(), Value::String("message".to_owned())),
                    ("role".to_owned(), Value::String("user".to_owned())),
                    (
                        "content".to_owned(),
                        map.get("content").cloned().unwrap_or(Value::Null),
                    ),
                ]);
                dirty = true;
            }
            "reasoning" => {
                if map.get("content") == Some(&Value::Null) {
                    map.remove("content");
                    dirty = true;
                }
            }
            "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "compaction"
            | "compaction_trigger" => {}
            _ => {
                dirty = true;
                continue;
            }
        }
        kept.push(Value::Object(map));
    }
    root.insert("input".to_owned(), Value::Array(kept));
    filter_tools(root, &mut dirty);
    filter_tool_choice(root, &mut dirty);
    filter_include(root, &mut dirty);
    filter_reasoning(root, &mut dirty);
    let _ = dirty;
}

/// Returns `body` unchanged when it is not a Responses request, else a fitted copy.
#[must_use]
#[allow(dead_code)] // byte-level entry used by unit tests; upstream uses `transform_map`.
pub fn transform(body: &[u8]) -> Vec<u8> {
    if !body.windows(7).any(|window| window == b"\"input\"") {
        return body.to_vec();
    }
    let Ok(mut root) = json::from_slice::<Map<String, Value>>(body) else {
        return body.to_vec();
    };
    if root.contains_key("messages") {
        return body.to_vec();
    }
    transform_map(&mut root);
    json::to_vec(&Value::Object(root)).unwrap_or_else(|_| body.to_vec())
}

fn flat_call(map: &mut Map<String, Value>) -> bool {
    let Some(Value::String(space)) = map.remove("namespace") else {
        return false;
    };
    if let Some(Value::String(name)) = map.get("name").cloned()
        && !name.is_empty()
        && !space.is_empty()
    {
        map.insert("name".to_owned(), Value::String(format!("{space}__{name}")));
    }
    true
}

fn flatten_namespace(ns: &Map<String, Value>) -> Vec<Value> {
    let space = ns.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(Value::Array(nested)) = ns.get("tools") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tool in nested {
        let Some(mut map) = tool.as_object().cloned() else {
            continue;
        };
        if map.get("type").and_then(Value::as_str) != Some("function") {
            continue;
        }
        let Some(name) = map
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        if !space.is_empty() {
            map.insert("name".to_owned(), Value::String(format!("{space}__{name}")));
        }
        out.push(Value::Object(map));
    }
    out
}

fn filter_tools(root: &mut Map<String, Value>, dirty: &mut bool) {
    let Some(Value::Array(tools)) = root.get("tools").cloned() else {
        return;
    };
    let mut out = Vec::new();
    for tool in tools {
        let Some(map) = tool.as_object() else {
            *dirty = true;
            continue;
        };
        match map.get("type").and_then(Value::as_str) {
            Some("function") | Some("custom") => out.push(tool),
            Some("namespace") => {
                out.extend(flatten_namespace(map));
                *dirty = true;
            }
            _ => *dirty = true,
        }
    }
    if out.is_empty() {
        root.remove("tools");
        root.remove("tool_choice");
    } else {
        root.insert("tools".to_owned(), Value::Array(out));
    }
}

fn filter_tool_choice(root: &mut Map<String, Value>, dirty: &mut bool) {
    let Some(Value::Object(tc)) = root.get("tool_choice").cloned() else {
        return;
    };
    *dirty = true;
    let choice = if tc.get("type") == Some(&Value::String("function".to_owned()))
        || tc.get("type") == Some(&Value::String("custom".to_owned()))
    {
        Value::String("required".to_owned())
    } else {
        Value::String("auto".to_owned())
    };
    root.insert("tool_choice".to_owned(), choice);
}

fn filter_include(root: &mut Map<String, Value>, dirty: &mut bool) {
    let Some(Value::Array(items)) = root.get("include").cloned() else {
        return;
    };
    let out: Vec<_> = items
        .into_iter()
        .filter(|value| value == &Value::String("reasoning.encrypted_content".to_owned()))
        .collect();
    if out.is_empty() {
        root.remove("include");
    } else if out.len()
        != root
            .get("include")
            .and_then(Value::as_array)
            .map_or(0, |a| a.len())
    {
        root.insert("include".to_owned(), Value::Array(out));
    }
    *dirty = true;
}

fn filter_reasoning(root: &mut Map<String, Value>, dirty: &mut bool) {
    let Some(Value::Object(mut reasoning)) = root.get("reasoning").cloned() else {
        return;
    };
    if let Some(summary) = reasoning.get("summary")
        && summary != &Value::String("auto".to_owned())
        && summary != &Value::String("concise".to_owned())
        && summary != &Value::String("detailed".to_owned())
    {
        reasoning.remove("summary");
        *dirty = true;
        root.insert("reasoning".to_owned(), Value::Object(reasoning));
    }
}

#[cfg(test)]
mod tests {
    use super::transform;

    #[test]
    fn a2c_11_developer_becomes_system() {
        let body = br#"{"input":[{"type":"message","role":"developer","content":"hi"}]}"#;
        let out = transform(body);
        let text = std::str::from_utf8(&out).expect("utf8");
        assert!(text.contains("\"system\""));
        assert!(!text.contains("developer"));
    }
}
