//! Ollama's `/api/chat` and `/api/generate` requests as Chat Completions
//! requests.

use hofund_plugin_sdk::json::{Map, Value, json};

/// The options a Chat Completions API takes. The rest (`num_ctx`, `top_k`,
/// `repeat_penalty`, ...) belong to a local runner and are dropped.
const PASSED_OPTIONS: [&str; 6] = [
    "temperature",
    "top_p",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "stop",
];

/// `text` of `value[key]`, or empty.
pub(crate) fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Whether `value[key]` is missing or `null`.
fn absent(value: &Value, key: &str) -> bool {
    value.get(key).is_none_or(Value::is_null)
}

/// Whether the request has nothing to say: a client sends one to load the
/// model.
pub(crate) fn is_empty(request: &Value, generate: bool) -> bool {
    if generate {
        text(request, "prompt").is_empty() && images(request).is_empty()
    } else {
        request
            .get("messages")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    }
}

/// Whether the client turned thinking off with `"think": false`.
pub(crate) fn thinking_off(request: &Value) -> bool {
    request.get("think") == Some(&Value::Bool(false))
}

/// `request` as a streamed Chat Completions request for `model`.
///
/// # Errors
///
/// Returns the message Ollama's API answers `400` with.
pub(crate) fn chat_body(request: &Value, model: &str, generate: bool) -> Result<Value, String> {
    let messages = if generate {
        let mut out = Vec::new();
        let system = text(request, "system");
        if !system.is_empty() {
            out.push(json!({"role": "system", "content": system}));
        }
        out.push(json!({
            "role": "user",
            "content": content(text(request, "prompt"), &images(request)),
        }));
        out
    } else {
        let list = match request.get("messages") {
            None | Some(Value::Null) => &Vec::new(),
            Some(Value::Array(list)) => list,
            Some(_) => return Err("invalid request: messages is not an array".to_owned()),
        };
        messages(list)?
    };
    let mut out = Map::new();
    out.insert("model".to_owned(), json!(model));
    out.insert("messages".to_owned(), Value::Array(messages));
    out.insert("stream".to_owned(), Value::Bool(true));
    out.insert("stream_options".to_owned(), json!({"include_usage": true}));
    if !generate && !absent(request, "tools") {
        // Ollama's tools are Chat Completions' own shape.
        out.insert("tools".to_owned(), request["tools"].clone());
    }
    if let Some(format) = response_format(request.get("format"))? {
        out.insert("response_format".to_owned(), format);
    }
    if let Some(options) = request.get("options").and_then(Value::as_object) {
        for key in PASSED_OPTIONS {
            if let Some(value) = options.get(key).filter(|value| !value.is_null()) {
                out.insert(key.to_owned(), value.clone());
            }
        }
        // -1 is no limit and -2 fills the window; a Chat API says either by
        // leaving max_tokens out.
        if let Some(limit) = options.get("num_predict").and_then(Value::as_f64)
            && limit >= 1.0
        {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive token count; a fraction is cut as Ollama does"
            )]
            let limit = limit as u64;
            out.insert("max_tokens".to_owned(), json!(limit));
        }
    }
    match request.get("think") {
        None | Some(Value::Null | Value::Bool(_)) => {}
        Some(Value::String(effort)) => {
            if !effort.is_empty() {
                out.insert("reasoning_effort".to_owned(), json!(effort));
            }
        }
        Some(other) => return Err(format!("invalid think value: {other}")),
    }
    Ok(Value::Object(out))
}

/// `format`: `"json"`, a JSON schema, or nothing.
fn response_format(format: Option<&Value>) -> Result<Option<Value>, String> {
    match format {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) if name.is_empty() => Ok(None),
        Some(Value::String(name)) if name == "json" => Ok(Some(json!({"type": "json_object"}))),
        Some(schema @ Value::Object(_)) => Ok(Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "response", "schema": schema},
        }))),
        Some(other) => Err(format!("invalid format: {other}")),
    }
}

/// Ollama's messages in Chat Completions' shape. A tool call with no id gets
/// one. A tool result names its call by id, else by the tool's name, else it
/// takes the oldest open call.
fn messages(list: &[Value]) -> Result<Vec<Value>, String> {
    let mut open: Vec<(String, String)> = Vec::new();
    let mut out = Vec::with_capacity(list.len());
    for (index, message) in list.iter().enumerate() {
        let role = text(message, "role");
        let body = text(message, "content");
        match role.to_ascii_lowercase().as_str() {
            lower @ ("system" | "developer") => out.push(json!({"role": lower, "content": body})),
            "user" => out.push(json!({"role": "user", "content": content(body, &images(message))})),
            "assistant" => out.push(assistant(message, index, &mut open)),
            "tool" => {
                let named = text(message, "tool_call_id");
                let name = text(message, "tool_name");
                let pick = open
                    .iter()
                    .position(|(id, call)| {
                        if named.is_empty() {
                            !name.is_empty() && call == name
                        } else {
                            id == named
                        }
                    })
                    .or_else(|| (named.is_empty() && !open.is_empty()).then_some(0));
                let id = match pick {
                    Some(at) => open.remove(at).0,
                    None => named.to_owned(),
                };
                if id.is_empty() {
                    return Err(format!(
                        "messages[{index}]: a tool result with no tool call before it"
                    ));
                }
                out.push(json!({"role": "tool", "tool_call_id": id, "content": body}));
                let pictures = images(message);
                if !pictures.is_empty() {
                    // A Chat tool message carries text alone; its images go
                    // in a user message after it.
                    out.push(json!({"role": "user", "content": content("", &pictures)}));
                }
            }
            _ => return Err(format!("messages[{index}]: unknown role {role:?}")),
        }
    }
    Ok(out)
}

/// An assistant message, opening its tool calls for the results after it.
fn assistant(message: &Value, index: usize, open: &mut Vec<(String, String)>) -> Value {
    let mut out = Map::new();
    out.insert("role".to_owned(), json!("assistant"));
    out.insert("content".to_owned(), json!(text(message, "content")));
    let thinking = text(message, "thinking");
    if !thinking.is_empty() {
        out.insert("reasoning_content".to_owned(), json!(thinking));
    }
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .filter(|calls| !calls.is_empty());
    if let Some(calls) = calls {
        open.clear();
        let mut wire = Vec::with_capacity(calls.len());
        for (position, call) in calls.iter().enumerate() {
            let id = match text(call, "id") {
                "" => format!("call_{index}_{position}"),
                id => id.to_owned(),
            };
            let function = call.get("function").unwrap_or(&Value::Null);
            let name = text(function, "name");
            let arguments = match function.get("arguments") {
                None | Some(Value::Null) => "{}".to_owned(),
                // Already a JSON string, as a Chat client sends them.
                Some(Value::String(raw)) => raw.clone(),
                Some(other) => other.to_string(),
            };
            wire.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            }));
            open.push((id, name.to_owned()));
        }
        out.insert("tool_calls".to_owned(), Value::Array(wire));
        if text(message, "content").is_empty() {
            out.insert("content".to_owned(), Value::Null);
        }
    }
    Value::Object(out)
}

/// The base64 images `value["images"]` holds.
fn images(value: &Value) -> Vec<&str> {
    value
        .get("images")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// A message's text with its images, which Ollama sends as bare base64, as
/// Chat Completions content parts with data URLs.
fn content(text: &str, images: &[&str]) -> Value {
    if images.is_empty() {
        return json!(text);
    }
    let mut parts = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    for image in images {
        let url = if image.starts_with("data:") {
            (*image).to_owned()
        } else {
            format!("data:{};base64,{image}", image_type(image))
        };
        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
    }
    Value::Array(parts)
}

/// The media type of a base64 image, read from how its first bytes encode.
fn image_type(base64: &str) -> &'static str {
    if base64.starts_with("/9j/") {
        "image/jpeg"
    } else if base64.starts_with("R0lGOD") {
        "image/gif"
    } else if base64.starts_with("UklGR") && base64.get(11..16) == Some("XRUJQ") {
        "image/webp"
    } else {
        "image/png"
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::{Value, json};

    use super::{chat_body, image_type, is_empty, thinking_off};

    fn chat(request: &Value) -> Value {
        chat_body(request, "seer", false).expect("chat body")
    }

    #[test]
    fn a_chat_becomes_a_streamed_chat_completion() {
        let body = chat(&json!({
            "model": "seer:latest",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "Developer", "content": "rules"},
                {"role": "user", "content": "hi"},
            ],
            "stream": false,
        }));
        assert_eq!(
            body,
            json!({
                "model": "seer",
                "messages": [
                    {"role": "system", "content": "be brief"},
                    {"role": "developer", "content": "rules"},
                    {"role": "user", "content": "hi"},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
            })
        );
    }

    #[test]
    fn options_tools_format_and_think_carry_over() {
        let tools = json!([{"type": "function", "function": {"name": "read_file"}}]);
        let body = chat(&json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": tools,
            "format": "json",
            "think": "high",
            "options": {
                "temperature": 0.2, "top_p": 0.9, "seed": 7, "presence_penalty": 0.1,
                "frequency_penalty": 0.3, "stop": ["END"], "num_predict": 64.9,
                "num_ctx": 8192, "top_k": 40, "repeat_penalty": 1.1, "seed_extra": null,
            },
        }));
        assert_eq!(body["tools"], tools);
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["seed"], 7);
        assert_eq!(body["presence_penalty"], 0.1);
        assert_eq!(body["frequency_penalty"], 0.3);
        assert_eq!(body["stop"], json!(["END"]));
        assert_eq!(body["max_tokens"], 64);
        for dropped in [
            "num_ctx",
            "top_k",
            "repeat_penalty",
            "seed_extra",
            "num_predict",
        ] {
            assert!(body.get(dropped).is_none(), "{dropped}");
        }
        let schema = json!({"type": "object"});
        let body = chat(&json!({"messages": [], "format": schema, "options": {"num_predict": -1}}));
        assert_eq!(
            body["response_format"],
            json!({"type": "json_schema", "json_schema": {"name": "response", "schema": schema}})
        );
        assert!(body.get("max_tokens").is_none());
        let body = chat(
            &json!({"messages": [], "format": "", "think": true, "tools": null,
            "options": {"num_predict": 0.5, "temperature": null}}),
        );
        for absent in [
            "response_format",
            "reasoning_effort",
            "tools",
            "max_tokens",
            "temperature",
        ] {
            assert!(body.get(absent).is_none(), "{absent}");
        }
        let body = chat(&json!({"messages": [], "think": ""}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn bad_fields_are_refused() {
        let refuse = |request: Value| chat_body(&request, "seer", false).expect_err("refused");
        assert_eq!(
            refuse(json!({"format": "yaml"})),
            r#"invalid format: "yaml""#
        );
        assert_eq!(refuse(json!({"format": 3})), "invalid format: 3");
        assert_eq!(refuse(json!({"think": 2})), "invalid think value: 2");
        assert_eq!(
            refuse(json!({"messages": {"role": "user"}})),
            "invalid request: messages is not an array"
        );
        assert_eq!(
            refuse(json!({"messages": [{"role": "robot", "content": "x"}]})),
            r#"messages[0]: unknown role "robot""#
        );
        assert_eq!(
            refuse(json!({"messages": [{"role": "tool", "content": "x"}]})),
            "messages[0]: a tool result with no tool call before it"
        );
    }

    #[test]
    fn tool_calls_get_ids_and_results_find_their_call() {
        let body = chat(&json!({"messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "thinking": "plan", "tool_calls": [
                {"function": {"name": "read", "arguments": {"path": "a"}}},
                {"id": "own", "function": {"name": "list", "arguments": "{\"dir\":\".\"}"}},
                {"function": {"name": "stat"}},
            ]},
            {"role": "tool", "tool_name": "list", "content": "files"},
            {"role": "tool", "tool_call_id": "call_1_2", "content": "size"},
            {"role": "tool", "content": "text", "images": ["iVBORw0KGgo"]},
            {"role": "assistant", "content": "done", "tool_calls": [{"function": {"name": "x"}}]},
            {"role": "tool", "tool_call_id": "elsewhere", "content": "kept"},
        ]}));
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(
            messages[1],
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "plan",
                "tool_calls": [
                    {"id": "call_1_0", "type": "function",
                     "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}},
                    {"id": "own", "type": "function",
                     "function": {"name": "list", "arguments": "{\"dir\":\".\"}"}},
                    {"id": "call_1_2", "type": "function",
                     "function": {"name": "stat", "arguments": "{}"}},
                ],
            })
        );
        assert_eq!(
            messages[2],
            json!({"role": "tool", "tool_call_id": "own", "content": "files"})
        );
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_1_2", "content": "size"})
        );
        assert_eq!(
            messages[4],
            json!({"role": "tool", "tool_call_id": "call_1_0", "content": "text"})
        );
        assert_eq!(
            messages[5],
            json!({"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo"}},
            ]})
        );
        assert_eq!(messages[6]["content"], "done");
        assert_eq!(messages[6]["tool_calls"][0]["id"], "call_5_0");
        assert_eq!(
            messages[7],
            json!({"role": "tool", "tool_call_id": "elsewhere", "content": "kept"})
        );
    }

    #[test]
    fn generate_sends_system_and_prompt_with_images() {
        let body = chat_body(
            &json!({"system": "terse", "prompt": "what is this", "images": ["/9j/4AAQ", "data:image/gif;base64,R0"],
                "messages": [{"role": "user", "content": "ignored"}], "tools": [{}]}),
            "seer",
            true,
        )
        .expect("generate body");
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "terse"},
                {"role": "user", "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/4AAQ"}},
                    {"type": "image_url", "image_url": {"url": "data:image/gif;base64,R0"}},
                ]},
            ])
        );
        assert!(body.get("tools").is_none());
        let body = chat_body(&json!({"prompt": "hi"}), "seer", true).expect("bare prompt");
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
    }

    #[test]
    fn images_are_typed_by_their_first_bytes() {
        assert_eq!(image_type("/9j/4AAQSkZJRg"), "image/jpeg");
        assert_eq!(image_type("R0lGODlhAQABAIAAAP"), "image/gif");
        assert_eq!(image_type("UklGRiQAAABXRUJQVlA4"), "image/webp");
        assert_eq!(image_type("UklGRiQAAABXQVZFZm10"), "image/png");
        assert_eq!(image_type("iVBORw0KGgoAAAANSUhEUg"), "image/png");
        assert_eq!(image_type("UklGR"), "image/png");
    }

    #[test]
    fn empty_requests_and_thinking_off_are_seen() {
        assert!(is_empty(&json!({"prompt": ""}), true));
        assert!(!is_empty(&json!({"prompt": "x"}), true));
        assert!(!is_empty(&json!({"images": ["iVBOR"]}), true));
        assert!(is_empty(&json!({}), false));
        assert!(is_empty(&json!({"messages": []}), false));
        assert!(!is_empty(&json!({"messages": [{"role": "user"}]}), false));
        assert!(thinking_off(&json!({"think": false})));
        assert!(!thinking_off(&json!({"think": true})));
        assert!(!thinking_off(&json!({})));
    }
}
