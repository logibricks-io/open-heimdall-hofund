//! Drop foreign thinking signatures before Claude upstreams see them.

use serde_json::Value;

/// Removes thinking blocks whose signature is a UUID (DeepSeek's Anthropic endpoint).
#[must_use]
pub fn without_foreign_thinking(base_url: &str, protocol: &str, body: &[u8]) -> Vec<u8> {
    if protocol != "anthropic-messages"
        || !body.windows(11).any(|window| window == b"\"signature\"")
    {
        return body.to_vec();
    }
    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let Some(model) = root.get("model").and_then(Value::as_str) else {
        return body.to_vec();
    };
    if !serves_claude(base_url, model) {
        return body.to_vec();
    }
    let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) else {
        return body.to_vec();
    };
    let mut changed = false;
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = content.len();
        content.retain(|block| !drops_foreign_thinking(block));
        if content.is_empty() {
            *message = Value::Null;
            changed = true;
        } else if content.len() != before {
            changed = true;
        }
    }
    messages.retain(|message| !message.is_null());
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&root).unwrap_or_else(|_| body.to_vec())
}

fn serves_claude(base_url: &str, model: &str) -> bool {
    let host = host_of(base_url);
    host == "api.anthropic.com" || model.to_ascii_lowercase().contains("claude-")
}

fn host_of(base_url: &str) -> String {
    base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn drops_foreign_thinking(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("thinking")
        && block
            .get("signature")
            .and_then(Value::as_str)
            .is_some_and(deepseek_uuid_signature)
}

fn deepseek_uuid_signature(signature: &str) -> bool {
    let bytes = signature.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        match index {
            8 | 13 | 18 | 23 => {
                if *byte != b'-' {
                    return false;
                }
            }
            _ => {
                if !byte.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::without_foreign_thinking;

    #[test]
    fn d70_1_uuid_thinking_is_dropped_for_claude() {
        let body = br#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"550e8400-e29b-41d4-a716-446655440000"},{"type":"text","text":"hi"}]}]}"#;
        let out = without_foreign_thinking("https://api.anthropic.com", "anthropic-messages", body);
        let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
        let content = value["messages"][0]["content"].as_array().expect("content");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
    }

    #[test]
    fn d70_2_claude_base64_signature_stays() {
        let sig = "A".repeat(800);
        let body = format!(
            r#"{{"model":"claude-sonnet-4-20250514","messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"x","signature":"{sig}"}}]}}]}}"#
        );
        let out = without_foreign_thinking(
            "https://api.anthropic.com",
            "anthropic-messages",
            body.as_bytes(),
        );
        assert!(
            out.windows(b"signature".len())
                .any(|window| window == b"signature")
        );
    }

    #[test]
    fn d70_3_deepseek_upstream_keeps_uuid_thinking() {
        let body = br#"{"model":"deepseek-chat","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"550e8400-e29b-41d4-a716-446655440000"}]}]}"#;
        let out = without_foreign_thinking("https://api.deepseek.com", "anthropic-messages", body);
        assert_eq!(out, body);
    }

    #[test]
    fn d70_4_unsigned_thinking_stays_on_claude() {
        let body = br#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"x"}]}]}"#;
        let out = without_foreign_thinking("https://api.anthropic.com", "anthropic-messages", body);
        assert_eq!(out, body);
    }

    #[test]
    fn d70_5_uuid_with_an_extra_char_stays() {
        let body = br#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"550e8400-e29b-41d4-a716-446655440000a"}]}]}"#;
        let out = without_foreign_thinking("https://api.anthropic.com", "anthropic-messages", body);
        assert_eq!(out, body);
    }

    #[test]
    fn d70_6_non_thinking_block_with_uuid_signature_stays() {
        let body = br#"{"model":"claude-sonnet-4-20250514","messages":[{"role":"assistant","content":[{"type":"text","text":"x","signature":"550e8400-e29b-41d4-a716-446655440000"}]}]}"#;
        let out = without_foreign_thinking("https://api.anthropic.com", "anthropic-messages", body);
        assert_eq!(out, body);
    }
}
