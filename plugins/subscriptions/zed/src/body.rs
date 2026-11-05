//! Fit OpenAI Responses bodies to what Zed cloud accepts.

use hofund_plugin_api::ProtocolId;
use hofund_plugin_sdk::json::{self, Map, Value};

use crate::models;

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
    root.insert("stream".to_owned(), Value::Bool(true));
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

/// Wire shape Zed expects inside `provider_request`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wire {
    Anthropic,
    Responses,
    Chat,
    Gemini,
}

impl Wire {
    #[must_use]
    pub(crate) const fn outbound(self) -> ProtocolId {
        match self {
            Self::Anthropic => ProtocolId::ANTHROPIC_MESSAGES,
            Self::Responses => ProtocolId::OPENAI_RESPONSES,
            Self::Chat => ProtocolId::OPENAI_CHAT,
            Self::Gemini => ProtocolId::GENERATIVE_LANGUAGE,
        }
    }
}

#[must_use]
pub(crate) fn wire_for(inbound: ProtocolId) -> Option<Wire> {
    match inbound {
        ProtocolId::ANTHROPIC_MESSAGES => Some(Wire::Anthropic),
        ProtocolId::OPENAI_RESPONSES => Some(Wire::Responses),
        ProtocolId::OPENAI_CHAT => Some(Wire::Chat),
        ProtocolId::GENERATIVE_LANGUAGE => Some(Wire::Gemini),
        _ => None,
    }
}

/// Provider id from Zed's catalog (`anthropic`, `open_ai`, `x_ai`, `google`).
#[must_use]
pub(crate) fn wire_for_vendor(vendor: &str) -> Option<Wire> {
    match vendor {
        "anthropic" => Some(Wire::Anthropic),
        "open_ai" => Some(Wire::Responses),
        "x_ai" => Some(Wire::Chat),
        "google" => Some(Wire::Gemini),
        _ => None,
    }
}

#[must_use]
pub(crate) fn guess_vendor(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude") {
        return "anthropic";
    }
    if lower.starts_with("gemini") {
        return "google";
    }
    if lower.starts_with("grok") {
        return "x_ai";
    }
    "open_ai"
}

/// Shapes an encoded provider JSON object for Zed cloud.
pub(crate) fn prepare_provider(
    wire: Wire,
    root: &mut Map<String, Value>,
    model: &str,
    caps: models::Capabilities,
) {
    if !caps.supports_images {
        strip_images(root, wire);
    }
    if !caps.supports_thinking {
        strip_thinking(root, wire);
    }
    match wire {
        Wire::Responses => transform_map(root),
        Wire::Anthropic => prepare_anthropic(root),
        Wire::Chat => prepare_chat(root),
        Wire::Gemini => prepare_gemini(root, model),
    }
}

fn strip_thinking(root: &mut Map<String, Value>, wire: Wire) {
    match wire {
        Wire::Anthropic => {
            root.remove("thinking");
        }
        Wire::Responses => {
            root.remove("reasoning");
        }
        Wire::Chat => {
            root.remove("reasoning_effort");
        }
        Wire::Gemini => {
            if let Some(gc) = root
                .get_mut("generationConfig")
                .and_then(Value::as_object_mut)
            {
                gc.remove("thinkingConfig");
            }
        }
    }
}

fn strip_images(root: &mut Map<String, Value>, wire: Wire) {
    match wire {
        Wire::Anthropic => {
            if let Some(Value::Array(messages)) = root.get_mut("messages") {
                for message in messages {
                    strip_message_images(message);
                }
            }
        }
        Wire::Responses => {
            if let Some(Value::Array(input)) = root.get_mut("input") {
                for item in input {
                    strip_message_images(item);
                }
            }
        }
        Wire::Chat => {
            if let Some(Value::Array(messages)) = root.get_mut("messages") {
                for message in messages {
                    strip_message_images(message);
                }
            }
        }
        Wire::Gemini => {
            if let Some(Value::Array(contents)) = root.get_mut("contents") {
                for turn in contents {
                    if let Some(Value::Array(parts)) = turn.get_mut("parts") {
                        parts.retain(|part| {
                            part.get("inlineData").is_none() && part.get("inline_data").is_none()
                        });
                    }
                }
            }
        }
    }
}

fn strip_message_images(value: &mut Value) {
    let Some(content) = value.get_mut("content") else {
        return;
    };
    let Value::Array(parts) = content else {
        return;
    };
    parts.retain(|part| {
        part.get("type").and_then(Value::as_str) != Some("image")
            && part.get("type").and_then(Value::as_str) != Some("image_url")
            && part.get("type").and_then(Value::as_str) != Some("input_image")
    });
}

fn prepare_anthropic(root: &mut Map<String, Value>) {
    root.remove("stream");
    if let Some(Value::Object(cm)) = root.get_mut("context_management") {
        if let Some(Value::Array(edits)) = cm.get_mut("edits") {
            edits.retain(|edit| {
                edit.get("type").and_then(Value::as_str) == Some("compact_20260112")
            });
            if edits.is_empty() {
                root.remove("context_management");
            }
        } else {
            root.remove("context_management");
        }
    }
    if let Some(Value::Array(messages)) = root.get_mut("messages") {
        let folded = without_system_turns(std::mem::take(messages));
        *messages = folded;
        for message in messages.iter_mut() {
            let Some(map) = message.as_object_mut() else {
                continue;
            };
            if let Some(Value::String(text)) = map.get("content").cloned() {
                map.insert(
                    "content".to_owned(),
                    Value::Array(vec![json::json!({"type":"text","text": text})]),
                );
            }
            if let Some(Value::Array(blocks)) = map.get_mut("content") {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result")
                        && block.get("is_error").is_none()
                    {
                        block
                            .as_object_mut()
                            .expect("object")
                            .insert("is_error".to_owned(), Value::Bool(false));
                    }
                }
            }
        }
    }
    if let Some(Value::Object(th)) = root.get_mut("thinking")
        && let Some(display) = th.get("display").and_then(Value::as_str)
        && display != "omitted"
        && display != "summarized"
    {
        if display == "highlights" {
            th.insert("display".to_owned(), Value::String("summarized".to_owned()));
        } else {
            th.remove("display");
        }
    }
}

fn prepare_chat(root: &mut Map<String, Value>) {
    root.insert("stream".to_owned(), Value::Bool(true));
    if root.contains_key("max_tokens")
        && let Some(max) = root.remove("max_tokens")
    {
        root.insert("max_completion_tokens".to_owned(), max);
    }
}

fn prepare_gemini(root: &mut Map<String, Value>, model: &str) {
    root.remove("session_id");
    root.remove("sessionId");
    root.insert("model".to_owned(), Value::String(format!("models/{model}")));
    const MODES: &[(&str, &str)] = &[
        ("AUTO", "auto"),
        ("ANY", "any"),
        ("NONE", "none"),
        ("VALIDATED", "auto"),
        ("MODE_UNSPECIFIED", "auto"),
    ];
    if let Some(tc) = root
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
        .and_then(|gc| gc.get_mut("thinkingConfig"))
        .and_then(Value::as_object_mut)
        && let Some(level) = tc.get("thinkingLevel").and_then(Value::as_str)
    {
        if level.is_empty() {
            tc.remove("thinkingLevel");
        } else {
            tc.insert(
                "thinkingLevel".to_owned(),
                Value::String(level.to_ascii_uppercase()),
            );
        }
    }
    if let Some(mode) = root
        .get("toolConfig")
        .and_then(|tc| tc.get("functionCallingConfig"))
        .and_then(|fc| fc.get("mode"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        let upper = mode.to_ascii_uppercase();
        let mapped = MODES
            .iter()
            .find(|(from, _)| upper == *from)
            .map(|(_, to)| (*to).to_owned())
            .unwrap_or_else(|| mode.to_ascii_lowercase());
        root.get_mut("toolConfig")
            .and_then(Value::as_object_mut)
            .and_then(|tc| tc.get_mut("functionCallingConfig"))
            .and_then(Value::as_object_mut)
            .expect("fc")
            .insert("mode".to_owned(), Value::String(mapped));
    }
}

fn without_system_turns(messages: Vec<Value>) -> Vec<Value> {
    if !messages
        .iter()
        .any(|m| m.get("role").and_then(Value::as_str) == Some("system"))
    {
        return messages;
    }
    let mut out: Vec<Value> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            let blocks = system_blocks(message.get("content"));
            if blocks.is_empty() {
                continue;
            }
            if let Some(prev) = out.last_mut()
                && prev.get("role").and_then(Value::as_str) == Some("user")
                && pending.is_empty()
            {
                append_blocks(prev, &blocks);
                continue;
            }
            pending.extend(blocks);
            continue;
        }
        if !pending.is_empty() {
            if message.get("role").and_then(Value::as_str) == Some("user") {
                let mut merged = message.clone();
                insert_pending_after_tool_results(&mut merged, &pending);
                pending.clear();
                out.push(merged);
                continue;
            }
            out.push(json::json!({"role":"user","content": pending}));
            pending.clear();
        }
        out.push(message);
    }
    if !pending.is_empty() {
        if let Some(prev) = out.last_mut()
            && prev.get("role").and_then(Value::as_str) == Some("user")
        {
            append_blocks(prev, &pending);
        } else {
            out.push(json::json!({"role":"user","content": pending}));
        }
    }
    out
}

fn system_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json::json!({"type":"text","text": text})]
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter(|b| {
                !(b.get("type").and_then(Value::as_str) == Some("text")
                    && b.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(str::is_empty))
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

fn append_blocks(message: &mut Value, blocks: &[Value]) {
    let Some(map) = message.as_object_mut() else {
        return;
    };
    let mut content = map.get("content").cloned().unwrap_or(Value::Array(vec![]));
    match &mut content {
        Value::Array(items) => items.extend(blocks.iter().cloned()),
        Value::String(text) if !text.is_empty() => {
            let mut items = vec![hofund_plugin_sdk::json::json!({"type":"text","text": text})];
            items.extend(blocks.iter().cloned());
            content = Value::Array(items);
        }
        _ => content = Value::Array(blocks.to_vec()),
    }
    map.insert("content".to_owned(), content);
}

fn insert_pending_after_tool_results(message: &mut Value, pending: &[Value]) {
    let Some(map) = message.as_object_mut() else {
        return;
    };
    let mut content = map.get("content").cloned().unwrap_or(Value::Array(vec![]));
    if !matches!(&content, Value::Array(_)) {
        content = Value::Array(system_blocks(map.get("content")));
    }
    let Value::Array(ref mut items) = content else {
        map.insert("content".to_owned(), Value::Array(pending.to_vec()));
        return;
    };
    let mut index = 0;
    while index < items.len()
        && items[index].get("type").and_then(Value::as_str) == Some("tool_result")
    {
        index += 1;
    }
    items.splice(index..index, pending.iter().cloned());
    map.insert("content".to_owned(), content);
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
