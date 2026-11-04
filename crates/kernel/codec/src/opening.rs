//! Conversation opening bytes for affinity keys when no session header is sent.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;

/// Stable id suffix from request body when no session header is present.
pub fn hashed_id(body: &[u8]) -> String {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let object = parsed.as_ref().and_then(Value::as_object);
    let messages = object
        .and_then(|object| object.get("messages"))
        .and_then(Value::as_array);
    let contents = object
        .and_then(|object| object.get("contents"))
        .and_then(Value::as_array);
    let input = object.and_then(|object| object.get("input"));
    let input_items = input.and_then(Value::as_array);
    let input_string = input.and_then(|value| {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|_| value.clone())
    });
    let gemini =
        messages.is_none_or(Vec::is_empty) && contents.is_some_and(|items| !items.is_empty());
    let items = if gemini {
        contents
    } else if messages.is_some_and(|items| !items.is_empty()) {
        messages
    } else {
        input_items
    };
    if let Some(string) = input_string {
        return digest_bytes(string.to_string().as_bytes());
    }
    let Some(items) = items.filter(|items| !items.is_empty()) else {
        return digest_bytes(body);
    };
    let start = items
        .iter()
        .position(|item| is_user_turn(item, gemini))
        .unwrap_or(0);
    let slice = &items[start..];
    let opening: Vec<&Value> = conversation_opening(slice, gemini);
    let bytes = if opening.len() == 1 {
        serde_json::to_vec(opening[0]).unwrap_or_else(|_| body.to_vec())
    } else {
        let mut out = Vec::new();
        for (index, message) in opening.iter().enumerate() {
            if index > 0 {
                out.push(b'\n');
            }
            write_opening(&mut out, message);
        }
        out
    };
    digest_bytes(&bytes)
}

fn digest_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!(
        "hofund-{}",
        digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn is_user_turn(item: &Value, gemini: bool) -> bool {
    let role = item.get("role").and_then(Value::as_str);
    role == Some("user") || (gemini && role.is_none())
}

fn conversation_opening(items: &[Value], gemini: bool) -> Vec<&Value> {
    let Some(first) = items.first() else {
        return Vec::new();
    };
    let mut open = vec![first];
    for item in items.iter().skip(1) {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system" || role == "developer" {
            continue;
        }
        if role != "user" && !(gemini && role.is_empty()) {
            break;
        }
        if open.len() == 1 {
            let (theirs, result) = user_opens(open[0]);
            if theirs || result {
                break;
            }
        }
        let (theirs, result) = user_opens(item);
        if result {
            break;
        }
        open.push(item);
        if theirs {
            return open;
        }
    }
    open.truncate(1);
    open
}

fn user_opens(message: &Value) -> (bool, bool) {
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        return (!user_words(text).is_empty(), false);
    }
    let parts = message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            message
                .get("parts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    let mut theirs = false;
    for part in parts {
        let Some(object) = part.as_object() else {
            continue;
        };
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "tool_result" || object.contains_key("functionResponse") {
            return (false, true);
        }
        if matches!(
            kind,
            "image"
                | "image_url"
                | "input_image"
                | "document"
                | "file"
                | "input_file"
                | "input_audio"
        ) || object.contains_key("inlineData")
            || object.contains_key("fileData")
        {
            theirs = true;
            continue;
        }
        if matches!(kind, "text" | "input_text" | "")
            && !object
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            let text = object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            theirs |= !user_words(text).is_empty();
        }
    }
    (theirs, false)
}

fn user_words(text: &str) -> String {
    let mut out = text.to_owned();
    for tag in ["environment_context", "user_instructions"] {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        while let Some(start) = out.find(&open) {
            let Some(rel) = out[start..].find(&close) else {
                break;
            };
            let end = start + rel + close.len();
            out.replace_range(start..end, "");
        }
    }
    if let Some(idx) = out.find("<session-context") {
        out.truncate(idx);
    }
    out.trim().to_string()
}

fn write_opening(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<_> = map.keys().filter(|key| *key != "cache_control").collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write!(out, "\"{key}\":").expect("write key");
                if *key == "content"
                    && let Some(text) = one_text_part(map.get(*key))
                {
                    write!(out, "{}", serde_json::to_string(&text).expect("string"))
                        .expect("write str");
                    continue;
                }
                write_opening(out, &map[*key]);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_opening(out, item);
            }
            out.push(b']');
        }
        Value::String(text) => {
            write!(out, "{}", serde_json::to_string(text).expect("string")).expect("write str");
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {
            let raw = serde_json::to_string(value).expect("json");
            out.extend_from_slice(raw.as_bytes());
        }
    }
}

fn one_text_part(content: Option<&Value>) -> Option<String> {
    let array = content?.as_array()?;
    if array.len() != 1 {
        return None;
    }
    let part = array.first()?.as_object()?;
    let kind = part.get("type").and_then(Value::as_str).unwrap_or("text");
    if kind != "text" && kind != "input_text" {
        return None;
    }
    if part
        .keys()
        .any(|key| key != "type" && key != "text" && key != "cache_control")
    {
        return None;
    }
    part.get("text").and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::hashed_id;

    const OPENING_ENV: &str = "<environment_context>cwd=/w/proj ...</environment_context>";

    #[test]
    fn d54_4_first_user_message_regression_id_unchanged() {
        let body = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
        assert_eq!(hashed_id(body), "hofund-b5344ebd07750817371cf467");
    }

    #[test]
    fn d54_1_environment_context_conversations_differ_by_user_text() {
        let env = serde_json::to_string(OPENING_ENV).expect("env");
        let a = hashed_id(
            format!(r#"{{"messages":[{{"role":"user","content":{env}}},{{"role":"user","content":"question-a"}}]}}"#)
                .as_bytes(),
        );
        let b = hashed_id(
            format!(r#"{{"messages":[{{"role":"user","content":{env}}},{{"role":"user","content":"question-b"}}]}}"#)
                .as_bytes(),
        );
        assert_ne!(a, b);
        let env_only =
            hashed_id(format!(r#"{{"messages":[{{"role":"user","content":{env}}}]}}"#).as_bytes());
        assert_ne!(a, env_only);
    }

    #[test]
    fn d54_2_same_user_text_yields_same_id() {
        let env = serde_json::to_string(OPENING_ENV).expect("env");
        let asked = serde_json::to_string("same question").expect("asked");
        let first = hashed_id(
            format!(r#"{{"messages":[{{"role":"user","content":{env}}},{{"role":"user","content":{asked}}}]}}"#)
                .as_bytes(),
        );
        let later = hashed_id(
            format!(
                r#"{{"messages":[{{"role":"user","content":{env}}},{{"role":"user","content":{asked}}},{{"role":"assistant","content":"hi"}},{{"role":"user","content":"follow-up"}}]}}"#
            )
            .as_bytes(),
        );
        assert_eq!(first, later);
    }

    #[test]
    fn d54_3_cache_control_on_opening_does_not_change_id() {
        let env = serde_json::to_string(OPENING_ENV).expect("env");
        let plain = hashed_id(
            format!(r#"{{"messages":[{{"role":"user","content":{env}}},{{"role":"user","content":"asked"}}]}}"#)
                .as_bytes(),
        );
        let marked = hashed_id(
            format!(
                r#"{{"messages":[{{"role":"user","content":{env}}},{{"role":"user","content":[{{"type":"text","text":"asked","cache_control":{{"type":"ephemeral"}}}}]}}]}}"#
            )
            .as_bytes(),
        );
        assert_eq!(plain, marked);
    }

    #[test]
    fn d54_5_tool_result_ends_the_opening() {
        let env = serde_json::to_string(OPENING_ENV).expect("env");
        let env_only =
            hashed_id(format!(r#"{{"messages":[{{"role":"user","content":{env}}}]}}"#).as_bytes());
        let with_result = hashed_id(
            format!(
                r#"{{"messages":[{{"role":"user","content":{env}}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok"}}]}},{{"role":"user","content":"asked"}}]}}"#
            )
            .as_bytes(),
        );
        assert_eq!(env_only, with_result);
    }
}
