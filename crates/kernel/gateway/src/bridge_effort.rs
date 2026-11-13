//! Subscription-bridge stages A–B: trusted effort / think-off / classifier bits.
//!
//! Written onto `Request.extensions` after decode and before `execute`.
//! See the H8b subscription-bridge design §1.5.

use hofund_ir::Request;
use hofund_plugin_api::ProtocolId;
use serde_json::{Value, json};

/// Extension keys consumed by the H8b subscription plugin.
pub const BRIDGE_EFFORT: &str = "hofund_bridge_effort";
pub const THINK_OFF: &str = "hofund_think_off";
pub const AUTO_MODE_CLASSIFIER: &str = "hofund_auto_mode_classifier";

/// Applies stages A–B to `request`, overwriting the trusted extensions.
pub fn apply(protocol: &str, raw_body: &[u8], request: &mut Request) {
    let (think_off, mut effort) = stage_a(protocol, raw_body, request);
    let classifier = auto_mode_classifier(request);
    if protocol == ProtocolId::ANTHROPIC_MESSAGES.as_str()
        && effort.is_empty()
        && !think_off
        && !classifier
    {
        effort = output_config_effort(raw_body);
    }
    request.extensions.insert(THINK_OFF, json!(think_off));
    request.extensions.insert(BRIDGE_EFFORT, json!(effort));
    request
        .extensions
        .insert(AUTO_MODE_CLASSIFIER, json!(classifier));
}

fn stage_a(protocol: &str, raw_body: &[u8], request: &Request) -> (bool, String) {
    if protocol == ProtocolId::ANTHROPIC_MESSAGES.as_str() {
        return anthropic_a(request);
    }
    if protocol == ProtocolId::OPENAI_CHAT.as_str() {
        return chat_a(raw_body);
    }
    if protocol == ProtocolId::OPENAI_RESPONSES.as_str() {
        return responses_a(raw_body);
    }
    (false, String::new())
}

fn anthropic_a(request: &Request) -> (bool, String) {
    let thinking = request
        .extensions
        .get("thinking")
        .and_then(Value::as_object);
    let typ = thinking
        .and_then(|o| o.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if typ == "disabled" || typ == "between_tools" {
        return (true, String::new());
    }
    let mut effort = String::new();
    if typ == "enabled" || typ == "adaptive" {
        let budget = thinking
            .and_then(|o| o.get("budget_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        effort = effort_of_budget(budget);
        if let Some(label) = output_config_effort_of(request) {
            effort = label;
        }
    }
    (false, effort)
}

fn chat_a(raw_body: &[u8]) -> (bool, String) {
    let Ok(value) = serde_json::from_slice::<Value>(raw_body) else {
        return (false, String::new());
    };
    let literal = value
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .unwrap_or("");
    think_off_and_effort(literal)
}

fn responses_a(raw_body: &[u8]) -> (bool, String) {
    let Ok(value) = serde_json::from_slice::<Value>(raw_body) else {
        return (false, String::new());
    };
    let mut literal = value
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    // Last configuration_update with a non-empty effort wins when top-level
    // reasoning is present (reference responses.go:412-417).
    if value.get("reasoning").is_some()
        && let Some(updated) = last_configuration_effort(&value)
    {
        literal = updated;
    }
    think_off_and_effort(&literal)
}

fn last_configuration_effort(value: &Value) -> Option<String> {
    let mut last = None;
    if let Some(items) = value.get("input").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("configuration_update") {
                continue;
            }
            if let Some(effort) = item
                .pointer("/reasoning/effort")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                last = Some(effort.to_owned());
            }
        }
    }
    last
}

fn think_off_and_effort(literal: &str) -> (bool, String) {
    if literal.is_empty() {
        return (false, String::new());
    }
    let think_off = literal.eq_ignore_ascii_case("none");
    (think_off, effort_of(literal))
}

fn output_config_effort(raw_body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(raw_body) else {
        return String::new();
    };
    value
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
        .map(effort_of)
        .unwrap_or_default()
}

fn output_config_effort_of(request: &Request) -> Option<String> {
    let label = request
        .extensions
        .get("output_config")
        .and_then(Value::as_object)
        .and_then(|o| o.get("effort"))
        .and_then(Value::as_str)?;
    let mapped = effort_of(label);
    (!mapped.is_empty()).then_some(mapped)
}

fn auto_mode_classifier(request: &Request) -> bool {
    if !request.tools.is_empty() {
        return false;
    }
    let system: String = request
        .messages
        .iter()
        .filter(|m| matches!(m.role, hofund_ir::Role::System | hofund_ir::Role::Developer))
        .flat_map(|m| {
            m.content.iter().filter_map(|part| match part {
                hofund_ir::ContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !system.contains("<block>") && !system.contains("<severity>") {
        return false;
    }
    request.messages.iter().any(|message| {
        message.role == hofund_ir::Role::User
            && message.content.iter().any(|part| match part {
                hofund_ir::ContentPart::Text(text) => {
                    text.text.trim_start().starts_with("<transcript>")
                }
                _ => false,
            })
    })
}

fn effort_of(label: &str) -> String {
    match label.trim().to_ascii_lowercase().as_str() {
        "minimal" | "none" => "low".into(),
        "low" | "medium" | "high" | "xhigh" | "max" => label.trim().to_ascii_lowercase(),
        "ultra" => "max".into(),
        _ => String::new(),
    }
}

fn effort_of_budget(n: i64) -> String {
    if n <= 0 {
        String::new()
    } else if n <= 4096 {
        "low".into()
    } else if n <= 12000 {
        "medium".into()
    } else if n <= 24000 {
        "high".into()
    } else {
        "xhigh".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_ir::{ContentPart, Message, Role};

    fn req() -> Request {
        Request::new("model")
    }

    #[test]
    fn chat_none_is_low_plus_think_off() {
        let mut request = req();
        let body = br#"{"model":"m","messages":[],"reasoning_effort":"none"}"#;
        apply(ProtocolId::OPENAI_CHAT.as_str(), body, &mut request);
        assert_eq!(request.extensions.get(BRIDGE_EFFORT), Some(&json!("low")));
        assert_eq!(request.extensions.get(THINK_OFF), Some(&json!(true)));
    }

    #[test]
    fn chat_minimal_is_low_without_think_off() {
        let mut request = req();
        let body = br#"{"model":"m","messages":[],"reasoning_effort":"minimal"}"#;
        apply(ProtocolId::OPENAI_CHAT.as_str(), body, &mut request);
        assert_eq!(request.extensions.get(BRIDGE_EFFORT), Some(&json!("low")));
        assert_eq!(request.extensions.get(THINK_OFF), Some(&json!(false)));
    }

    #[test]
    fn anthropic_disabled_ignores_output_config() {
        let mut request = req();
        request
            .extensions
            .insert("thinking", json!({"type": "disabled"}));
        request
            .extensions
            .insert("output_config", json!({"effort": "high"}));
        let body = br#"{"thinking":{"type":"disabled"},"output_config":{"effort":"high"}}"#;
        apply(ProtocolId::ANTHROPIC_MESSAGES.as_str(), body, &mut request);
        assert_eq!(request.extensions.get(THINK_OFF), Some(&json!(true)));
        assert_eq!(request.extensions.get(BRIDGE_EFFORT), Some(&json!("")));
    }

    #[test]
    fn anthropic_b_fills_output_config_when_no_thinking() {
        let mut request = req();
        let body = br#"{"output_config":{"effort":"max"}}"#;
        apply(ProtocolId::ANTHROPIC_MESSAGES.as_str(), body, &mut request);
        assert_eq!(request.extensions.get(THINK_OFF), Some(&json!(false)));
        assert_eq!(request.extensions.get(BRIDGE_EFFORT), Some(&json!("max")));
    }

    #[test]
    fn anthropic_classifier_skips_stage_b() {
        let mut request = req();
        request.messages = vec![
            {
                let mut m = Message::new(Role::System);
                m.content = vec![ContentPart::text("say <block>yes</block>")];
                m
            },
            {
                let mut m = Message::new(Role::User);
                m.content = vec![ContentPart::text("<transcript>\nls\n</transcript>")];
                m
            },
        ];
        let body = br#"{"output_config":{"effort":"high"}}"#;
        apply(ProtocolId::ANTHROPIC_MESSAGES.as_str(), body, &mut request);
        assert_eq!(
            request.extensions.get(AUTO_MODE_CLASSIFIER),
            Some(&json!(true))
        );
        assert_eq!(request.extensions.get(BRIDGE_EFFORT), Some(&json!("")));
    }

    #[test]
    fn responses_configuration_update_overrides_top_level_none() {
        let mut request = req();
        let body = br#"{
            "reasoning": {"effort": "none"},
            "input": [
                {"type": "configuration_update", "reasoning": {"effort": "high"}}
            ]
        }"#;
        apply(ProtocolId::OPENAI_RESPONSES.as_str(), body, &mut request);
        assert_eq!(request.extensions.get(BRIDGE_EFFORT), Some(&json!("high")));
        assert_eq!(request.extensions.get(THINK_OFF), Some(&json!(false)));
    }
}
