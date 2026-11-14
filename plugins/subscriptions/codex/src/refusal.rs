//! Trim a posted Responses body after an unreadable-item refusal from the Codex
//! backend.

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
    let lower = text.to_ascii_lowercase();
    if !foreign_reasoning(&text) && !lower.contains("not found") {
        return None;
    }
    if let Some(id) = unreadable_item_id(&text) {
        return without_item(body, &id);
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
        if let Some(stripped) = without_item_seal(&item) {
            changed = true;
            kept.push(stripped);
        } else {
            kept.push(item);
        }
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
}
