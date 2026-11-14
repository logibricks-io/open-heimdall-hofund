//! Reshapes Responses bodies for Sign in with ChatGPT.

use hofund_plugin_sdk::json::{Map, Value, json};

const DROPPED: [&str; 16] = [
    "background",
    "conversation",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "moderation",
    "multi_agent",
    "prompt",
    "prompt_cache_retention",
    "safety_identifier",
    "temperature",
    "top_logprobs",
    "top_p",
    "truncation",
    "user",
    "previous_response_id",
];

const HOSTED: [&str; 8] = [
    "image_generation",
    "file_search",
    "code_interpreter",
    "computer_use",
    "computer_use_preview",
    "computer",
    "mcp",
    "tool_search",
];

/// Rewrites `body` for `api.openai.com` with a ChatGPT API token.
pub(crate) fn body(body: &mut Map<String, Value>) -> Vec<(String, String)> {
    for key in DROPPED {
        body.remove(key);
    }
    body.insert("store".to_owned(), Value::Bool(false));
    body.insert("stream".to_owned(), Value::Bool(true));
    match body.get_mut("input") {
        Some(Value::String(text)) => {
            let message = json_message(text);
            body.insert("input".to_owned(), Value::Array(vec![message]));
        }
        Some(Value::Array(items)) => {
            for item in items {
                let Some(obj) = item.as_object_mut() else {
                    continue;
                };
                if obj.get("role").and_then(Value::as_str) == Some("system") {
                    obj.insert("role".to_owned(), Value::String("developer".to_owned()));
                }
                if obj.get("type").and_then(Value::as_str) == Some("additional_tools")
                    && let Some(Value::Array(tools)) = obj.get_mut("tools")
                {
                    *tools = own_tools(std::mem::take(tools));
                }
            }
        }
        _ => {}
    }
    if body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        let instructions = body
            .get("instructions")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(Value::Array(input)) = body.get("input")
            && let Some(key) = conversation_key(instructions, input)
        {
            body.insert("prompt_cache_key".to_owned(), Value::String(key));
        }
    }
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        let kept = own_tools(std::mem::take(tools));
        if kept.is_empty() {
            body.remove("tools");
            body.remove("tool_choice");
        } else {
            body.insert("tools".to_owned(), Value::Array(kept));
        }
    }
    session_headers(body)
}

fn json_message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": text,
        }],
    })
}

fn own_tools(tools: Vec<Value>) -> Vec<Value> {
    tools
        .into_iter()
        .filter(|tool| {
            tool.get("type")
                .and_then(Value::as_str)
                .is_none_or(|ty| !HOSTED.contains(&ty))
        })
        .collect()
}

fn session_headers(body: &Map<String, Value>) -> Vec<(String, String)> {
    body.get("prompt_cache_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(|key| {
            vec![
                ("session-id".to_owned(), key.to_owned()),
                ("thread-id".to_owned(), key.to_owned()),
            ]
        })
        .unwrap_or_default()
}

pub(crate) fn conversation_key(instructions: &str, input: &[Value]) -> Option<String> {
    let mut hashed = instructions.to_owned();
    let mut user = false;
    for item in input {
        canonical(item, &mut hashed);
        if item.get("role").and_then(Value::as_str) == Some("user") {
            user = true;
            break;
        }
    }
    if !user {
        return None;
    }
    let hex = hofund_plugin_sdk::sigv4::payload_hash(hashed.as_bytes());
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(key);
                out.push(':');
                canonical(item, out);
            }
            out.push('}');
        }
        Value::String(text) => {
            out.push('"');
            out.push_str(text);
            out.push('"');
        }
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
    }
}
