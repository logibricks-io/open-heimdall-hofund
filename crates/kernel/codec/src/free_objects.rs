use std::collections::BTreeMap;

use hofund_ir::FreeObjectPaths;
use serde_json::{Map, Value, json};

/// Restores free-form objects in a same-protocol JSON or SSE reply.
#[derive(Debug)]
pub struct FreeObjectRaw {
    protocol: RawProtocol,
    streaming: bool,
    paths: FreeObjectPaths,
    buffer: Vec<u8>,
    block: Vec<u8>,
    names: BTreeMap<String, String>,
    text: BTreeMap<String, String>,
    order: Vec<String>,
    pending: BTreeMap<String, HeldFrame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawProtocol {
    Chat,
    Responses,
    Messages,
}

#[derive(Debug)]
struct HeldFrame {
    event: Map<String, Value>,
    pre: Vec<u8>,
    post: Vec<u8>,
}

impl FreeObjectRaw {
    /// Creates a filter for one of the three supported protocols.
    #[must_use]
    pub fn new(protocol: &str, streaming: bool, paths: FreeObjectPaths) -> Option<Self> {
        let protocol = match protocol {
            "openai-chat" => RawProtocol::Chat,
            "openai-responses" => RawProtocol::Responses,
            "anthropic-messages" => RawProtocol::Messages,
            _ => return None,
        };
        Some(Self {
            protocol,
            streaming,
            paths,
            buffer: Vec::new(),
            block: Vec::new(),
            names: BTreeMap::new(),
            text: BTreeMap::new(),
            order: Vec::new(),
            pending: BTreeMap::new(),
        })
    }

    /// Accepts arbitrary body chunks and returns bytes ready for the client.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(chunk);
        if !self.streaming {
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let blank = line.iter().all(|byte| matches!(byte, b'\r' | b'\n'));
            self.block.extend_from_slice(&line);
            if blank {
                let block = std::mem::take(&mut self.block);
                out.extend(self.event(&block));
            }
        }
        out
    }

    /// Flushes a unary body or a trailing SSE event and releases held calls.
    pub fn finish(&mut self) -> Vec<u8> {
        if !self.streaming {
            let body = std::mem::take(&mut self.buffer);
            return self.whole(&body).unwrap_or(body);
        }
        let mut rest = std::mem::take(&mut self.block);
        rest.extend_from_slice(&std::mem::take(&mut self.buffer));
        let mut out = if rest.is_empty() {
            Vec::new()
        } else {
            self.event(&rest)
        };
        out.extend(self.drain());
        out
    }

    fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.protocol == RawProtocol::Chat && !self.order.is_empty() {
            let chunk = json!({
                "object": "chat.completion.chunk",
                "choices": self.chat_held(""),
            });
            if let Ok(data) = serde_json::to_vec(&chunk) {
                out.extend_from_slice(b"data: ");
                out.extend_from_slice(&data);
                out.extend_from_slice(b"\n\n");
            }
        }
        let keys: Vec<String> = self.pending.keys().cloned().collect();
        for key in keys {
            out.extend(self.release_pending(&key, true));
        }
        out
    }

    fn event(&mut self, bytes: &[u8]) -> Vec<u8> {
        let Some((from, to, data)) = data_line(bytes) else {
            return bytes.to_vec();
        };
        if data == b"[DONE]" {
            if self.protocol == RawProtocol::Chat && !self.order.is_empty() {
                let mut out = self.drain();
                out.extend_from_slice(bytes);
                return out;
            }
            return bytes.to_vec();
        }
        if data.first() != Some(&b'{') {
            return bytes.to_vec();
        }
        let Ok(Value::Object(event)) = serde_json::from_slice::<Value>(data) else {
            return bytes.to_vec();
        };
        let mut held = HeldFrame {
            event,
            pre: bytes[..from].to_vec(),
            post: bytes[to..].to_vec(),
        };
        match self.protocol {
            RawProtocol::Chat => {
                if self.chat(&mut held.event) {
                    held.bytes()
                } else {
                    bytes.to_vec()
                }
            }
            RawProtocol::Responses => self.responses(held, bytes),
            RawProtocol::Messages => self.messages(held, bytes),
        }
    }

    fn hold(&mut self, key: &str, name: &str) {
        if self.names.contains_key(key) {
            return;
        }
        self.names.insert(key.to_owned(), name.to_owned());
        self.text.entry(key.to_owned()).or_default();
        self.order.push(key.to_owned());
    }

    fn forget(&mut self, key: &str) {
        self.names.remove(key);
        self.text.remove(key);
        if let Some(index) = self.order.iter().position(|held| held == key) {
            self.order.remove(index);
        }
    }

    fn chat_held(&mut self, choice: &str) -> Vec<Value> {
        let order = self.order.clone();
        let mut by_choice: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut choices = Vec::new();
        for key in order {
            let Some((choice_index, tool_index)) = key.split_once('/') else {
                continue;
            };
            if !choice.is_empty() && choice != choice_index {
                continue;
            }
            let name = self.names.get(&key).cloned().unwrap_or_default();
            let text = self.text.get(&key).cloned().unwrap_or_default();
            let arguments = self.paths.restore_text(&name, &text);
            let index = serde_json::from_str::<Value>(tool_index)
                .unwrap_or_else(|_| Value::String(tool_index.to_owned()));
            if !by_choice.contains_key(choice_index) {
                choices.push(choice_index.to_owned());
            }
            by_choice
                .entry(choice_index.to_owned())
                .or_default()
                .push(json!({"index": index, "function": {"arguments": arguments}}));
            self.forget(&key);
        }
        if !choice.is_empty() {
            return by_choice.remove(choice).unwrap_or_default();
        }
        choices
            .into_iter()
            .map(|choice| {
                let index = choice.parse::<u64>().unwrap_or(0);
                json!({
                    "index": index,
                    "delta": {"tool_calls": by_choice.remove(&choice).unwrap_or_default()},
                })
            })
            .collect()
    }

    fn chat(&mut self, event: &mut Map<String, Value>) -> bool {
        let mut changed = false;
        let Some(choices) = event.get_mut("choices").and_then(Value::as_array_mut) else {
            return false;
        };
        for choice in choices {
            let Some(choice) = choice.as_object_mut() else {
                continue;
            };
            let choice_index = choice.get("index").map(json_text).unwrap_or_default();
            if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut)
                && let Some(calls) = delta.get_mut("tool_calls").and_then(Value::as_array_mut)
            {
                for (position, call) in calls.iter_mut().enumerate() {
                    let Some(call) = call.as_object_mut() else {
                        continue;
                    };
                    let index = call
                        .get("index")
                        .map(json_text)
                        .unwrap_or_else(|| position.to_string());
                    let Some(function) = call.get_mut("function").and_then(Value::as_object_mut)
                    else {
                        continue;
                    };
                    let key = format!("{choice_index}/{index}");
                    if let Some(name) = function.get("name").and_then(Value::as_str)
                        && !name.is_empty()
                        && self.paths.paths(name).is_some()
                    {
                        self.hold(&key, name);
                    }
                    if !self.names.contains_key(&key) {
                        continue;
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str)
                        && !arguments.is_empty()
                    {
                        self.text.entry(key).or_default().push_str(arguments);
                        function.insert("arguments".to_owned(), Value::String(String::new()));
                        changed = true;
                    }
                }
            }
            let finished = choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .is_some_and(|reason| !reason.is_empty());
            if finished && !self.order.is_empty() {
                let held = self.chat_held(&choice_index);
                if !held.is_empty() {
                    if !choice.contains_key("delta") {
                        choice.insert("delta".to_owned(), json!({}));
                    }
                    if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut) {
                        let calls = delta
                            .entry("tool_calls")
                            .or_insert_with(|| Value::Array(Vec::new()))
                            .as_array_mut();
                        if let Some(calls) = calls {
                            calls.extend(held);
                            changed = true;
                        }
                    }
                }
            }
            if let Some(message) = choice.get_mut("message").and_then(Value::as_object_mut)
                && let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut)
            {
                for call in calls {
                    let Some(function) = call.get_mut("function").and_then(Value::as_object_mut)
                    else {
                        continue;
                    };
                    let name = function
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    changed |= restore_string_field(function, "arguments", &self.paths, &name);
                }
            }
        }
        changed
    }

    fn hold_delta(&mut self, key: &str, held: HeldFrame) -> Vec<u8> {
        let out = self.release_pending(key, false);
        self.pending.insert(key.to_owned(), held);
        out
    }

    fn release_pending(&mut self, key: &str, last: bool) -> Vec<u8> {
        let Some(mut held) = self.pending.remove(key) else {
            return Vec::new();
        };
        let text = if last {
            let name = self.names.get(key).cloned().unwrap_or_default();
            let text = self.text.get(key).cloned().unwrap_or_default();
            self.paths.restore_text(&name, &text)
        } else {
            String::new()
        };
        if self.protocol == RawProtocol::Messages {
            if let Some(delta) = held.event.get_mut("delta").and_then(Value::as_object_mut) {
                delta.insert("partial_json".to_owned(), Value::String(text));
            }
        } else {
            held.event.insert("delta".to_owned(), Value::String(text));
        }
        held.bytes()
    }

    fn responses(&mut self, mut held: HeldFrame, original: &[u8]) -> Vec<u8> {
        let kind = held
            .event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut changed = false;
        let mut pre = Vec::new();
        match kind.as_str() {
            "response.output_item.added" => {
                if let Some(item) = held.event.get("item").and_then(Value::as_object)
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                    if self.paths.paths(name).is_some() {
                        if let Some(id) = item.get("id").and_then(Value::as_str)
                            && !id.is_empty()
                        {
                            self.hold(id, name);
                        }
                        let key = format!(
                            "#{}",
                            held.event
                                .get("output_index")
                                .map(json_text)
                                .unwrap_or_default()
                        );
                        self.hold(&key, name);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let key = self.response_key(&held.event);
                if !key.is_empty() {
                    if let Some(delta) = held.event.get("delta").and_then(Value::as_str) {
                        self.text.entry(key.clone()).or_default().push_str(delta);
                    }
                    return self.hold_delta(&key, held);
                }
            }
            "response.function_call_arguments.done" => {
                let key = self.response_key(&held.event);
                if !key.is_empty() {
                    if let Some(arguments) = held.event.get("arguments").and_then(Value::as_str) {
                        self.text.insert(key.clone(), arguments.to_owned());
                    }
                    pre = self.release_pending(&key, true);
                    let name = self.names.get(&key).cloned().unwrap_or_default();
                    let text = self.text.get(&key).cloned().unwrap_or_default();
                    held.event.insert(
                        "arguments".to_owned(),
                        Value::String(self.paths.restore_text(&name, &text)),
                    );
                    self.forget_response(&held.event);
                    changed = true;
                }
            }
            _ => {}
        }
        if let Some(item) = held.event.get_mut("item").and_then(Value::as_object_mut) {
            changed |= restore_response_item(item, &self.paths);
        }
        let response = if let Some(response) = held
            .event
            .get_mut("response")
            .and_then(Value::as_object_mut)
        {
            Some(response)
        } else if held.event.get("object").and_then(Value::as_str) == Some("response") {
            Some(&mut held.event)
        } else {
            None
        };
        if let Some(response) = response
            && let Some(output) = response.get_mut("output").and_then(Value::as_array_mut)
        {
            for item in output.iter_mut().filter_map(Value::as_object_mut) {
                changed |= restore_response_item(item, &self.paths);
            }
        }
        if changed {
            pre.extend(held.bytes());
            pre
        } else {
            pre.extend_from_slice(original);
            pre
        }
    }

    fn response_key(&self, event: &Map<String, Value>) -> String {
        if let Some(id) = event.get("item_id").and_then(Value::as_str)
            && !id.is_empty()
            && self.names.contains_key(id)
        {
            return id.to_owned();
        }
        let key = format!(
            "#{}",
            event.get("output_index").map(json_text).unwrap_or_default()
        );
        if self.names.contains_key(&key) {
            key
        } else {
            String::new()
        }
    }

    fn forget_response(&mut self, event: &Map<String, Value>) {
        let id = event
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let output = format!(
            "#{}",
            event.get("output_index").map(json_text).unwrap_or_default()
        );
        for key in [id, output] {
            if !key.is_empty() {
                self.forget(&key);
            }
        }
    }

    fn messages(&mut self, mut held: HeldFrame, original: &[u8]) -> Vec<u8> {
        let mut changed = false;
        if let Some(content) = held.event.get_mut("content").and_then(Value::as_array_mut) {
            for block in content.iter_mut().filter_map(Value::as_object_mut) {
                changed |= restore_message_block(block, &self.paths);
            }
        }
        let key = held.event.get("index").map(json_text).unwrap_or_default();
        let kind = held
            .event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut pre = Vec::new();
        match kind.as_str() {
            "content_block_start" => {
                if let Some(block) = held
                    .event
                    .get_mut("content_block")
                    .and_then(Value::as_object_mut)
                {
                    changed |= restore_message_block(block, &self.paths);
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if self.paths.paths(name).is_some() {
                            self.hold(&key, name);
                        }
                    }
                }
            }
            "content_block_delta" => {
                let is_delta = held
                    .event
                    .get("delta")
                    .and_then(Value::as_object)
                    .is_some_and(|delta| {
                        delta.get("type").and_then(Value::as_str) == Some("input_json_delta")
                    });
                if self.names.contains_key(&key) && is_delta {
                    if let Some(text) = held
                        .event
                        .get("delta")
                        .and_then(Value::as_object)
                        .and_then(|delta| delta.get("partial_json"))
                        .and_then(Value::as_str)
                    {
                        self.text.entry(key.clone()).or_default().push_str(text);
                    }
                    return self.hold_delta(&key, held);
                }
            }
            "content_block_stop" if self.names.contains_key(&key) => {
                pre = self.release_pending(&key, true);
                self.forget(&key);
            }
            _ => {}
        }
        if changed {
            pre.extend(held.bytes());
            pre
        } else {
            pre.extend_from_slice(original);
            pre
        }
    }

    fn whole(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        let Ok(Value::Object(event)) = serde_json::from_slice::<Value>(body) else {
            return None;
        };
        let mut held = HeldFrame {
            event,
            pre: Vec::new(),
            post: Vec::new(),
        };
        match self.protocol {
            RawProtocol::Chat => self
                .chat(&mut held.event)
                .then(|| serde_json::to_vec(&held.event).ok())
                .flatten(),
            RawProtocol::Responses => {
                let out = self.responses(held, &[]);
                if out.is_empty() {
                    None
                } else {
                    Some(
                        out.strip_prefix(b"data: ")
                            .map_or(out.clone(), |rest| rest.to_vec()),
                    )
                }
            }
            RawProtocol::Messages => {
                let out = self.messages(held, &[]);
                if out.is_empty() {
                    None
                } else {
                    Some(
                        out.strip_prefix(b"data: ")
                            .map_or(out.clone(), |rest| rest.to_vec()),
                    )
                }
            }
        }
    }
}

impl HeldFrame {
    fn bytes(self) -> Vec<u8> {
        let Ok(data) = serde_json::to_vec(&self.event) else {
            return Vec::new();
        };
        let mut out = self.pre;
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&data);
        out.extend_from_slice(&self.post);
        out
    }
}

fn restore_response_item(item: &mut Map<String, Value>, paths: &FreeObjectPaths) -> bool {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return false;
    }
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    restore_string_field(item, "arguments", paths, &name)
}

fn restore_message_block(block: &mut Map<String, Value>, paths: &FreeObjectPaths) -> bool {
    if block.get("type").and_then(Value::as_str) != Some("tool_use") || !block.contains_key("input")
    {
        return false;
    }
    let name = block
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if paths.paths(name).is_none() {
        return false;
    }
    let Ok(arguments) = serde_json::to_vec(&block["input"]) else {
        return false;
    };
    let arguments = String::from_utf8(arguments).unwrap_or_default();
    let restored = paths.restore_text(name, &arguments);
    if restored == arguments {
        return false;
    }
    let Ok(value) = serde_json::from_str(&restored) else {
        return false;
    };
    block.insert("input".to_owned(), value);
    true
}

fn restore_string_field(
    object: &mut Map<String, Value>,
    field: &str,
    paths: &FreeObjectPaths,
    name: &str,
) -> bool {
    if paths.paths(name).is_none() {
        return false;
    }
    let Some(arguments) = object.get(field).and_then(Value::as_str) else {
        return false;
    };
    let restored = paths.restore_text(name, arguments);
    if restored == arguments {
        return false;
    }
    object.insert(field.to_owned(), Value::String(restored));
    true
}

fn data_line(bytes: &[u8]) -> Option<(usize, usize, &[u8])> {
    let mut at = 0;
    while at < bytes.len() {
        let end = bytes[at..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |end| at + end + 1);
        let line = bytes[at..end]
            .strip_suffix(b"\n")
            .unwrap_or(&bytes[at..end]);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(data) = line.strip_prefix(b"data:") {
            return Some((at, at + line.len(), trim_ascii(data)));
        }
        at = end;
    }
    None
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn json_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => String::new(),
    }
}
