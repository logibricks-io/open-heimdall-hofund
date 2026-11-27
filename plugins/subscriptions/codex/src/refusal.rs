//! Recover a posted Responses body after an unreadable-item refusal from the
//! Codex backend.

use hofund_plugin_sdk::json::{self, Value};

const SEALED_KINDS: [&str; 4] = [
    "reasoning",
    "compaction",
    "compaction_summary",
    "context_compaction",
];

const UNREADABLE_MARKERS: [&str; 3] = [
    "encrypted content for item",
    "item with id",
    "referenced reasoning item",
];

/// The Responses body without what `message` says the backend cannot read.
#[must_use]
pub(crate) fn without_unreadable(body: &[u8], message: &[u8]) -> Option<Vec<u8>> {
    if let Some(field) = hofund_plugin_sdk::refused_passthrough(message) {
        let (trimmed, ok) = hofund_plugin_sdk::without_passthrough(body, &field);
        return ok.then_some(trimmed);
    }
    let text = message_from(message);
    if agent_message_refused(&text) {
        return without_agent_messages(body);
    }
    let lower = text.to_ascii_lowercase();
    if !foreign_reasoning(&text) && !lower.contains("not found") {
        return None;
    }
    if let Some(id) = unreadable_item_id(&text)
        && let Some(trimmed) = without_item(body, &id)
    {
        return Some(trimmed);
    }
    if foreign_reasoning(&text) {
        return without_seals(body);
    }
    None
}

/// Whether `tries` is past the reference's unreadable-item retry budget.
#[must_use]
pub(crate) const fn unreadable_retry_exhausted(tries: u8) -> bool {
    tries >= 3
}

fn message_from(message: &[u8]) -> String {
    if let Ok(Value::Object(object)) = json::from_slice::<Value>(message)
        && let Some(Value::Object(error)) = object.get("error")
    {
        if let Some(Value::String(text)) = error.get("message") {
            return text.clone();
        }
        if let Some(Value::String(text)) = error.get("code") {
            return text.clone();
        }
    }
    String::from_utf8_lossy(message).into_owned()
}

fn foreign_reasoning(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("invalid_encrypted_content") {
        return true;
    }
    if lower.contains("invalid encrypted summary") || lower.contains("invalid_encrypted summary") {
        return true;
    }
    if lower.contains("invalid encrypted content") || lower.contains("invalid_encrypted content") {
        return true;
    }
    if let Some(index) = lower.find("encrypted") {
        let window = &lower[index..lower.len().min(index + 120)];
        if window.contains("could not be verified") || window.contains("could not be decrypted") {
            return true;
        }
    }
    if lower.contains("could not decrypt") && lower.contains("encrypted_content") {
        return true;
    }
    if lower.contains("could not verify") && lower.contains("encrypted") {
        return true;
    }
    if let Some(index) = lower.find("compaction") {
        let window = &lower[index..lower.len().min(index + 40)];
        if window.contains("could not decode") || window.contains("could not decrypt") {
            return true;
        }
        if window.contains("blob") {
            return true;
        }
    }
    false
}

fn unreadable_item_id(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let (marker_at, marker) = UNREADABLE_MARKERS
        .iter()
        .filter_map(|marker| lower.find(marker).map(|at| (at, *marker)))
        .min_by_key(|(at, _)| *at)?;
    let tail = text[marker_at + marker.len()..].trim_start();
    let tail = tail.strip_prefix(['\'', '"', ' ']).unwrap_or(tail);
    item_id_token(tail)
}

fn item_id_token(text: &str) -> Option<String> {
    let end = text
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        .map(|(index, ch)| index + ch.len_utf8())
        .last()
        .unwrap_or(0);
    let id = &text[..end];
    if id.contains('_') && !id.is_empty() {
        Some(id.to_owned())
    } else {
        None
    }
}

fn agent_message_refused(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let Some(at) = lower.find("unsupported input item") else {
        return false;
    };
    let tail = lower[at + "unsupported input item".len()..].trim_start();
    let tail = tail.strip_prefix(':').map_or(tail, str::trim_start);
    let tail = tail.trim_start_matches(['\'', '"']);
    tail.starts_with("agent_message")
}

fn without_agent_messages(body: &[u8]) -> Option<Vec<u8>> {
    let mut root = json::from_slice::<Value>(body).ok()?;
    let input = root.get_mut("input")?.as_array_mut()?;
    let mut changed = false;
    for item in input {
        if let Some(message) = agent_message_as_user(item) {
            *item = message;
            changed = true;
        }
    }
    changed.then(|| json::to_vec(&root).ok()).flatten()
}

fn agent_message_as_user(item: &Value) -> Option<Value> {
    let object = item.as_object()?;
    if object.get("type").and_then(Value::as_str)? != "agent_message"
        || object.contains_key("encrypted_content")
    {
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
            json::json!({"type": "input_text", "text": format!("{header}\n\n")}),
        );
    }
    Some(json::json!({"type": "message", "role": "user", "content": content}))
}

fn without_item(body: &[u8], id: &str) -> Option<Vec<u8>> {
    let mut root = json::from_slice::<Value>(body).ok()?;
    let input = root.get_mut("input")?.as_array_mut()?;
    let before = input.len();
    input.retain(|item| item.get("id").and_then(Value::as_str) != Some(id));
    if input.len() == before {
        return None;
    }
    json::to_vec(&root).ok()
}

fn without_seals(body: &[u8]) -> Option<Vec<u8>> {
    let mut root = json::from_slice::<Value>(body).ok()?;
    let input = root.get_mut("input")?.as_array_mut()?;
    let mut changed = false;
    let mut kept = Vec::with_capacity(input.len());
    for item in input.drain(..) {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            changed = true;
            continue;
        }
        let mut item = if let Some(stripped) = without_item_seal(&item) {
            changed = true;
            stripped
        } else {
            item
        };
        if let Some(stripped) = without_sealed_agent_message(&item) {
            changed = true;
            item = stripped;
        }
        kept.push(item);
    }
    if !changed {
        return None;
    }
    root["input"] = Value::Array(kept);
    json::to_vec(&root).ok()
}

fn without_item_seal(item: &Value) -> Option<Value> {
    let kind = item.get("type")?.as_str()?;
    if SEALED_KINDS.contains(&kind) {
        return None;
    }
    let encrypted = item.get("encrypted_content")?;
    if encrypted.as_str().is_none_or(str::is_empty) {
        return None;
    }
    let Value::Object(object) = item else {
        return None;
    };
    let mut object = object.clone();
    object.remove("encrypted_content");
    Some(Value::Object(object))
}

fn without_sealed_agent_message(item: &Value) -> Option<Value> {
    if item.get("type").and_then(Value::as_str)? != "agent_message" {
        return None;
    }
    let parts = item.get("content")?.as_array()?;
    let message = parts.iter().any(|part| {
        part.get("type").and_then(Value::as_str) == Some("input_text")
            && part
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.starts_with("Message Type: MESSAGE\n"))
    });
    let sealed = |part: &Value| {
        part.get("type").and_then(Value::as_str) == Some("encrypted_content")
            && part
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|seal| !seal.is_empty())
    };
    if !message || !parts.iter().any(&sealed) {
        return None;
    }
    let author = item
        .get("author")
        .and_then(Value::as_str)
        .filter(|author| !author.is_empty())
        .unwrap_or("another agent");
    let note = format!(
        "[This message from {author} was sealed by the server that answered that agent, and only that server can open it, so it was left out here. An agent's final result still reaches the agent that spawned it as text.]"
    );
    let mut replacement = item.clone();
    replacement["content"] = Value::Array(
        parts
            .iter()
            .map(|part| {
                if sealed(part) {
                    json::json!({"type": "input_text", "text": note})
                } else {
                    part.clone()
                }
            })
            .collect(),
    );
    Some(replacement)
}

#[cfg(test)]
mod tests {
    use super::{
        foreign_reasoning, unreadable_item_id, without_item, without_seals, without_unreadable,
    };
    use hofund_plugin_sdk::json::{self, Value, json};

    #[test]
    fn r47j_unreadable_item_id_and_foreign_reasoning_match_reference() {
        assert_eq!(
            unreadable_item_id("Item with id 'rs_1' not found"),
            Some("rs_1".to_owned())
        );
        assert_eq!(
            unreadable_item_id("Referenced reasoning item rs_2 was not found"),
            Some("rs_2".to_owned())
        );
        assert!(foreign_reasoning("invalid_encrypted_content"));
        assert!(foreign_reasoning("Could not decode the compaction blob"));
        assert!(!foreign_reasoning("model is disabled"));
    }

    #[test]
    fn r47j_without_item_drops_named_input_item() {
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"type": "reasoning", "id": "rs_bad", "encrypted_content": "sealed"},
                {"type": "message", "role": "user", "content": "next"}
            ]
        });
        let trimmed = without_unreadable(
            &json::to_vec(&body).expect("json"),
            br#"{"error":{"message":"Item with id 'rs_bad' not found"}}"#,
        )
        .expect("trimmed");
        let value: Value = json::from_slice(&trimmed).expect("json");
        assert_eq!(
            value["input"],
            json!([{"type": "message", "role": "user", "content": "next"}])
        );
    }

    #[test]
    fn r47j_without_seals_drops_reasoning_and_strips_other_seals() {
        let body = json!({
            "input": [
                {"type": "reasoning", "encrypted_content": "seal"},
                {"type": "message", "role": "assistant", "encrypted_content": "replay"},
                {"type": "message", "role": "user", "content": "hi"}
            ]
        });
        let trimmed = without_unreadable(
            &json::to_vec(&body).expect("json"),
            br#"{"error":{"message":"invalid_encrypted_content"}}"#,
        )
        .expect("trimmed");
        let value: Value = json::from_slice(&trimmed).expect("json");
        assert_eq!(
            value["input"],
            json!([
                {"type": "message", "role": "assistant"},
                {"type": "message", "role": "user", "content": "hi"}
            ])
        );
        assert!(without_seals(br#"{"input":[]}"#).is_none());
        assert!(without_item(br#"{"input":[{"type":"message"}]}"#, "rs_1").is_none());
    }

    #[test]
    fn r47j_unrelated_refusal_does_not_trim() {
        let body = json!({"input": [{"type": "message", "content": "hi"}]});
        assert!(
            without_unreadable(
                &json::to_vec(&body).expect("json"),
                br#"{"error":{"message":"model is not available"}}"#
            )
            .is_none()
        );
    }

    fn d272_refusal(body: &Value, message: &str) -> Option<Value> {
        let message = json!({"error": {"message": message, "type": "invalid_request_error"}});
        let trimmed = without_unreadable(
            &json::to_vec(body).expect("body"),
            &json::to_vec(&message).expect("message"),
        )?;
        Some(json::from_slice(&trimmed).expect("trimmed body"))
    }

    #[test]
    fn d272_agent_message_refusal_rewrites_each_plain_item_as_a_user_turn() {
        let body = json!({"input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "agent_message", "author": "/root", "recipient": "/root/research", "content": [
                {"type": "input_text", "text": "Message Type: NEW_TASK\nPayload:\nresearch the failure"},
                {"type": "input_image", "image_url": "https://example.test/task.png"}
            ]},
            {"type": "agent_message", "author": "/root", "content": [{"type": "input_text", "text": "from only"}]},
            {"type": "agent_message", "recipient": "/root/research", "content": [{"type": "input_text", "text": "to only"}]},
            {"type": "agent_message", "content": [{"type": "input_text", "text": "no route"}]}
        ]});
        let rewritten = d272_refusal(&body, "Unsupported input item: agent_message")
            .expect("plain agent messages are retried");
        assert_eq!(
            rewritten["input"],
            json!([
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "From /root to /root/research\n\n"},
                    {"type": "input_text", "text": "Message Type: NEW_TASK\nPayload:\nresearch the failure"},
                    {"type": "input_image", "image_url": "https://example.test/task.png"}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "From /root\n\n"},
                    {"type": "input_text", "text": "from only"}
                ]},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "To /root/research\n\n"},
                    {"type": "input_text", "text": "to only"}
                ]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "no route"}]}
            ])
        );
    }

    #[test]
    fn d272_agent_message_rewrite_preserves_sealed_empty_and_other_shapes() {
        let sealed = json!({"type": "agent_message", "author": "/root", "content": [
            {"type": "input_text", "text": "Message Type: MESSAGE\nPayload:\n"},
            {"type": "encrypted_content", "encrypted_content": "sealed"}
        ]});
        let empty = json!({"type": "agent_message", "content": []});
        let other =
            json!({"type": "agent_message", "content": [{"type": "output_text", "text": "other"}]});
        let plain =
            json!({"type": "agent_message", "content": [{"type": "input_text", "text": "plain"}]});
        let body = json!({"input": [sealed.clone(), empty.clone(), other.clone(), plain]});
        let rewritten = d272_refusal(&body, "unsupported input item 'agent_message'")
            .expect("the plain item is retried");
        assert_eq!(rewritten["input"][0], sealed);
        assert_eq!(rewritten["input"][1], empty);
        assert_eq!(rewritten["input"][2], other);
        assert_eq!(rewritten["input"][3]["type"], "message");
        assert!(
            d272_refusal(
                &json!({"input": [sealed, empty, other]}),
                "Unsupported input item: agent_message"
            )
            .is_none()
        );
        assert!(d272_refusal(&body, "Some other bad request").is_none());
    }

    #[test]
    fn d272_foreign_seal_refusal_replaces_message_seal_and_keeps_new_task() {
        let task = json!({"type": "agent_message", "author": "/root", "recipient": "/root/sub", "content": [
            {"type": "input_text", "text": "Message Type: NEW_TASK\nTask name: /root/sub\nPayload:\n"},
            {"type": "encrypted_content", "encrypted_content": "sealed-task"}
        ]});
        let body = json!({"input": [
            {"type": "agent_message", "author": "/root/sub", "recipient": "/root", "content": [
                {"type": "input_text", "text": "Message Type: MESSAGE\nTask name: /root\nSender: /root/sub\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": "sealed-message"}
            ]},
            task.clone()
        ]});
        let rewritten = d272_refusal(&body, "invalid_encrypted_content")
            .expect("foreign agent message seal is retried");
        assert_eq!(rewritten["input"][0]["type"], "agent_message");
        assert_eq!(
            rewritten["input"][0]["content"][0],
            body["input"][0]["content"][0]
        );
        assert_eq!(rewritten["input"][0]["content"][1]["type"], "input_text");
        let note = rewritten["input"][0]["content"][1]["text"]
            .as_str()
            .expect("note");
        assert!(note.contains("/root/sub"));
        assert!(note.contains("left out"));
        assert!(note.contains("final result"));
        assert_eq!(rewritten["input"][1], task);
        assert!(d272_refusal(&json!({"input": [task]}), "invalid_encrypted_content").is_none());
    }

    #[test]
    fn d272_named_foreign_agent_message_falls_back_to_seal_retry() {
        let body = json!({"input": [{
            "type": "agent_message",
            "author": "/root/sub",
            "recipient": "/root",
            "content": [
                {"type": "input_text", "text": "Message Type: MESSAGE\nTask name: /root\nSender: /root/sub\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": "sealed-message"}
            ]
        }]});
        let rewritten = d272_refusal(
            &body,
            "The encrypted content for item amsg_a could not be verified. Reason: Encrypted content could not be decrypted or parsed.",
        )
        .expect("a shaped item without the named id still has its seal removed");
        assert_eq!(rewritten["input"][0]["type"], "agent_message");
        assert_eq!(rewritten["input"][0]["content"][1]["type"], "input_text");
        assert!(
            rewritten["input"][0]["content"][1]["text"]
                .as_str()
                .is_some_and(|note| note.contains("left out"))
        );
    }
}
