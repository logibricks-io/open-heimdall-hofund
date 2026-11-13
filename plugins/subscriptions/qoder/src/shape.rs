//! Chat Completions to the native agent request body.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::uuid;

use crate::cosy::digest_hex;
use crate::models::{ModelInfo, model_config};

const SYSTEM: &str = "You are a Qoder agent. Use the instructions below and the tools available to you to assist the user.";
const SMALL_BUDGET: u64 = 1024;
const RANK: &[&str] = &[
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

pub(crate) struct Body {
    pub value: Value,
    pub task_key: String,
}

#[derive(Clone)]
struct Task {
    id: String,
    name: String,
    begin: u64,
    answered: bool,
}

#[derive(Default)]
pub(crate) struct Tasks {
    entries: Mutex<HashMap<String, Task>>,
}

impl Tasks {
    pub(crate) fn body(
        &self,
        chat: &Value,
        model: &ModelInfo,
        now: SystemTime,
    ) -> Result<Body, PluginError> {
        let session = session_of(chat);
        let messages = chat.get("messages").and_then(Value::as_array);
        let last_user =
            messages.and_then(|items| {
                items.iter().enumerate().rev().find(|(_, message)| {
                    message.get("role").and_then(Value::as_str) == Some("user")
                })
            });
        let last_content = last_user
            .map(|(_, message)| message["content"].to_string())
            .unwrap_or_default();
        let task_key = digest_hex(&format!(
            "{session}\0{}\0{last_content}",
            last_user.map_or(0, |(index, _)| index),
        ));
        let millis = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let task = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&task_key)
            .cloned()
            .unwrap_or_else(|| Task {
                id: uuid::v4().unwrap_or_else(|_| session.replace('-', "")),
                name: last_user.map_or_else(String::new, |(_, message)| {
                    text_of(&message["content"]).chars().take(10).collect()
                }),
                begin: millis,
                answered: false,
            });
        let request =
            uuid::v4().map_err(|_| PluginError::internal("the system random source failed"))?;
        let value = build(chat, model, &session, &request, &task)?;
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(task_key.clone(), task);
        Ok(Body { value, task_key })
    }

    pub(crate) fn answered(&self, key: &str) {
        if let Some(task) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(key)
        {
            task.answered = true;
        }
    }
}

fn build(
    chat: &Value,
    model: &ModelInfo,
    session: &str,
    request: &str,
    task: &Task,
) -> Result<Value, PluginError> {
    let messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let system = messages
        .iter()
        .filter(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        })
        .map(|message| text_of(&message["content"]))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut system = if system.is_empty() {
        SYSTEM.to_owned()
    } else {
        format!("{SYSTEM}\n\n{system}")
    };
    let tools = chat
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter(|tool| {
                    tool.get("type").and_then(Value::as_str) == Some("function")
                        && tool
                            .pointer("/function/name")
                            .and_then(Value::as_str)
                            .is_some()
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let choice = chat
        .get("tool_choice")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tools = if choice == "none" { Vec::new() } else { tools };
    if choice == "required" && !tools.is_empty() {
        system.push_str("\nYou must call an available function in this response.");
    }
    let max_tokens = chat
        .get("max_completion_tokens")
        .or_else(|| chat.get("max_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(32_000);
    let (thinking, effort) = effort_for(
        chat.get("reasoning_effort").and_then(Value::as_str),
        model,
        max_tokens,
    );
    let mut parameters = json!({"enable_thinking": thinking, "max_tokens": max_tokens});
    if !effort.is_empty() {
        parameters["reasoning_effort"] = Value::String(effort);
    }
    if let Some(window) = window_for(chat, model, max_tokens) {
        parameters["context_length"] = Value::from(window);
    }
    let sys = json!({"type": "text", "text": system});
    let mut body = json!({
        "request_id": request,
        "request_set_id": task.id,
        "chat_record_id": request,
        "session_id": session,
        "parameters": parameters,
        "business": {
            "product": "app", "version": "1.1.69", "type": "agent", "id": task.id,
            "name": task.name, "begin_at": task.begin,
            "stage": if task.answered { "processing" } else { "start" },
            "model_key": model.key,
        },
        "agent_id": "agent_common",
        "task_id": "common",
        "session_type": "app",
        "model_config": model_config(model, thinking)?,
        "system": [sys.clone()],
        "messages": Vec::<Value>::new(),
    });
    let mut native = vec![json!({"role": "system", "content": [sys]})];
    native.extend(cache_marked(qoder_messages(&messages)));
    body["messages"] = Value::Array(native);
    if !tools.is_empty() {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool["function"]["name"],
                            "description": tool["function"].get("description").and_then(Value::as_str).unwrap_or(""),
                            "parameters": typed_unions(tool["function"].get("parameters").unwrap_or(&Value::Null)),
                        }
                    })
                })
                .collect(),
        );
    }
    Ok(body)
}

fn effort_for(asked: Option<&str>, model: &ModelInfo, budget: u64) -> (bool, String) {
    let mut want = asked.unwrap_or("").to_ascii_lowercase();
    if !RANK.contains(&want.as_str()) {
        want.clear();
    }
    if want.is_empty() && budget > 0 && budget <= SMALL_BUDGET {
        want = "none".to_owned();
    }
    if !model.thinks || (want == "none" && !model.always_thinks) {
        return (false, String::new());
    }
    if model.efforts.is_empty() {
        return (true, String::new());
    }
    if want.is_empty() {
        return (true, model.default_effort.clone());
    }
    if want == "none" {
        return (true, model.efforts[0].clone());
    }
    if want == "ultra" && !model.efforts.contains(&want) {
        want = "max".to_owned();
    }
    if model.efforts.contains(&want) {
        return (true, want);
    }
    let at = RANK
        .iter()
        .position(|level| *level == want)
        .unwrap_or(RANK.len());
    let mut best = want;
    let mut distance = RANK.len();
    for effort in &model.efforts {
        let Some(index) = RANK.iter().position(|level| level == effort) else {
            continue;
        };
        let difference = index.abs_diff(at);
        if difference < distance || (difference == distance && index > at) {
            best = effort.clone();
            distance = difference;
        }
    }
    (true, best)
}

fn window_for(chat: &Value, model: &ModelInfo, reply: u64) -> Option<u64> {
    if model.windows.is_empty() {
        return (model.context > 0).then_some(model.context);
    }
    let base = if model.default_window > 0 {
        model.default_window
    } else {
        model
            .raw
            .get("max_input_tokens")
            .and_then(Value::as_u64)
            .filter(|tokens| *tokens > 0)
            .unwrap_or(model.context)
    };
    let input = json!({"m": chat.get("messages"), "t": chat.get("tools")})
        .to_string()
        .len() as u64;
    let need = input.div_ceil(3).saturating_add(reply);
    if base > 0 && need <= base {
        return Some(base);
    }
    model
        .windows
        .iter()
        .copied()
        .find(|window| *window >= need)
        .or_else(|| model.windows.last().copied())
}

fn session_of(chat: &Value) -> String {
    let seed = chat
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(|key| format!("key\0{key}"))
        .or_else(|| {
            chat.get("user")
                .and_then(Value::as_str)
                .map(|user| format!("user\0{user}"))
        })
        .unwrap_or_else(|| {
            let messages = chat
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let system: Vec<String> = messages
                .iter()
                .filter(|message| {
                    matches!(
                        message.get("role").and_then(Value::as_str),
                        Some("system" | "developer")
                    )
                })
                .map(|message| text_of(&message["content"]))
                .collect();
            let first = messages
                .iter()
                .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
                .map(|message| message["content"].clone())
                .unwrap_or(Value::String(String::new()));
            format!("chat\0{}", json!([system, first]))
        });
    let hash = digest_hex(&seed);
    format!(
        "{}-{}-{}-{}-{}",
        &hash[..8],
        &hash[8..12],
        &hash[12..16],
        &hash[16..20],
        &hash[20..32]
    )
}

fn qoder_messages(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = Vec::new();
    let mut turn: Option<(Vec<String>, HashSet<String>)> = None;
    for (index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if matches!(role, "system" | "developer") {
            continue;
        }
        if role == "user" {
            turn = None;
        }
        if role != "user" && role != "tool" {
            show_seen(&mut out, &mut seen);
        }
        if role == "tool" {
            let text = text_of(&message["content"]);
            let images = image_blocks(&message["content"]);
            let named = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let answer = turn.as_mut().and_then(|(open, kept)| {
                let at = open
                    .iter()
                    .position(|id| id == named)
                    .or_else(|| open.iter().position(|id| !kept.contains(id)))?;
                Some(open.remove(at))
            });
            let Some(answer) = answer else {
                as_text(&mut seen, &text, images);
                continue;
            };
            if !images.is_empty() {
                seen.push(json!({"type": "text", "text": format!("[From the result of tool call {answer}:]")}));
                seen.extend(images);
                seen.push(json!({"type": "text", "text": "[The tool returned images; they follow in the next message.]"}));
            }
            out.push(json!({"role": "tool", "tool_call_id": answer, "content": text}));
            continue;
        }
        if role == "assistant"
            && let Some(calls) = message.get("tool_calls").and_then(Value::as_array)
            && !calls.is_empty()
        {
            let calls: Vec<Value> = calls
                .iter()
                .enumerate()
                .map(|(call_index, call)| {
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| call_id(index, call_index, call));
                    json!({
                        "id": id, "type": "function",
                        "function": {
                            "name": call["function"]["name"],
                            "arguments": call["function"].get("arguments").map(|args| {
                                args.as_str().map(str::to_owned).unwrap_or_else(|| args.to_string())
                            }).unwrap_or_else(|| "{}".to_owned()),
                        }
                    })
                })
                .collect();
            let mut assistant = json!({
                "role": "assistant", "content": text_of(&message["content"]), "tool_calls": calls,
            });
            add_reasoning(&mut assistant, message);
            out.push(assistant);
            let open: Vec<String> = calls
                .iter()
                .filter_map(|call| call.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect();
            let kept = messages
                .iter()
                .skip(index + 1)
                .take_while(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
                .filter_map(|message| {
                    message
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .filter(|id| open.contains(id))
                .collect();
            turn = Some((open, kept));
            continue;
        }
        let mut blocks = blocks_of(&message["content"]);
        if role == "assistant" {
            blocks.retain(|block| block.get("type").and_then(Value::as_str) == Some("text"));
        }
        if blocks.is_empty() {
            continue;
        }
        if role == "user" && !seen.is_empty() {
            blocks.splice(0..0, seen.drain(..));
        }
        let mut native = json!({"role": role, "content": blocks});
        if role == "assistant" {
            add_reasoning(&mut native, message);
        }
        out.push(native);
        turn = None;
    }
    show_seen(&mut out, &mut seen);
    out
}

fn show_seen(out: &mut Vec<Value>, seen: &mut Vec<Value>) {
    if !seen.is_empty() {
        out.push(json!({"role": "user", "content": seen}));
        seen.clear();
    }
}

fn as_text(seen: &mut Vec<Value>, text: &str, images: Vec<Value>) {
    if text.trim().is_empty() && images.is_empty() {
        return;
    }
    if !seen.is_empty() {
        seen.push(json!({"type": "text", "text": "\n\n"}));
    }
    if !text.trim().is_empty() {
        seen.push(json!({"type": "text", "text": text.trim()}));
    }
    seen.extend(images);
}

fn call_id(at: usize, index: usize, call: &Value) -> String {
    format!(
        "call_{}",
        &digest_hex(&format!(
            "{at}\0{index}\0{}\0{}",
            call["function"]["name"], call["function"]["arguments"]
        ))[..32]
    )
}

fn add_reasoning(native: &mut Value, message: &Value) {
    let mut reasoning = message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if reasoning.is_empty() {
        reasoning = message
            .get("reasoning_details")
            .and_then(Value::as_array)
            .map(|details| {
                details
                    .iter()
                    .filter_map(|detail| {
                        detail
                            .get("text")
                            .or_else(|| detail.get("summary"))
                            .and_then(Value::as_str)
                    })
                    .collect()
            })
            .unwrap_or_default();
    }
    if !reasoning.trim().is_empty() {
        native["reasoning_content"] = Value::String(reasoning);
    }
}

fn blocks_of(content: &Value) -> Vec<Value> {
    if let Some(text) = content.as_str() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type": "text", "text": text})]
        };
    }
    content
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => part
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|text| json!({"type": "text", "text": text})),
                    Some("image_url") => image_block(part),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn image_blocks(content: &Value) -> Vec<Value> {
    content
        .as_array()
        .map(|parts| parts.iter().filter_map(image_block).collect())
        .unwrap_or_default()
}

fn image_block(part: &Value) -> Option<Value> {
    if part.get("type").and_then(Value::as_str) != Some("image_url") {
        return None;
    }
    let url = part.get("image_url").and_then(|image| {
        image
            .as_str()
            .map(str::to_owned)
            .or_else(|| image.get("url").and_then(Value::as_str).map(str::to_owned))
    })?;
    Some(json!({"type": "image_url", "image_url": {"url": url}}))
}

fn text_of(content: &Value) -> String {
    content.as_str().map(str::to_owned).unwrap_or_else(|| {
        blocks_of(content)
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect()
    })
}

fn cache_marked(messages: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = messages
        .into_iter()
        .map(|mut message| {
            if message.get("role").and_then(Value::as_str) == Some("user")
                && message.get("content").and_then(Value::as_array).is_some()
            {
                message["contents"] = message["content"].clone();
            }
            message
        })
        .collect();
    let Some(last) = out.last_mut() else {
        return out;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return out;
    }
    let Some(blocks) = last.get("content").and_then(Value::as_array) else {
        return out;
    };
    let Some(index) = blocks.iter().rposition(|block| {
        !matches!(
            block.get("type").and_then(Value::as_str),
            Some("thinking" | "redacted_thinking" | "tool_use" | "tool_result")
        )
    }) else {
        return out;
    };
    let mut blocks = blocks.clone();
    blocks[index]["cache_control"] = json!({"type": "ephemeral"});
    last["content"] = Value::Array(blocks.clone());
    last["contents"] = Value::Array(blocks);
    out
}

fn typed_unions(schema: &Value) -> Value {
    let mut root = schema.clone();
    if schema.is_null() {
        return json!({"type": "object", "properties": {}});
    }
    let snapshot = root.clone();
    type_unions_at(&mut root, &snapshot, 0);
    root
}

fn type_unions_at(value: &mut Value, root: &Value, depth: usize) {
    if depth > 64 {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if !object.contains_key("type")
        && object
            .get("oneOf")
            .and_then(Value::as_array)
            .is_some_and(|branches| {
                !branches.is_empty()
                    && branches
                        .iter()
                        .all(|branch| object_schema(branch, root, &mut Vec::new()))
            })
    {
        object.insert("type".to_owned(), Value::String("object".to_owned()));
    }
    for child in object.values_mut() {
        match child {
            Value::Object(_) => type_unions_at(child, root, depth + 1),
            Value::Array(items) => {
                for item in items {
                    type_unions_at(item, root, depth + 1);
                }
            }
            _ => {}
        }
    }
}

fn object_schema(value: &Value, root: &Value, seen: &mut Vec<String>) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if let Some(kind) = object.get("type") {
        return kind.as_str() == Some("object");
    }
    let Some(reference) = object.get("$ref").and_then(Value::as_str) else {
        return false;
    };
    if seen.contains(&reference.to_owned()) || seen.len() > 16 || !reference.starts_with("#/") {
        return false;
    }
    let Some(target) = reference
        .strip_prefix("#/")
        .and_then(|path| pointer(root, path))
    else {
        return false;
    };
    seen.push(reference.to_owned());
    object_schema(target, root, seen)
}

fn pointer<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for part in path.split('/') {
        current = current.get(part.replace("~1", "/").replace("~0", "~"))?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::{Tasks, cache_marked, session_of, typed_unions};
    use hofund_plugin_sdk::json::json;

    #[test]
    fn r54b_cache_marks_only_the_last_user_block() {
        let messages = cache_marked(vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
        ]);
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(messages[0]["content"], messages[0]["contents"]);
    }

    #[test]
    fn r54b_sessions_are_stable_and_do_not_expose_prompt_keys() {
        let chat = json!({"prompt_cache_key": "secret-client-key", "messages": []});
        assert_eq!(session_of(&chat), session_of(&chat));
        assert!(!session_of(&chat).contains("secret-client-key"));
    }

    #[test]
    fn r54b_object_oneof_gets_a_type() {
        let schema = typed_unions(
            &json!({"oneOf": [{"type": "object"}, {"$ref": "#/$defs/X"}], "$defs": {"X": {"type": "object"}}}),
        );
        assert_eq!(schema["type"], "object");
    }

    #[test]
    fn r54b_unknown_result_id_does_not_take_an_explicitly_named_call() {
        let messages = vec![
            json!({"role": "user", "content": "both"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                {"id": "b", "type": "function", "function": {"name": "g", "arguments": "{}"}}
            ]}),
            json!({"role": "tool", "tool_call_id": "lost", "content": "B"}),
            json!({"role": "tool", "tool_call_id": "a", "content": "A"}),
        ];
        let shaped = super::qoder_messages(&messages);
        let results: Vec<_> = shaped
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| {
                (
                    message["tool_call_id"].as_str().expect("result id"),
                    message["content"].as_str().expect("result content"),
                )
            })
            .collect();
        assert_eq!(results, [("b", "B"), ("a", "A")]);
    }

    #[test]
    fn r54b_images_reasoning_effort_and_history_reach_the_native_body() {
        let models =
            crate::models::parse_list(include_bytes!("fixtures/models.json")).expect("models");
        let model = models
            .iter()
            .find(|model| model.key == "qfmodel")
            .expect("model");
        let chat = json!({
            "max_tokens": 4000,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "inspect"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}]},
                {"role": "assistant", "content": "Picked.", "reasoning_content": "think first"},
                {"role": "assistant", "content": "", "reasoning_content": "call next", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"},
                {"role": "user", "content": "continue"}
            ]
        });
        let body = Tasks::default()
            .body(&chat, model, std::time::SystemTime::UNIX_EPOCH)
            .expect("body");
        assert_eq!(body.value["model_config"]["is_reasoning"], true);
        assert_eq!(body.value["parameters"]["reasoning_effort"], "high");
        assert_eq!(body.value["messages"][1]["content"][1]["type"], "image_url");
        assert_eq!(
            body.value["messages"][2]["reasoning_content"],
            "think first"
        );
        assert_eq!(body.value["messages"][3]["reasoning_content"], "call next");
        assert_eq!(body.value["messages"][4]["role"], "tool");
    }
}
