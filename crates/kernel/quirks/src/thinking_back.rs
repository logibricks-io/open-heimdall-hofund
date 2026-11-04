//! Models that demand earlier reasoning back on tool turns.

use serde_json::Value;

/// DeepSeek's exact demand for earlier thinking or reasoning on tool turns.
#[must_use]
pub fn thinking_back_refusal(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("must be passed back")
        && (lower.contains("content[].thinking") || lower.contains("reasoning_content"))
}

/// Other vendors naming `reasoning_content` as missing or required.
#[must_use]
pub fn reasoning_back_refused(message: &str) -> bool {
    if thinking_back_refusal(message) {
        return true;
    }
    let lower = message.to_ascii_lowercase();
    if !lower.contains("reasoning_content") {
        return false;
    }
    if lower.contains("missing required parameter")
        && (lower.contains("'messages'") || lower.contains("\"messages\""))
    {
        return false;
    }
    reasoning_back_words(&lower)
}

fn reasoning_back_words(lower: &str) -> bool {
    if let Some(at) = lower.find("reasoning_content") {
        let window = &lower[at.saturating_sub(0)..lower.len().min(at + 80)];
        if window.contains("is missing")
            || window.contains("is required")
            || window.contains("must be passed")
            || window.contains("must be provided")
            || window.contains("must be included")
        {
            return true;
        }
    }
    if lower.contains("missing") && lower.contains("reasoning_content") {
        return true;
    }
    if lower.contains("required") && lower.contains("reasoning_content") {
        return true;
    }
    false
}

fn contains_substring(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Fills tool-turn thinking for Messages bodies.
#[must_use]
pub fn fill_messages_thinking(body: &[u8]) -> Vec<u8> {
    if !contains_substring(body, b"tool_use") && !contains_substring(body, b"redacted_thinking") {
        return body.to_vec();
    }
    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) else {
        return body.to_vec();
    };
    let space = serde_json::json!({"type":"thinking","thinking":" "});
    let mut changed = false;
    let mut after_tool = false;
    for message in messages.iter_mut() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "assistant" {
            after_tool = message_after_tool(message);
            continue;
        }
        let after = after_tool;
        after_tool = false;
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut thought = false;
        let mut called = false;
        let mut edited = false;
        let mut kept = Vec::new();
        for block in content.drain(..) {
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "thinking" => thought = true,
                "tool_use" => called = true,
                "redacted_thinking" if !thought => {
                    edited = true;
                    thought = true;
                    kept.push(space.clone());
                }
                "redacted_thinking" => edited = true,
                _ => kept.push(block),
            }
        }
        if (called || after) && !thought {
            kept.insert(0, space.clone());
            edited = true;
        }
        if edited {
            message
                .as_object_mut()
                .expect("object")
                .insert("content".to_owned(), Value::Array(kept));
            changed = true;
        }
    }
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&root).unwrap_or_else(|_| body.to_vec())
}

fn message_after_tool(message: &Value) -> bool {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return false;
    };
    !content.is_empty()
        && content
            .iter()
            .all(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
}

/// Fills tool-turn `reasoning_content` for Chat bodies.
#[must_use]
pub fn fill_chat_reasoning(body: &[u8]) -> Vec<u8> {
    if !contains_substring(body, b"tool_calls") {
        return body.to_vec();
    }
    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) else {
        return body.to_vec();
    };
    let mut changed = false;
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        let has_tool = object.get("tool_calls").is_some();
        let has_reasoning = object.contains_key("reasoning_content");
        if has_tool && !has_reasoning {
            object.insert(
                "reasoning_content".to_owned(),
                Value::String(" ".to_owned()),
            );
            changed = true;
        }
    }
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&root).unwrap_or_else(|_| body.to_vec())
}

/// Fills tool-turn thinking when a provider's model asked for it before.
#[must_use]
pub fn fill_thinking_back(protocol: &str, body: &[u8]) -> Vec<u8> {
    match protocol {
        "anthropic-messages" => fill_messages_thinking(body),
        "openai-chat" => fill_chat_reasoning(body),
        _ => body.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fill_messages_thinking, fill_thinking_back, reasoning_back_refused, thinking_back_refusal,
    };

    #[test]
    fn d65_1_thinking_back_refusal_matches_deepseek_wording() {
        let msg = "The `content[].thinking` in the thinking mode must be passed back to the API";
        assert!(thinking_back_refusal(msg));
        assert!(reasoning_back_refused(msg));
    }

    #[test]
    fn d65_2_missing_reasoning_content_is_shape_failover() {
        let msg = r#"Missing required parameter: 'messages[1].reasoning_content'"#;
        assert!(reasoning_back_refused(msg));
    }

    #[test]
    fn d65_3_missing_messages_is_not_reasoning_back() {
        let msg = "Missing required parameter: 'messages'";
        assert!(!reasoning_back_refused(msg));
    }

    #[test]
    fn d65_4_fill_messages_adds_thinking_on_tool_turns() {
        let body = br#"{"model":"demo","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"n","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]},{"role":"assistant","content":[{"type":"text","text":"done"}]}]}"#;
        let filled = fill_messages_thinking(body);
        let value: serde_json::Value = serde_json::from_slice(&filled).expect("json");
        let last = &value["messages"][3]["content"];
        assert_eq!(last[0]["type"], "thinking");
    }

    #[test]
    fn d65_5_fill_chat_adds_reasoning_content() {
        let body = br#"{"model":"demo","messages":[{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"n","arguments":"{}"}}]}]}"#;
        let filled = fill_thinking_back("openai-chat", body);
        let value: serde_json::Value = serde_json::from_slice(&filled).expect("json");
        assert_eq!(value["messages"][0]["reasoning_content"], " ");
    }

    #[test]
    fn d65_6_fill_thinking_back_routes_by_protocol() {
        let chat = br#"{"model":"demo","messages":[{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"n","arguments":"{}"}}]}]}"#;
        assert_ne!(
            fill_thinking_back("openai-chat", chat),
            fill_thinking_back("anthropic-messages", chat)
        );
    }

    #[test]
    fn d65_10_passed_back_needs_a_named_field() {
        assert!(!thinking_back_refusal("the token must be passed back"));
        assert!(!thinking_back_refusal("reasoning_content is unsupported"));
    }
}
