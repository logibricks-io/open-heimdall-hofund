//! The Responses body Codex CLI would send, made from the one the codec
//! encoded: it streams only, keeps nothing (so its items carry no ids),
//! carries Codex's instructions with the client's own first in the input,
//! and names its conversation in the session headers the backend caches by.

use hofund_plugin_sdk::json::{self, Map, Value};
use hofund_plugin_sdk::sigv4::payload_hash;

/// Body fields the backend refuses on every model it serves.
pub(crate) const DELETED_KEYS: [&str; 8] = [
    "max_output_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "previous_response_id",
    "user",
    "safety_identifier",
    "prompt_cache_options",
];

/// The service tiers Codex sends: Fast's and Ultrafast's.
const KEPT_TIERS: [&str; 2] = ["priority", "ultrafast"];

/// The longest call id the backend takes.
pub(crate) const MAX_CALL_ID: usize = 64;

/// The longest tool output kept in a note about an orphan output.
pub(crate) const MAX_NOTE: usize = 16_000;

/// The include entry that carries reasoning across turns.
const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

/// Call item types whose ids are bounded.
const CALL_TYPES: [&str; 8] = [
    "function_call",
    "function_call_output",
    "local_shell_call",
    "local_shell_call_output",
    "custom_tool_call",
    "custom_tool_call_output",
    "tool_search_call",
    "tool_search_output",
];

/// Reshapes `body` as Codex CLI sends it, with `codex` as the model's own
/// instructions when they are known, and returns the session headers.
pub(crate) fn body(body: &mut Map<String, Value>, codex: Option<&str>) -> Vec<(String, String)> {
    for key in DELETED_KEYS {
        body.remove(key);
    }
    let tier = body.get("service_tier").and_then(Value::as_str);
    if !tier.is_some_and(|tier| KEPT_TIERS.contains(&tier)) {
        body.remove("service_tier");
    }
    if let Some(Value::String(text)) = body.get("input") {
        let text = text.clone();
        body.insert(
            "input".to_owned(),
            Value::Array(vec![message("user", &text)]),
        );
    }
    let input = match body.remove("input") {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    let mut input = items(input);
    let own = body
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let key = body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .unwrap_or("");
    if key.is_empty()
        && let Some(key) = conversation_key(&own, &input)
    {
        body.insert("prompt_cache_key".to_owned(), Value::String(key));
    }
    // The client's instructions go first as a developer message and Codex's
    // in their place, unless they are Codex's. Without Codex's, the
    // client's stay where they are.
    if let Some(codex) = codex {
        let mut instructions = codex.to_owned();
        if !own.trim().is_empty() {
            if first_line(&own) == first_line(codex) {
                instructions = own;
            } else {
                input.insert(0, message("developer", &own));
            }
        }
        body.insert("instructions".to_owned(), Value::String(instructions));
    }
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("store".to_owned(), Value::Bool(false));
    body.insert("stream".to_owned(), Value::Bool(true));
    body.insert("tool_choice".to_owned(), Value::String("auto".to_owned()));
    declare_search(body);
    body.entry("parallel_tool_calls")
        .or_insert(Value::Bool(true));
    body.entry("text")
        .or_insert_with(|| json::json!({"verbosity": "medium"}));
    reasoning(body);
    session_headers(body)
}

/// A message item of `role` with one text part.
fn message(role: &str, text: &str) -> Value {
    json::json!({
        "type": "message",
        "role": role,
        "content": [{"type": "input_text", "text": text}],
    })
}

/// Reasoning and its encrypted content to carry across turns, unless it is
/// turned off; `ultra` is Codex CLI's name for sending `max`.
fn reasoning(body: &mut Map<String, Value>) {
    let mut effort = String::new();
    if let Some(Value::Object(reasoning)) = body.get_mut("reasoning") {
        effort = reasoning
            .get("effort")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if effort == "ultra" {
            reasoning.insert("effort".to_owned(), Value::String("max".to_owned()));
        }
        if effort != "none" {
            reasoning
                .entry("summary")
                .or_insert_with(|| Value::String("auto".to_owned()));
        }
    }
    if effort == "none" {
        body.remove("include");
        return;
    }
    let mut include = match body.remove("include") {
        Some(Value::Array(include)) => include,
        _ => Vec::new(),
    };
    if !include
        .iter()
        .any(|entry| entry.as_str() == Some(ENCRYPTED_REASONING))
    {
        include.push(Value::String(ENCRYPTED_REASONING.to_owned()));
    }
    body.insert("include".to_owned(), Value::Array(include));
}

/// The session-id and thread-id headers naming the body's conversation,
/// none when it has no `prompt_cache_key`.
fn session_headers(body: &Map<String, Value>) -> Vec<(String, String)> {
    let key = body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .unwrap_or("");
    if key.is_empty() {
        return Vec::new();
    }
    vec![
        ("session-id".to_owned(), key.to_owned()),
        ("thread-id".to_owned(), key.to_owned()),
    ]
}

/// A call id as the backend takes one: itself when it fits, else the
/// SHA-256 hex of it trimmed, so a call and its output stay paired.
pub(crate) fn bound_call_id(id: &str) -> String {
    if id.len() <= MAX_CALL_ID {
        return id.to_owned();
    }
    payload_hash(id.trim().as_bytes())
}

/// The type an item has, or `""`.
fn item_type(item: &Map<String, Value>) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Input items fit for a request that keeps nothing: no references to
/// stored items, no ids (nor a message's status, which goes with its id),
/// a tool output without its call told as a note, and no system role.
pub(crate) fn items(mut input: Vec<Value>) -> Vec<Value> {
    // call id -> the type of its output
    let mut calls: Vec<(String, &'static str)> = Vec::new();
    for item in &mut input {
        let Value::Object(item) = item else {
            continue;
        };
        let kind = item_type(item).to_owned();
        if CALL_TYPES.contains(&kind.as_str())
            && let Some(Value::String(id)) = item.get_mut("call_id")
        {
            let bound = bound_call_id(id);
            if bound != *id {
                *id = bound;
            }
        }
        let output = match kind.as_str() {
            "function_call" => "function_call_output",
            "local_shell_call" => "local_shell_call_output",
            "custom_tool_call" => "custom_tool_call_output",
            _ => continue,
        };
        let id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
        calls.push((id.trim().to_owned(), output));
    }
    let wanted = |id: &str| {
        calls
            .iter()
            .rev()
            .find(|(call, _)| call == id)
            .map_or("", |(_, output)| *output)
    };
    let mut out = Vec::with_capacity(input.len());
    for item in &input {
        let Value::Object(item) = item else {
            out.push(item.clone());
            continue;
        };
        let kind = item_type(item);
        if kind == "item_reference" {
            continue;
        }
        let call_id = item.get("call_id");
        let unnamed = call_id.is_none_or(|id| id.is_null() || id.as_str() == Some(""));
        // A notification Codex sends with no call id is not an output whose
        // call went missing: it goes as it is.
        if matches!(kind, "function_call_output" | "custom_tool_call_output") && unnamed {
            out.push(Value::Object(item.clone()));
            continue;
        }
        let mut item = item.clone();
        item.remove("id");
        if matches!(kind, "" | "message" | "reasoning") {
            item.remove("status");
        }
        if matches!(
            kind,
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output"
        ) {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let want = wanted(&id);
            let paired = want == kind
                || (kind == "function_call_output" && want == "local_shell_call_output");
            if !paired {
                item = orphan_note(&item, &id);
            }
        }
        if item.get("role").and_then(Value::as_str) == Some("system")
            && matches!(item_type(&item), "" | "message")
        {
            item.insert("role".to_owned(), Value::String("developer".to_owned()));
        }
        out.push(Value::Object(item));
    }
    out
}

/// A tool output whose call is not in the input, told as the assistant's
/// note of it.
pub(crate) fn orphan_note(item: &Map<String, Value>, id: &str) -> Map<String, Value> {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("tool");
    let id = if id.is_empty() { "unknown" } else { id };
    let mut text = match item.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "null".to_owned(),
    };
    if text.len() > MAX_NOTE {
        let mut end = MAX_NOTE;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n...[truncated]");
    }
    let mut note = Map::new();
    note.insert("type".to_owned(), Value::String("message".to_owned()));
    note.insert("role".to_owned(), Value::String("assistant".to_owned()));
    note.insert(
        "content".to_owned(),
        Value::String(format!("[Previous {name} result; call_id={id}]: {text}")),
    );
    note
}

/// The key of the conversation a request belongs to, for a client that
/// names none: a UUID-shaped SHA-256 of the client's instructions and the
/// input up to its first user message, which every later turn repeats.
/// Items are hashed as JSON with sorted keys. None without a user message.
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
    let hex = payload_hash(hashed.as_bytes());
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// Writes `value` as JSON with object keys sorted.
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
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical(&object[key], out);
            }
            out.push('}');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The first non-blank line of `text`, trimmed.
fn first_line(text: &str) -> &str {
    let text = text.trim();
    text.split('\n').next().unwrap_or("")
}

/// The web_search tool declared for a replayed search: the cached one,
/// which fetches nothing live.
fn cached_search() -> Value {
    json::json!({"type": "web_search", "external_web_access": false})
}

/// Declares the web_search tool when the input replays a web search and
/// nothing declares one; the backend turns such a request away otherwise.
/// A request whose tools ride in an `additional_tools` item, or that asks
/// for reasoning context `all_turns`, gets it there. A request that offered
/// no tools then calls none.
pub(crate) fn declare_search(body: &mut Map<String, Value>) -> bool {
    let Some(Value::Array(input)) = body.get("input") else {
        return false;
    };
    let mut searched = false;
    let mut offered = false;
    let mut additional = None;
    for (index, item) in input.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("web_search_call") => searched = true,
            Some("additional_tools") => {
                let tools = item
                    .get("tools")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice);
                if declares_search(tools) {
                    return false;
                }
                offered = offered || !tools.is_empty();
                additional = additional.or(Some(index));
            }
            _ => {}
        }
    }
    if !searched {
        return false;
    }
    let tools = match body.get("tools") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(tools)) => tools.as_slice(),
        Some(_) => return false,
    };
    if declares_search(tools) {
        return false;
    }
    offered = offered || !tools.is_empty();
    let all_turns = body
        .get("reasoning")
        .and_then(|reasoning| reasoning.get("context"))
        .and_then(Value::as_str)
        == Some("all_turns");
    match additional {
        Some(index) => {
            if let Some(Value::Array(input)) = body.get_mut("input")
                && let Some(Value::Object(item)) = input.get_mut(index)
            {
                match item.get_mut("tools") {
                    Some(Value::Array(tools)) => tools.push(cached_search()),
                    _ => {
                        item.insert("tools".to_owned(), Value::Array(vec![cached_search()]));
                    }
                }
            }
        }
        None if all_turns => {
            if let Some(Value::Array(input)) = body.get_mut("input") {
                input.insert(
                    0,
                    json::json!({
                        "type": "additional_tools",
                        "role": "developer",
                        "tools": [cached_search()],
                    }),
                );
            }
        }
        None => {
            let mut tools = match body.remove("tools") {
                Some(Value::Array(tools)) => tools,
                _ => Vec::new(),
            };
            tools.push(cached_search());
            body.insert("tools".to_owned(), Value::Array(tools));
        }
    }
    if !offered {
        let choice = body.get("tool_choice");
        let none = match choice {
            None | Some(Value::Null) => true,
            Some(Value::String(choice)) => matches!(choice.as_str(), "" | "auto" | "none"),
            Some(_) => false,
        };
        if none {
            body.insert("tool_choice".to_owned(), Value::String("none".to_owned()));
        }
    }
    true
}

/// Says `tools` has a web search: the hosted one by any of its names, or
/// Codex's own `web.run`, a function in the `web` namespace.
pub(crate) fn declares_search(tools: &[Value]) -> bool {
    tools.iter().any(|tool| {
        let kind = tool.get("type").and_then(Value::as_str).unwrap_or("");
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        if kind.starts_with("web_search") || (kind == "function" && name == "web.run") {
            return true;
        }
        kind == "namespace"
            && name == "web"
            && tool
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|inner| {
                    inner.iter().any(|function| {
                        function.get("type").and_then(Value::as_str) == Some("function")
                            && function.get("name").and_then(Value::as_str) == Some("run")
                    })
                })
    })
}
