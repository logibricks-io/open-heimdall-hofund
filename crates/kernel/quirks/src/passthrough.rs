use serde_json::Value;

use crate::json::{self, Node};

const PASSTHROUGH: &str = "internal_chat_message_metadata_passthrough";
const UNKNOWN_PARAMETER: &str = "[unknown_parameter]";

/// Names the passthrough member an upstream unknown-parameter refusal names.
/// An empty string means the whole passthrough object.
#[must_use]
pub fn refused_passthrough(body: &[u8]) -> Option<String> {
    let body = words_of(body);
    let text = String::from_utf8_lossy(&body);
    let mut best: Option<(usize, String)> = None;
    for (start, _) in text.match_indices("input[") {
        let Some((field, end)) = passthrough_path(&text, start) else {
            continue;
        };
        let mut at = end;
        if text.as_bytes().get(at) == Some(&b']') {
            at += 1;
        }
        at = skip_space(&text, at);
        if text[at..].starts_with(UNKNOWN_PARAMETER) {
            choose(&mut best, start, field);
        }
    }
    let lower = text.to_ascii_lowercase();
    for (start, _) in lower.match_indices("unknown parameter") {
        let mut at = start + "unknown parameter".len();
        if text.as_bytes().get(at) == Some(&b':') {
            at += 1;
        }
        at = skip_space(&text, at);
        if text.as_bytes().get(at) != Some(&b'\'') {
            continue;
        }
        at += 1;
        let Some((field, end)) = passthrough_path(&text, at) else {
            continue;
        };
        if text.as_bytes().get(end) == Some(&b'\'') {
            choose(&mut best, start, field);
        }
    }
    best.map(|(_, field)| field)
}

fn choose(best: &mut Option<(usize, String)>, start: usize, field: String) {
    if best.as_ref().is_none_or(|(at, _)| start < *at) {
        *best = Some((start, field));
    }
}

fn passthrough_path(text: &str, start: usize) -> Option<(String, usize)> {
    let rest = text.get(start + "input[".len()..)?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let after = rest[digits..].strip_prefix("].")?;
    let after = after.strip_prefix(PASSTHROUGH)?;
    let path_len = "input[".len() + digits + 2 + PASSTHROUGH.len();
    if let Some(member) = after.strip_prefix('.') {
        let len = member
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            .count();
        if len == 0 {
            return None;
        }
        return Some((member[..len].to_owned(), start + path_len + 1 + len));
    }
    Some((String::new(), start + path_len))
}

fn skip_space(text: &str, mut at: usize) -> usize {
    while text
        .as_bytes()
        .get(at)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | 0x0c))
    {
        at += 1;
    }
    at
}

fn words_of(body: &[u8]) -> Vec<u8> {
    if !body.windows(2).any(|window| window == b"\\u") {
        return body.to_vec();
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return body.to_vec();
    };
    let mut out = body.to_vec();
    append_strings(&value, &mut out);
    out
}

fn append_strings(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::String(text) => {
            out.push(b'\n');
            out.extend_from_slice(text.as_bytes());
        }
        Value::Array(items) => {
            for item in items {
                append_strings(item, out);
            }
        }
        Value::Object(members) => {
            for value in members.values() {
                append_strings(value, out);
            }
        }
        _ => {}
    }
}

/// Removes `field` from every input item's passthrough, preserving every
/// other byte. An empty field removes the whole passthrough object.
#[must_use]
pub fn without_passthrough(body: &[u8], field: &str) -> (Vec<u8>, bool) {
    without_passthrough_if(body, |candidate| candidate == field)
}

pub(crate) fn without_passthrough_if(body: &[u8], drop: impl Fn(&str) -> bool) -> (Vec<u8>, bool) {
    let Some(root) = json::parse(body) else {
        return (body.to_vec(), false);
    };
    let Some(items) = root.get("input").and_then(Node::items) else {
        return (body.to_vec(), false);
    };
    let mut edits = Vec::new();
    for item in items {
        let Some(passthrough) = item.get(PASSTHROUGH) else {
            continue;
        };
        if drop("") {
            edits.extend(json::remove_fields(item, &[PASSTHROUGH]));
            continue;
        }
        let Some(members) = passthrough.members() else {
            continue;
        };
        let fields: Vec<String> = members
            .iter()
            .filter(|member| drop(&member.key))
            .map(|member| member.key.clone())
            .collect();
        let fields: Vec<&str> = fields.iter().map(String::as_str).collect();
        edits.extend(json::remove_fields(passthrough, &fields));
    }
    let changed = !edits.is_empty();
    (json::apply(body, edits), changed)
}

#[cfg(test)]
mod tests {
    use super::without_passthrough;

    #[test]
    fn removal_preserves_every_other_byte() {
        let body = br#"{"model":"m", "input":[{"type":"message", "internal_chat_message_metadata_passthrough":{"turn_id":"t1", "content_token_count":[3], "other": true}, "content":[{"text":"hi"}]}]}"#;
        let (stripped, changed) = without_passthrough(body, "content_token_count");
        assert!(changed);
        assert_eq!(
            stripped,
            br#"{"model":"m", "input":[{"type":"message", "internal_chat_message_metadata_passthrough":{"turn_id":"t1", "other": true}, "content":[{"text":"hi"}]}]}"#
        );
        let (_, again) = without_passthrough(&stripped, "content_token_count");
        assert!(!again);
    }
}
