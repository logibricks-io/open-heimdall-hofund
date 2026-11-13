//! Incremental decoding for Kiro AWS event-stream replies.

use std::collections::{HashMap, VecDeque};

use hofund_plugin_api::{
    BlockStart, BodyStream, Completion, ContentBlockStart, ContentBlockStop, ContentPart,
    Extensions, Message, MessageDelta, MessageStart, MessageStop, PluginError, Response, Role,
    Stop, StopReason, StreamEvent, TextDelta, ToolArguments, ToolArgumentsDelta, ToolCallPart,
};
use hofund_plugin_sdk::futures;
use hofund_plugin_sdk::json::{self, Value};

const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Text(String),
    ToolStart { id: String, name: String },
    ToolArguments(String),
    Stop(StopReason),
}

#[derive(Default)]
pub(crate) struct Decoder {
    buffer: Vec<u8>,
    events: VecDeque<Event>,
    current_tool: String,
    stop: Option<StopReason>,
    saw_tool: bool,
    frames: usize,
    finished: bool,
}

impl Decoder {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), PluginError> {
        if self.finished {
            return Err(PluginError::decode(
                "Kiro event stream was read after its end",
            ));
        }
        self.buffer.extend_from_slice(chunk);
        while self.buffer.len() >= 12 {
            let total =
                u32::from_be_bytes(self.buffer[..4].try_into().expect("four-byte frame length"))
                    as usize;
            let header_len = u32::from_be_bytes(
                self.buffer[4..8]
                    .try_into()
                    .expect("four-byte header length"),
            ) as usize;
            if !(16..=MAX_FRAME).contains(&total) || header_len > total - 16 {
                return Err(PluginError::decode("Kiro sent a malformed event stream"));
            }
            if self.buffer.len() < total {
                break;
            }
            let frame = self.buffer[..total].to_vec();
            self.buffer.drain(..total);
            self.frames += 1;
            decode_frame(&frame, self)?;
        }
        Ok(())
    }

    pub(crate) fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    pub(crate) fn finish(&mut self) -> Result<(), PluginError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        if !self.buffer.is_empty() {
            return Err(PluginError::decode(format!(
                "Kiro's event stream ended in a partial frame ({} buffered bytes)",
                self.buffer.len()
            )));
        }
        if self.frames == 0 {
            return Err(PluginError::decode(
                "Kiro's event stream contained no frames",
            ));
        }
        self.events.push_back(Event::Stop(if self.saw_tool {
            StopReason::ToolUse
        } else {
            self.stop.clone().unwrap_or(StopReason::EndTurn)
        }));
        Ok(())
    }
}

fn decode_frame(frame: &[u8], decoder: &mut Decoder) -> Result<(), PluginError> {
    let total = frame.len();
    let header_len = u32::from_be_bytes(frame[4..8].try_into().expect("header length")) as usize;
    let headers = frame_headers(&frame[12..12 + header_len])?;
    let payload = &frame[12 + header_len..total - 4];
    let message_type = headers.get(":message-type").map(String::as_str);
    let event_type = headers.get(":event-type").map(String::as_str).unwrap_or("");
    if matches!(message_type, Some("exception" | "error"))
        || matches!(
            event_type,
            "error"
                | "throttlingError"
                | "validationError"
                | "serviceUnavailableError"
                | "internalServerException"
        )
    {
        let kind = headers
            .get(":exception-type")
            .or_else(|| headers.get(":error-code"))
            .map(String::as_str)
            .unwrap_or(event_type);
        return Err(PluginError::decode(format!(
            "Kiro reply failed{}: {}",
            if kind.is_empty() { "" } else { ": " },
            if kind.is_empty() {
                String::from_utf8_lossy(payload).trim().to_owned()
            } else {
                format!("{kind}: {}", String::from_utf8_lossy(payload).trim())
            }
        )));
    }
    let Ok(value) = json::from_slice::<Value>(payload) else {
        return Ok(());
    };
    match event_type {
        "assistantResponseEvent" => {
            if let Some(text) = value.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                decoder.events.push_back(Event::Text(text.to_owned()));
            }
        }
        "toolUseEvent" => {
            let id = value.get("toolUseId").and_then(Value::as_str).unwrap_or("");
            if !id.is_empty() && id != decoder.current_tool {
                decoder.current_tool = id.to_owned();
                decoder.saw_tool = true;
                decoder.events.push_back(Event::ToolStart {
                    id: id.to_owned(),
                    name: value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                });
            }
            if let Some(input) = value.get("input").filter(|input| !input.is_null()) {
                let arguments = match input {
                    Value::String(text) => text.clone(),
                    Value::Object(object) if object.is_empty() => String::new(),
                    other => other.to_string(),
                };
                if !arguments.is_empty() {
                    decoder.events.push_back(Event::ToolArguments(arguments));
                }
            }
        }
        "metadataEvent" | "messageMetadataEvent" => {
            if let Some(reason) = value
                .get("stopReason")
                .and_then(Value::as_str)
                .map(stop_reason)
            {
                decoder.stop = Some(reason);
            }
        }
        _ => {}
    }
    Ok(())
}

fn frame_headers(mut cursor: &[u8]) -> Result<HashMap<String, String>, PluginError> {
    let mut headers = HashMap::new();
    while !cursor.is_empty() {
        let name_len = cursor[0] as usize;
        if cursor.len() < 2 + name_len {
            return Err(PluginError::decode("Kiro event header is truncated"));
        }
        let name = std::str::from_utf8(&cursor[1..1 + name_len])
            .map_err(|_| PluginError::decode("Kiro event header name is not UTF-8"))?;
        let value_type = cursor[1 + name_len];
        cursor = &cursor[2 + name_len..];
        if matches!(value_type, 6 | 7) {
            if cursor.len() < 2 {
                return Err(PluginError::decode("Kiro event header is truncated"));
            }
            let len = u16::from_be_bytes(cursor[..2].try_into().expect("header value length"));
            let len = usize::from(len);
            if cursor.len() < 2 + len {
                return Err(PluginError::decode("Kiro event header is truncated"));
            }
            if value_type == 7 {
                let value = std::str::from_utf8(&cursor[2..2 + len])
                    .map_err(|_| PluginError::decode("Kiro event header is not UTF-8"))?;
                headers.insert(name.to_owned(), value.to_owned());
            }
            cursor = &cursor[2 + len..];
            continue;
        }
        let len = match value_type {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            _ => return Err(PluginError::decode("Kiro event header type is unknown")),
        };
        if cursor.len() < len {
            return Err(PluginError::decode("Kiro event header is truncated"));
        }
        cursor = &cursor[len..];
    }
    Ok(headers)
}

fn stop_reason(reason: &str) -> StopReason {
    if reason.eq_ignore_ascii_case("max_tokens") {
        StopReason::MaxTokens
    } else if reason.eq_ignore_ascii_case("content_filtered") {
        StopReason::Refusal
    } else if reason.eq_ignore_ascii_case("model_context_window_exceeded") {
        StopReason::Other("model_context_window_exceeded".to_owned())
    } else {
        StopReason::EndTurn
    }
}

#[cfg(test)]
pub(crate) fn assistant_text(body: &[u8]) -> Result<String, PluginError> {
    let response = unary(body, "auto", &HashMap::new())?;
    let text = response
        .message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if text.is_empty() {
        return Err(PluginError::decode(
            "Kiro's event stream did not contain assistant text",
        ));
    }
    Ok(text)
}

pub(crate) fn unary(
    body: &[u8],
    model: &str,
    names: &HashMap<String, String>,
) -> Result<Box<Response>, PluginError> {
    let mut decoder = Decoder::default();
    decoder.push(body)?;
    decoder.finish()?;
    let mut message = Message::new(Role::Assistant);
    let mut stop = StopReason::EndTurn;
    while let Some(event) = decoder.next_event() {
        match event {
            Event::Text(text) => {
                if let Some(ContentPart::Text(previous)) = message.content.last_mut() {
                    previous.text.push_str(&text);
                } else {
                    message.content.push(ContentPart::text(text));
                }
            }
            Event::ToolStart { id, name } => {
                message.content.push(ContentPart::ToolCall(ToolCallPart {
                    id,
                    name: names.get(&name).cloned().unwrap_or(name),
                    arguments: ToolArguments::Raw {
                        text: String::new(),
                    },
                    extensions: Extensions::new(),
                }));
            }
            Event::ToolArguments(arguments) => {
                if let Some(ContentPart::ToolCall(call)) = message.content.last_mut() {
                    match &mut call.arguments {
                        ToolArguments::Raw { text } => text.push_str(&arguments),
                        ToolArguments::Json { .. } => {}
                    }
                }
            }
            Event::Stop(reason) => stop = reason,
        }
    }
    for part in &mut message.content {
        if let ContentPart::ToolCall(call) = part {
            let raw = call.arguments.to_text();
            call.arguments = ToolArguments::Json {
                value: json::from_str(&raw).unwrap_or_else(|_| Value::Object(Default::default())),
            };
        }
    }
    Ok(Box::new(
        Response::new(message)
            .with_id(Some("msg_kiro_probe".to_owned()))
            .with_model(Some(model.to_owned()))
            .with_stop(Some(Stop {
                reason: stop,
                sequence: None,
            })),
    ))
}

pub(crate) fn stream(
    body: BodyStream,
    model: String,
    names: HashMap<String, String>,
) -> Completion {
    let state = StreamState {
        reader: Reader {
            body,
            decoder: Decoder::default(),
            ended: false,
        },
        model,
        names,
        pending: VecDeque::new(),
        open: None,
        next_index: 0,
        started: false,
        finished: false,
    };
    Completion::Stream(Box::pin(futures::unfold(state, |mut state| async move {
        loop {
            if let Some(event) = state.pending.pop_front() {
                return Some((Ok(event), state));
            }
            if state.finished {
                return None;
            }
            if !state.started {
                state.started = true;
                return Some((Ok(state.start()), state));
            }
            match state.reader.next_event().await {
                Ok(Some(event)) => state.apply(event),
                Ok(None) => state.finished = true,
                Err(error) => {
                    state.finished = true;
                    return Some((Err(error), state));
                }
            }
        }
    })))
}

struct Reader {
    body: BodyStream,
    decoder: Decoder,
    ended: bool,
}

impl Reader {
    async fn next_event(&mut self) -> Result<Option<Event>, PluginError> {
        loop {
            if let Some(event) = self.decoder.next_event() {
                return Ok(Some(event));
            }
            if self.ended {
                return Ok(None);
            }
            match self.body.next_chunk().await {
                Some(Ok(chunk)) => self.decoder.push(&chunk)?,
                Some(Err(error)) => {
                    return Err(PluginError::decode(format!(
                        "Kiro's reply broke off: {}",
                        error.message()
                    )));
                }
                None => {
                    self.decoder.finish()?;
                    self.ended = true;
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text,
    Tool,
}

struct StreamState {
    reader: Reader,
    model: String,
    names: HashMap<String, String>,
    pending: VecDeque<StreamEvent>,
    open: Option<(OpenBlock, u32)>,
    next_index: u32,
    started: bool,
    finished: bool,
}

impl StreamState {
    fn start(&self) -> StreamEvent {
        StreamEvent::MessageStart(MessageStart {
            id: Some("msg_kiro_probe".to_owned()),
            model: Some(self.model.clone()),
            usage: None,
            extensions: Extensions::new(),
        })
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Text(text) => {
                let index = self.open_block(OpenBlock::Text, BlockStart::Text);
                self.pending.push_back(StreamEvent::TextDelta(TextDelta {
                    index,
                    text,
                    extensions: Extensions::new(),
                }));
            }
            Event::ToolStart { id, name } => {
                let name = self.names.get(&name).cloned().unwrap_or(name);
                self.close_block();
                self.open_block(OpenBlock::Tool, BlockStart::ToolCall { id, name });
            }
            Event::ToolArguments(partial_json) => {
                if let Some((OpenBlock::Tool, index)) = self.open {
                    self.pending
                        .push_back(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                            index,
                            partial_json,
                            extensions: Extensions::new(),
                        }));
                }
            }
            Event::Stop(reason) => {
                self.close_block();
                self.pending
                    .push_back(StreamEvent::MessageDelta(MessageDelta {
                        stop: Some(Stop {
                            reason,
                            sequence: None,
                        }),
                        usage: None,
                        extensions: Extensions::new(),
                    }));
                self.pending
                    .push_back(StreamEvent::MessageStop(MessageStop::default()));
                self.finished = true;
            }
        }
    }

    fn open_block(&mut self, kind: OpenBlock, block: BlockStart) -> u32 {
        if self.open.is_some_and(|(open, _)| open == kind) {
            return self.open.expect("open block was checked").1;
        }
        self.close_block();
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some((kind, index));
        self.pending
            .push_back(StreamEvent::ContentBlockStart(ContentBlockStart {
                index,
                block,
                extensions: Extensions::new(),
            }));
        index
    }

    fn close_block(&mut self) {
        if let Some((_, index)) = self.open.take() {
            self.pending
                .push_back(StreamEvent::ContentBlockStop(ContentBlockStop {
                    index,
                    extensions: Extensions::new(),
                }));
        }
    }
}
