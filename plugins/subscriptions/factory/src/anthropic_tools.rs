//! Factory Anthropic Messages tool-schema wrapping and response unwrap.
//!
//! Factory rejects root-level `anyOf` / `oneOf` / `allOf` on tool `input_schema`.
//! The original schema rides under one `arguments` property; replies and history
//! hide that wire-only envelope from the caller.

use std::collections::{HashMap, HashSet, VecDeque};

use hofund_plugin_api::{
    BodyStream, Completion, ContentPart, PluginError, Request, Role, ToolArguments,
};
use hofund_plugin_sdk::bytes::Bytes;
use hofund_plugin_sdk::futures::unfold;
use hofund_plugin_sdk::json::{Map, Value, json};

const ARGUMENT: &str = "arguments";
const COMPOSITIONS: &[&str] = &["anyOf", "oneOf", "allOf"];
const SCHEMA_MAPS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];
const SCHEMA_LISTS: &[&str] = &["anyOf", "oneOf", "allOf", "prefixItems"];
const SCHEMA_VALUES: &[&str] = &[
    "additionalProperties",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "items",
    "additionalItems",
    "unevaluatedItems",
    "not",
    "if",
    "then",
    "else",
    "contentSchema",
];

fn has_composition_root(schema: &Map<String, Value>) -> bool {
    COMPOSITIONS.iter().any(|key| schema.contains_key(*key))
}

/// Wraps composed tool schemas and prior assistant tool inputs before encoding.
pub(crate) fn prepare_request(
    mut request: Request,
) -> Result<(HashSet<String>, Request), PluginError> {
    validate_request_tools(&request)?;
    let mut names = HashSet::new();
    for tool in &mut request.tools {
        let Value::Object(schema) = &mut tool.input_schema else {
            continue;
        };
        if !has_composition_root(schema) {
            continue;
        }
        let wrapped = wrap_tool_schema(schema)?;
        tool.input_schema = Value::Object(wrapped);
        names.insert(tool.name.clone());
    }
    if names.is_empty() {
        return Ok((names, request));
    }
    for message in &mut request.messages {
        if message.role != Role::Assistant {
            continue;
        }
        for part in &mut message.content {
            let ContentPart::ToolCall(call) = part else {
                continue;
            };
            if !names.contains(&call.name) {
                continue;
            }
            if let ToolArguments::Json { value } = &call.arguments {
                call.arguments = ToolArguments::Json {
                    value: json!({ (ARGUMENT): value.clone() }),
                };
            }
        }
    }
    Ok((names, request))
}

fn validate_request_tools(request: &Request) -> Result<(), PluginError> {
    for tool in &request.tools {
        let Value::Object(schema) = &tool.input_schema else {
            continue;
        };
        if !has_composition_root(schema) {
            continue;
        }
        let legacy = schema
            .get("$schema")
            .and_then(Value::as_str)
            .is_some_and(|uri| uri.contains("draft-03") || uri.contains("draft-04"));
        let mut clone = schema.clone();
        rebase_schema(&mut clone, true, legacy)?;
    }
    Ok(())
}

fn wrap_tool_schema(schema: &mut Map<String, Value>) -> Result<Map<String, Value>, PluginError> {
    let legacy = schema
        .get("$schema")
        .and_then(Value::as_str)
        .is_some_and(|uri| uri.contains("draft-03") || uri.contains("draft-04"));
    rebase_schema(schema, true, legacy)?;
    let schema_meta = schema.remove("$schema");
    let inner = std::mem::take(schema);
    let mut wrapped = json!({
        "type": "object",
        "properties": { (ARGUMENT): Value::Object(inner) },
        "required": [ARGUMENT],
        "additionalProperties": false,
    })
    .as_object()
    .cloned()
    .expect("object literal");
    if let Some(uri) = schema_meta {
        wrapped.insert("$schema".to_owned(), uri);
    }
    *schema = wrapped.clone();
    Ok(wrapped)
}

/// Wraps composed tool schemas and prior assistant tool inputs. Returns wrapped
/// tool names, or an error when a recursive schema cannot be relocated.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn wrap_body(body: &mut Map<String, Value>) -> Result<HashSet<String>, PluginError> {
    validate_wrap(body)?;
    if !body.get("messages").is_some_and(Value::is_array) {
        return Ok(HashSet::new());
    }
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return Ok(HashSet::new());
    };
    let mut names = HashSet::new();
    for tool in tools {
        let Some(object) = tool.as_object_mut() else {
            continue;
        };
        let Some(name) = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let schema = object
            .get_mut("input_schema")
            .and_then(Value::as_object_mut);
        let Some(schema) = schema else {
            continue;
        };
        if !has_composition_root(schema) {
            continue;
        }
        let wrapped = wrap_tool_schema(schema)?;
        object.insert("input_schema".to_owned(), Value::Object(wrapped));
        if let Some(examples) = object
            .get_mut("input_examples")
            .and_then(Value::as_array_mut)
        {
            for example in examples {
                let lifted = std::mem::replace(example, Value::Null);
                *example = json!({ (ARGUMENT): lifted });
            }
        }
        names.insert(name);
    }
    if names.is_empty() {
        return Ok(names);
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return Ok(names);
    };
    for message in messages {
        let Some(object) = message.as_object_mut() else {
            continue;
        };
        if object.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content {
            let Some(block) = block.as_object_mut() else {
                continue;
            };
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let block_name = block.get("name").and_then(Value::as_str);
            if !block_name.is_some_and(|name| names.contains(name)) {
                continue;
            }
            if !block.contains_key("input") {
                continue;
            }
            let input = block.remove("input").expect("input");
            block.insert("input".to_owned(), json!({ (ARGUMENT): input }));
        }
    }
    Ok(names)
}

#[cfg_attr(not(test), allow(dead_code))]
fn validate_wrap(body: &Map<String, Value>) -> Result<(), PluginError> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Ok(());
    };
    if !body.get("messages").is_some_and(Value::is_array) {
        return Ok(());
    }
    for tool in tools {
        let Some(object) = tool.as_object() else {
            continue;
        };
        let Some(schema) = object.get("input_schema").and_then(Value::as_object) else {
            continue;
        };
        if !has_composition_root(schema) {
            continue;
        }
        let legacy = schema
            .get("$schema")
            .and_then(Value::as_str)
            .is_some_and(|uri| uri.contains("draft-03") || uri.contains("draft-04"));
        let mut clone = schema.clone();
        rebase_schema(&mut clone, true, legacy)?;
    }
    Ok(())
}

fn rebase_schema(
    schema: &mut Map<String, Value>,
    rebase: bool,
    legacy: bool,
) -> Result<(), PluginError> {
    let id = schema
        .get("$id")
        .or_else(|| if legacy { schema.get("id") } else { None })
        .and_then(Value::as_str);
    let mut rebase = rebase;
    if let Some(id) = id
        && id
            .split('#')
            .next()
            .is_some_and(|prefix| !prefix.is_empty())
    {
        rebase = false;
    }
    if rebase
        && (schema.get("$recursiveAnchor") == Some(&Value::Bool(true))
            || schema.contains_key("$recursiveRef"))
    {
        return Err(PluginError::config(
            "Factory: a recursive tool schema needs $id before it can be wrapped",
        ));
    }
    if rebase {
        for key in ["$ref", "$dynamicRef"] {
            let Some(Value::String(reference)) = schema.get(key) else {
                continue;
            };
            if !reference.starts_with('#') {
                continue;
            }
            let pointer = percent_decode(&reference[1..]);
            if pointer.is_empty() || pointer.starts_with('/') {
                let next = format!("#/properties/{ARGUMENT}{}", &reference[1..]);
                schema.insert(key.to_owned(), Value::String(next));
            }
        }
    }
    for key in SCHEMA_MAPS {
        if let Some(Value::Object(map)) = schema.get_mut(*key) {
            for child in map.values_mut() {
                if let Value::Object(object) = child {
                    rebase_schema(object, rebase, legacy)?;
                }
            }
        }
    }
    for key in SCHEMA_LISTS {
        if let Some(Value::Array(list)) = schema.get_mut(*key) {
            for child in list.iter_mut() {
                if let Value::Object(object) = child {
                    rebase_schema(object, rebase, legacy)?;
                }
            }
        }
    }
    for key in SCHEMA_VALUES {
        match schema.get_mut(*key) {
            Some(Value::Array(list)) => {
                for child in list.iter_mut() {
                    if let Value::Object(object) = child {
                        rebase_schema(object, rebase, legacy)?;
                    }
                }
            }
            Some(Value::Object(object)) => {
                rebase_schema(object, rebase, legacy)?;
            }
            _ => {}
        }
    }
    if let Some(Value::Object(map)) = schema.get_mut("dependencies") {
        for child in map.values_mut() {
            if let Value::Object(object) = child {
                rebase_schema(object, rebase, legacy)?;
            }
        }
    }
    Ok(())
}

fn percent_decode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hi = chars.next().and_then(|c| c.to_digit(16));
            let lo = chars.next().and_then(|c| c.to_digit(16));
            if let (Some(hi), Some(lo)) = (hi, lo)
                && let Some(decoded) = char::from_u32((hi << 4) | lo)
            {
                out.push(decoded);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Unwraps a finished unary JSON message value.
pub(crate) fn unwrap_message(
    value: &mut Value,
    names: &HashSet<String>,
) -> Result<(), PluginError> {
    let Some(content) = value.get_mut("content").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for block in content {
        let Some(block) = block.as_object_mut() else {
            continue;
        };
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = block.get("name").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        if !names.contains(&name) {
            continue;
        }
        let Some(input) = block.remove("input") else {
            continue;
        };
        block.insert("input".to_owned(), unwrap_input(input, &name)?);
    }
    Ok(())
}

fn unwrap_input(input: Value, name: &str) -> Result<Value, PluginError> {
    let Value::Object(map) = input else {
        return Err(invalid_envelope(name));
    };
    if !map.contains_key(ARGUMENT) || map.len() != 1 {
        return Err(invalid_envelope(name));
    }
    Ok(map.get(ARGUMENT).cloned().expect("arguments"))
}

fn invalid_envelope(name: &str) -> PluginError {
    PluginError::decode(format!(
        "Factory: tool {name} returned invalid wrapped arguments"
    ))
}

/// Adapts a same-protocol completion, unwrapping wrapped tool arguments.
pub(crate) async fn adapt_completion(
    completion: Completion,
    names: &HashSet<String>,
    streaming: bool,
) -> Result<Completion, PluginError> {
    if names.is_empty() {
        return Ok(completion);
    }
    match completion {
        Completion::Raw(body) if streaming => {
            Ok(Completion::Raw(unwrap_stream(body, names.clone())))
        }
        Completion::Raw(body) => {
            let mut bytes = Vec::new();
            let mut stream = body;
            while let Some(chunk) = stream.next_chunk().await {
                bytes.extend_from_slice(&chunk?);
            }
            let mut value: Value = hofund_plugin_sdk::json::from_slice(&bytes)
                .map_err(|error| PluginError::decode(error.to_string()))?;
            unwrap_message(&mut value, names)?;
            Ok(Completion::Raw(BodyStream::from_bytes(
                value.to_string().into_bytes(),
            )))
        }
        other => Ok(other),
    }
}

struct StreamState {
    body: BodyStream,
    names: HashSet<String>,
    pending: Vec<u8>,
    ended: bool,
    blocks: HashMap<u64, ToolBlock>,
    out: VecDeque<Result<Vec<u8>, PluginError>>,
}

struct ToolBlock {
    name: String,
    json: String,
    initial: Value,
}

pub(crate) fn unwrap_stream(body: BodyStream, names: HashSet<String>) -> BodyStream {
    let state = StreamState {
        body,
        names,
        pending: Vec::new(),
        ended: false,
        blocks: HashMap::new(),
        out: VecDeque::new(),
    };
    BodyStream::from_stream(unfold(state, |mut state| async move {
        state.step().await.map(|item| (item, state))
    }))
}

impl StreamState {
    async fn step(&mut self) -> Option<Result<Bytes, PluginError>> {
        loop {
            if let Some(front) = self.out.pop_front() {
                return Some(front.map(Bytes::from));
            }
            if self.ended && self.pending.is_empty() {
                return self.finish_open_blocks();
            }
            let boundary = find_frame_end(&self.pending);
            if boundary.is_some() || (self.ended && !self.pending.is_empty()) {
                let (raw, ending) = split_frame(&mut self.pending, boundary);
                match self.frame(raw, ending) {
                    Ok(frames) => {
                        for frame in frames {
                            self.out.push_back(Ok(frame));
                        }
                    }
                    Err(error) => return Some(Err(error)),
                }
                continue;
            }
            match self.body.next_chunk().await {
                None => {
                    self.ended = true;
                }
                Some(Err(error)) => return Some(Err(error)),
                Some(Ok(chunk)) => self.pending.extend_from_slice(&chunk),
            }
        }
    }

    fn finish_open_blocks(&mut self) -> Option<Result<Bytes, PluginError>> {
        if self.blocks.is_empty() {
            return None;
        }
        Some(Err(PluginError::decode(
            "Factory: incomplete wrapped tool arguments",
        )))
    }

    fn frame(&mut self, raw: Vec<u8>, ending: Vec<u8>) -> Result<Vec<Vec<u8>>, PluginError> {
        let text = String::from_utf8_lossy(&raw);
        let uses_crlf = raw.windows(2).any(|window| window == b"\r\n");
        let lines: Vec<&str> = text
            .split(['\r', '\n'])
            .filter(|line| !line.is_empty())
            .collect();
        let data: String = lines
            .iter()
            .filter(|line| line.starts_with("data:"))
            .map(|line| line.strip_prefix("data:").unwrap_or(line).trim_start())
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Ok(vec![[raw.as_slice(), ending.as_slice()].concat()]);
        }
        if data == "[DONE]" {
            if !self.blocks.is_empty() {
                return Err(PluginError::decode(
                    "Factory: incomplete wrapped tool arguments",
                ));
            }
            return Ok(vec![[raw.as_slice(), ending.as_slice()].concat()]);
        }
        let mut event: Value = hofund_plugin_sdk::json::from_str(&data)
            .map_err(|error| PluginError::decode(error.to_string()))?;
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_type == "content_block_start" {
            let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(block) = event.get("content_block").and_then(Value::as_object)
                && block.get("type").and_then(Value::as_str) == Some("tool_use")
            {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if self.names.contains(name) {
                    if self.blocks.contains_key(&index) {
                        return Err(PluginError::decode("Factory: duplicate wrapped tool block"));
                    }
                    let initial = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    self.blocks.insert(
                        index,
                        ToolBlock {
                            name: name.to_owned(),
                            json: String::new(),
                            initial,
                        },
                    );
                    if let Some(block) = event
                        .get_mut("content_block")
                        .and_then(Value::as_object_mut)
                    {
                        block.insert("input".to_owned(), json!({}));
                    }
                    return Ok(vec![replace_event(&raw, &ending, &event, uses_crlf)]);
                }
            }
        }
        if let Some(tracked) = event.get("index").and_then(Value::as_u64)
            && self.blocks.contains_key(&tracked)
        {
            if event_type == "content_block_delta"
                && event
                    .get("delta")
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str)
                    == Some("input_json_delta")
            {
                let partial = event
                    .get("delta")
                    .and_then(|delta| delta.get("partial_json"))
                    .and_then(Value::as_str);
                let Some(partial) = partial else {
                    return Err(PluginError::decode("Factory: invalid tool argument delta"));
                };
                if let Some(block) = self.blocks.get_mut(&tracked) {
                    block.json.push_str(partial);
                }
                return Ok(Vec::new());
            }
            if event_type == "content_block_stop" {
                let block = self.blocks.remove(&tracked).expect("tracked");
                let input = if block.json.is_empty() {
                    unwrap_input(block.initial, &block.name)?
                } else {
                    let wrapped: Value = hofund_plugin_sdk::json::from_str(&block.json)
                        .map_err(|error| PluginError::decode(error.to_string()))?;
                    unwrap_input(wrapped, &block.name)?
                };
                let partial = serde_preserving_stringify(&input, &block.json);
                let delta = json!({
                    "type": "content_block_delta",
                    "index": tracked,
                    "delta": { "type": "input_json_delta", "partial_json": partial },
                });
                return Ok(vec![
                    replace_event(&raw, &ending, &event, uses_crlf),
                    encode_event("content_block_delta", &delta, uses_crlf),
                ]);
            }
        }
        if event_type == "error" {
            self.blocks.clear();
        }
        if event_type == "message_stop" && !self.blocks.is_empty() {
            return Err(PluginError::decode(
                "Factory: incomplete wrapped tool arguments",
            ));
        }
        Ok(vec![[raw.as_slice(), ending.as_slice()].concat()])
    }
}

fn serde_preserving_stringify(input: &Value, wrapped_json: &str) -> String {
    if let Ok(inner) = unwrap_wrapped_text(wrapped_json) {
        return inner;
    }
    input.to_string()
}

fn unwrap_wrapped_text(text: &str) -> Result<String, PluginError> {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("{\"arguments\":") else {
        return Err(PluginError::decode("not wrapped"));
    };
    let inner = rest
        .strip_suffix('}')
        .ok_or_else(|| PluginError::decode("not wrapped"))?;
    Ok(inner.to_owned())
}

fn find_frame_end(pending: &[u8]) -> Option<usize> {
    const ENDINGS: &[&[u8]] = &[b"\r\n\r\n", b"\r\n\n", b"\n\r\n", b"\n\n", b"\r\r"];
    let mut earliest = None;
    for ending in ENDINGS {
        if let Some(index) = pending
            .windows(ending.len())
            .position(|window| window == *ending)
        {
            earliest = Some(earliest.map_or(index, |prev: usize| prev.min(index)));
        }
    }
    earliest
}

fn split_frame(pending: &mut Vec<u8>, boundary: Option<usize>) -> (Vec<u8>, Vec<u8>) {
    match boundary {
        Some(index) => {
            let ending_len = frame_ending_len(&pending[index..]);
            let raw = pending.drain(..index).collect();
            let ending = pending.drain(..ending_len).collect();
            (raw, ending)
        }
        None => (std::mem::take(pending), Vec::new()),
    }
}

fn frame_ending_len(after_boundary: &[u8]) -> usize {
    for len in [4usize, 3, 2] {
        if after_boundary.len() >= len && is_frame_ending(&after_boundary[..len]) {
            return len;
        }
    }
    2
}

fn is_frame_ending(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"\r\n\r\n" | b"\r\n\n" | b"\n\r\n" | b"\n\n" | b"\r\r"
    )
}

fn replace_event(raw: &[u8], ending: &[u8], event: &Value, uses_crlf: bool) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let eol = if uses_crlf { "\r\n" } else { "\n" };
    let mut next = Vec::new();
    let mut wrote_data = false;
    for line in text.split(['\r', '\n']).filter(|line| !line.is_empty()) {
        if line.starts_with("data:") {
            if !wrote_data {
                next.extend_from_slice(b"data: ");
                next.extend_from_slice(event.to_string().as_bytes());
                wrote_data = true;
            }
        } else {
            next.extend_from_slice(line.as_bytes());
            next.extend_from_slice(eol.as_bytes());
        }
    }
    next.extend_from_slice(ending);
    next
}

fn encode_event(event_name: &str, data: &Value, uses_crlf: bool) -> Vec<u8> {
    let eol = if uses_crlf { "\r\n" } else { "\n" };
    let mut out = format!("event: {event_name}{eol}data: ");
    out.push_str(&data.to_string());
    out.push_str(eol);
    out.push_str(eol);
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_kit_subscription::block_on;
    use hofund_plugin_sdk::json::json;

    fn object() -> Map<String, Value> {
        json!({
            "type": "object",
            "properties": { "mode": { "const": "view" }, "id": { "type": "string" } },
            "required": ["mode", "id"],
            "additionalProperties": false
        })
        .as_object()
        .cloned()
        .expect("object")
    }

    fn sample_input() -> Value {
        json!({ "mode": "view", "id": "task_1" })
    }

    fn request(schema: Value, extra: Map<String, Value>) -> Map<String, Value> {
        let mut body = json!({
            "model": "claude-opus-5-5",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "Use the task tool." }],
            "tools": [{ "name": "task", "input_schema": schema }],
        })
        .as_object()
        .cloned()
        .expect("body");
        for (key, value) in extra {
            body.insert(key, value);
        }
        body
    }

    #[test]
    fn r52e_wraps_anyof_oneof_allof_root_schemas() {
        let input = sample_input();
        for keyword in ["anyOf", "oneOf", "allOf"] {
            let create = json!({
                "type": "object",
                "properties": { "mode": { "const": "create" }, "name": { "type": "string" } },
                "required": ["mode", "name"],
                "additionalProperties": false
            });
            let schema = if keyword == "allOf" {
                json!({
                    "allOf": [object(), json!({ "properties": { "id": { "pattern": "^task_" } } })]
                })
            } else {
                json!({ (keyword): [object(), create] })
            };
            let mut body = request(schema, Map::new());
            let names = wrap_body(&mut body).expect("wrap");
            assert_eq!(names, HashSet::from(["task".to_owned()]));
            let wrapped = &body["tools"][0]["input_schema"];
            assert_eq!(wrapped["type"], "object");
            assert_eq!(wrapped["required"], json!(["arguments"]));
            assert_eq!(wrapped["additionalProperties"], Value::Bool(false));
            assert!(wrapped["properties"]["arguments"].is_object());
            let mut reply = json!({
                "content": [{ "type": "tool_use", "id": "call_1", "name": "task", "input": { "arguments": input } }]
            });
            unwrap_message(&mut reply, &names).expect("unwrap");
            assert_eq!(reply["content"][0]["input"], input);
        }
    }

    #[test]
    fn r52e_wraps_history_examples_and_skips_ordinary_tools() {
        let schema = json!({ "anyOf": [object()] });
        let ordinary = json!({
            "name": "read",
            "input_schema": { "type": "object", "properties": { "path": { "anyOf": [{ "type": "string" }, { "type": "null" }] } } }
        });
        let input = sample_input();
        let mut body = request(
            schema.clone(),
            Map::from_iter([
                (
                    "tools".to_owned(),
                    json!([
                        { "name": "task", "input_schema": schema, "input_examples": [input.clone()], "cache_control": { "type": "ephemeral" } },
                        ordinary
                    ]),
                ),
                (
                    "messages".to_owned(),
                    json!([
                        { "role": "assistant", "content": [
                            { "type": "tool_use", "id": "call_1", "name": "task", "input": input.clone() },
                            { "type": "tool_use", "id": "call_2", "name": "read", "input": { "path": Value::Null } }
                        ]},
                        { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "call_1", "content": "done" }] }
                    ]),
                ),
            ]),
        );
        let names = wrap_body(&mut body).expect("wrap");
        assert_eq!(names, HashSet::from(["task".to_owned()]));
        assert_eq!(
            body["messages"][0]["content"][0]["input"],
            json!({ "arguments": input })
        );
        assert_eq!(
            body["messages"][0]["content"][1],
            json!({ "type": "tool_use", "id": "call_2", "name": "read", "input": { "path": Value::Null } })
        );
        assert_eq!(
            body["tools"][0]["input_examples"],
            json!([{ "arguments": input }])
        );
        assert_eq!(body["tools"][1], ordinary);
    }

    #[test]
    fn r52e_rebases_pointers_without_touching_resource_ids() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "anyOf": [
                { "$ref": "#/$defs/view" },
                { "$ref": "#%2F$defs%2Fview" },
                { "$ref": "#anchor" },
                { "$ref": "https://example.test/other.json" }
            ],
            "$defs": {
                "view": {
                    "type": "object",
                    "properties": { "mode": { "const": "view" }, "id": { "type": "string" }, "nested": { "$ref": "#" } },
                    "required": ["mode", "id"],
                    "additionalProperties": false,
                    "$anchor": "anchor"
                },
                "resource": { "$id": "child.json", "$defs": { "value": { "type": "string" } }, "anyOf": [{ "$ref": "#/$defs/value" }] }
            },
            "properties": {
                "literal": {
                    "const": { "$ref": "#/$defs/untouched", "anyOf": [{ "$ref": "literal" }] },
                    "default": { "$ref": "default" },
                    "examples": [{ "$ref": "example" }]
                }
            }
        });
        let mut body = request(schema.clone(), Map::new());
        wrap_body(&mut body).expect("wrap");
        let original = &body["tools"][0]["input_schema"]["properties"]["arguments"];
        assert_eq!(
            original["anyOf"][0],
            json!({ "$ref": "#/properties/arguments/$defs/view" })
        );
        assert_eq!(
            original["$defs"]["view"]["properties"]["nested"]["$ref"],
            "#/properties/arguments"
        );
        assert_eq!(original["$defs"]["resource"], schema["$defs"]["resource"]);
    }

    #[test]
    fn r52e_recursive_schema_without_id_is_rejected() {
        let schema = json!({ "anyOf": [{ "$recursiveRef": "#" }], "$recursiveAnchor": true });
        let mut body = request(schema, Map::new());
        let error = wrap_body(&mut body).expect_err("recursive");
        assert!(error.message().contains("recursive tool schema needs $id"));
    }

    #[test]
    fn r52e_invalid_wrapped_inputs_fail() {
        let names = HashSet::from(["task".to_owned()]);
        for input in [
            json!({ "wrong": {} }),
            json!({ "arguments": {}, "extra": true }),
        ] {
            let mut reply =
                json!({ "content": [{ "type": "tool_use", "name": "task", "input": input }] });
            assert!(unwrap_message(&mut reply, &names).is_err());
        }
    }

    #[test]
    fn r52e_stream_unwraps_split_deltas_and_preserves_frames() {
        let input =
            json!({ "mode": "view", "id": "task_1", "text": "hello \"arguments\" { braces }" });
        let wrapped = format!(r#"{{"arguments":{input}}}"#);
        let ordinary = r#"{"arguments":"a caller-owned field"}"#;
        let eol = "\r\n";
        let mut stream = String::new();
        stream.push_str(&format!(": keep-alive{eol}{eol}"));
        stream.push_str(&format!(
            "event: message_start{eol}data: {{\"type\":\"message_start\"}}{eol}{eol}"
        ));
        stream.push_str(&format!(
            "event: content_block_start{eol}data: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"task\",\"input\":{{}}}}}}{eol}{eol}"
        ));
        for i in (0..wrapped.len()).step_by(3) {
            let partial = &wrapped[i..wrapped.len().min(i + 3)];
            let quoted = Value::String(partial.to_owned()).to_string();
            stream.push_str(&format!(
                "event: content_block_delta{eol}data: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{quoted}}}}}{eol}{eol}"
            ));
        }
        stream.push_str(&format!(
            "event: content_block_stop{eol}data: {{\"type\":\"content_block_stop\",\"index\":1}}{eol}{eol}"
        ));
        stream.push_str(&format!(
            "event: content_block_start{eol}data: {{\"type\":\"content_block_start\",\"index\":2,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"call_2\",\"name\":\"read\",\"input\":{{}}}}}}{eol}{eol}"
        ));
        let ordinary_quoted = Value::String(ordinary.to_owned()).to_string();
        stream.push_str(&format!(
            "event: content_block_delta{eol}data: {{\"type\":\"content_block_delta\",\"index\":2,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{ordinary_quoted}}}}}{eol}{eol}"
        ));
        stream.push_str(&format!(
            "event: content_block_stop{eol}data: {{\"type\":\"content_block_stop\",\"index\":2}}{eol}{eol}"
        ));
        stream.push_str(&format!(
            "event: message_stop{eol}data: {{\"type\":\"message_stop\"}}{eol}{eol}"
        ));
        let names = HashSet::from(["task".to_owned()]);
        let mut adapted = unwrap_stream(BodyStream::from_bytes(stream.into_bytes()), names);
        let mut out = Vec::new();
        while let Some(chunk) = block_on(adapted.next_chunk()) {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains(": keep-alive"));
        assert!(text.contains("\"partial_json\":"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn r52e_stream_initial_input_without_deltas() {
        let stream = concat!(
            "event: content_block_start\n",
            "data: ",
            "{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"name\":\"task\",\"input\":{\"arguments\":{\"mode\":\"view\",\"id\":\"task_1\"}}}}",
            "\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let names = HashSet::from(["task".to_owned()]);
        let mut adapted = unwrap_stream(BodyStream::from_bytes(stream.as_bytes().to_vec()), names);
        let mut out = Vec::new();
        while let Some(chunk) = block_on(adapted.next_chunk()) {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""partial_json":"{\"mode\":\"view\",\"id\":\"task_1\"}"#)
                || text.contains(r#""partial_json":"{"mode":"view","id":"task_1"}"#),
            "stream output: {text}"
        );
        assert!(!text.contains("arguments"));
    }
}
