//! OpenAI Chat Completions codec.
//!
//! Text is block index 0, reasoning is block index 1, and a tool call uses
//! index `2 + tool_calls[].index`. `data: [DONE]` closes open blocks and ends
//! the turn. See `docs/codecs.md` for lossy fields.

use std::collections::BTreeSet;

use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Effort, Extensions,
    GenerationParams, ImageDetail, ImagePart, ImageSource, Message, MessageDelta, MessageStart,
    MessageStop, Ping, Request, Response, Role, Stop, StopReason, StreamEvent, TextDelta, TextPart,
    ThinkingDelta, Tool, ToolArguments, ToolArgumentsDelta, ToolCallPart, ToolChoice,
    ToolChoiceMode, ToolResultPart, UnknownEvent, UnknownPart, Usage,
};
use serde_json::Value;

use crate::error::CodecError;
use crate::jsonutil::{
    Object, combine_extensions, expect_array, expect_object, insert_f64, insert_i64, insert_u64,
    parse_object, require_string, take_bool, take_f64, take_i64, take_object, take_string,
    take_string_list, take_u64, to_bytes,
};
use crate::sse::{self, SseFrame, SseParser};
use crate::{Codec, StreamDecoder};

const TEXT_INDEX: u32 = 0;
const THINKING_INDEX: u32 = 1;
const TOOL_INDEX_BASE: u32 = 2;

/// Codec for the OpenAI Chat Completions protocol.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAiChat;

impl OpenAiChat {
    /// Adapter policy for a streamed Chat Completions upstream.
    ///
    /// Sets `stream_options.include_usage` when the body asks for a stream, so
    /// the upstream returns token counts. A codec round trip does not call this.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when `bytes` is not a JSON object.
    pub fn prepare_upstream_body(bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
        let mut object = parse_object(bytes)?;
        if object.get("stream") == Some(&Value::Bool(true)) {
            let mut options = match object.remove("stream_options") {
                Some(Value::Object(options)) => options,
                _ => Object::new(),
            };
            options.insert("include_usage".to_owned(), Value::Bool(true));
            object.insert("stream_options".to_owned(), Value::Object(options));
        }
        to_bytes(&Value::Object(object))
    }
}

impl Codec for OpenAiChat {
    fn id(&self) -> &'static str {
        "openai-chat"
    }

    fn decode_request(&self, bytes: &[u8]) -> Result<Request, CodecError> {
        decode_request(bytes)
    }

    fn encode_request(&self, request: &Request) -> Result<Vec<u8>, CodecError> {
        encode_request(request)
    }

    fn decode_response(&self, bytes: &[u8]) -> Result<Response, CodecError> {
        decode_response(bytes)
    }

    fn encode_response(&self, response: &Response) -> Result<Vec<u8>, CodecError> {
        encode_response(response)
    }

    fn encode_stream(&self, events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
        encode_stream(events)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(Decoder::default())
    }

    fn stream_encoder(&self) -> Box<dyn crate::StreamEncoder> {
        Box::new(Encoder::default())
    }

    fn stream_encoder_for(&self, request: &Request) -> Box<dyn crate::StreamEncoder> {
        Box::new(OpenAiChat::stream_encoder_for_request(request))
    }

    fn encode_error(&self, status: u16, message: &str) -> Vec<u8> {
        encode_error(status, message)
    }

    fn encode_stream_error(&self, status: u16, message: &str) -> Vec<u8> {
        crate::sse::data_frame(&self.encode_error(status, message))
    }

    fn severed_stream_end(&self) -> &'static [u8] {
        b"data: [DONE]\n\n"
    }

    fn encode_stream_failure(&self, message: &str) -> Vec<u8> {
        let body = serde_json::json!({"error": {"message": message, "type": "api_error"}});
        crate::sse::data_frame(body.to_string().as_bytes())
    }
}

#[derive(Debug, Default)]
struct Decoder {
    parser: SseParser,
    started: bool,
    text_open: bool,
    thinking_open: bool,
    tools_open: BTreeSet<u32>,
    /// A stop delta with no usage, held until the next chunk so a following
    /// usage-only chunk can join it.
    held: Option<MessageDelta>,
}

impl StreamDecoder for Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.push(chunk)?;
        self.frames(frames)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.finish()?;
        let mut events = self.frames(frames)?;
        events.extend(self.flush_held());
        Ok(events)
    }
}

impl Decoder {
    fn frames(&mut self, frames: Vec<SseFrame>) -> Result<Vec<StreamEvent>, CodecError> {
        let mut events = Vec::new();
        for frame in frames {
            events.extend(self.frame(frame)?);
        }
        Ok(events)
    }

    fn frame(&mut self, frame: SseFrame) -> Result<Vec<StreamEvent>, CodecError> {
        if frame.data.trim().is_empty() {
            return Ok(Vec::new());
        }
        if frame.data.trim() == "[DONE]" {
            let mut events = self.flush_held();
            events.extend(self.finish_blocks());
            return Ok(events);
        }
        let value: Value = serde_json::from_str(&frame.data)?;
        let decoded = match value {
            Value::Object(object) => self.decode_chunk(object)?,
            other => vec![StreamEvent::Unknown(UnknownEvent {
                name: "data".to_owned(),
                payload: other,
            })],
        };
        Ok(self.absorb(decoded))
    }

    fn flush_held(&mut self) -> Vec<StreamEvent> {
        self.held
            .take()
            .map(|delta| vec![StreamEvent::MessageDelta(delta)])
            .unwrap_or_default()
    }

    fn absorb(&mut self, mut decoded: Vec<StreamEvent>) -> Vec<StreamEvent> {
        if let Some(held) = self.held.take() {
            if let Some(usage) = usage_only(&decoded) {
                let mut held = held;
                held.usage = usage.usage.clone();
                held.extensions.absorb(&usage.extensions);
                return vec![StreamEvent::MessageDelta(held)];
            }
            let mut out = vec![StreamEvent::MessageDelta(held)];
            out.extend(self.absorb(decoded));
            return out;
        }
        if let Some(StreamEvent::MessageDelta(delta)) = decoded.last()
            && delta.stop.is_some()
            && delta.usage.is_none()
        {
            let Some(StreamEvent::MessageDelta(delta)) = decoded.pop() else {
                return decoded;
            };
            self.held = Some(delta);
        }
        decoded
    }
}

fn usage_only(events: &[StreamEvent]) -> Option<&MessageDelta> {
    match events {
        [StreamEvent::MessageDelta(delta)] if delta.stop.is_none() && delta.usage.is_some() => {
            Some(delta)
        }
        _ => None,
    }
}

impl Decoder {
    fn finish_blocks(&mut self) -> Vec<StreamEvent> {
        let mut open = self.tools_open.clone();
        if self.text_open {
            open.insert(TEXT_INDEX);
        }
        if self.thinking_open {
            open.insert(THINKING_INDEX);
        }
        self.text_open = false;
        self.thinking_open = false;
        self.tools_open.clear();
        let mut events = Vec::new();
        for index in open {
            events.push(StreamEvent::ContentBlockStop(ContentBlockStop {
                index,
                extensions: Extensions::new(),
            }));
        }
        events.push(StreamEvent::MessageStop(MessageStop::default()));
        events
    }

    fn decode_chunk(&mut self, mut object: Object) -> Result<Vec<StreamEvent>, CodecError> {
        if !is_chat_chunk(&object) {
            return Ok(vec![decode_non_chat(object)]);
        }
        let id = take_string(&mut object, "id")?;
        let model = take_string(&mut object, "model")?;
        let _ = object.remove("object");
        let choices = match object.remove("choices") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items,
            Some(_) => return Err(CodecError::WrongType { field: "choices" }),
        };
        if choices.len() > 1 {
            return Err(CodecError::Unsupported {
                message: "only choices[0] is supported".to_owned(),
            });
        }
        let usage = match take_object(&mut object, "usage")? {
            Some(usage) => Some(decode_usage(usage)?),
            None => None,
        };
        let mut events = Vec::new();
        if !self.started && (id.is_some() || model.is_some()) {
            events.push(StreamEvent::MessageStart(MessageStart {
                id,
                model,
                usage: None,
                extensions: Extensions::new(),
            }));
            self.started = true;
        }
        if let Some(choice) = choices.into_iter().next() {
            self.decode_choice(&mut events, &mut object, choice)?;
        }
        if let Some(usage) = usage {
            attach_usage(&mut events, usage);
        }
        attach_extensions(&mut events, Extensions::from_map(object));
        Ok(events)
    }

    fn decode_choice(
        &mut self,
        events: &mut Vec<StreamEvent>,
        extra: &mut Object,
        choice: Value,
    ) -> Result<(), CodecError> {
        let mut choice = expect_object(choice, "choices")?;
        if let Some(index) = take_u64(&mut choice, "index")?
            && index != 0
        {
            return Err(CodecError::Unsupported {
                message: format!("choice index {index} is not supported"),
            });
        }
        let finish = take_string(&mut choice, "finish_reason")?;
        let delta = take_object(&mut choice, "delta")?.unwrap_or_default();
        for (key, value) in choice {
            extra.entry(key).or_insert(value);
        }
        self.decode_delta(events, delta)?;
        if let Some(finish) = finish {
            events.push(StreamEvent::MessageDelta(MessageDelta {
                stop: Some(Stop {
                    reason: decode_finish(&finish),
                    sequence: None,
                }),
                usage: None,
                extensions: Extensions::new(),
            }));
        }
        Ok(())
    }

    fn decode_delta(
        &mut self,
        events: &mut Vec<StreamEvent>,
        mut delta: Object,
    ) -> Result<(), CodecError> {
        let _role = take_string(&mut delta, "role")?;
        if let Some(content) = take_content_string(&mut delta)?
            && !content.is_empty()
        {
            if !self.text_open {
                events.push(block_start(TEXT_INDEX, BlockStart::Text));
                self.text_open = true;
            }
            events.push(StreamEvent::TextDelta(TextDelta {
                index: TEXT_INDEX,
                text: content,
                extensions: Extensions::new(),
            }));
        }
        if let Some(reasoning) = take_string(&mut delta, "reasoning_content")?
            && !reasoning.is_empty()
        {
            if !self.thinking_open {
                events.push(block_start(
                    THINKING_INDEX,
                    BlockStart::Thinking {
                        redacted: false,
                        data: None,
                    },
                ));
                self.thinking_open = true;
            }
            events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                index: THINKING_INDEX,
                text: reasoning,
                extensions: Extensions::new(),
            }));
        }
        if let Some(calls) = delta.remove("tool_calls") {
            for call in expect_array(calls, "tool_calls")? {
                self.decode_tool_delta(events, call)?;
            }
        }
        if !delta.is_empty() {
            attach_extensions(events, Extensions::from_map(delta));
        }
        Ok(())
    }

    fn decode_tool_delta(
        &mut self,
        events: &mut Vec<StreamEvent>,
        value: Value,
    ) -> Result<(), CodecError> {
        let mut object = expect_object(value, "tool_calls")?;
        let index = take_u64(&mut object, "index")?.ok_or(CodecError::MissingField("index"))?;
        let index = u32::try_from(index).map_err(|_| CodecError::Unsupported {
            message: "tool call index does not fit in u32".to_owned(),
        })?;
        let ir_index =
            index
                .checked_add(TOOL_INDEX_BASE)
                .ok_or_else(|| CodecError::Unsupported {
                    message: "tool call index is too large".to_owned(),
                })?;
        let id = take_string(&mut object, "id")?.unwrap_or_default();
        if let Some(kind) = take_string(&mut object, "type")?
            && kind != "function"
        {
            return Err(CodecError::Unsupported {
                message: format!("tool type `{kind}` is not supported"),
            });
        }
        let mut function = take_object(&mut object, "function")?.unwrap_or_default();
        let name = take_string(&mut function, "name")?.unwrap_or_default();
        let arguments = take_string(&mut function, "arguments")?.unwrap_or_default();
        let extensions = combine_extensions(function, object);
        if !self.tools_open.contains(&ir_index) {
            events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
                index: ir_index,
                block: BlockStart::ToolCall { id, name },
                extensions,
            }));
            self.tools_open.insert(ir_index);
            if !arguments.is_empty() {
                events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                    index: ir_index,
                    partial_json: arguments,
                    extensions: Extensions::new(),
                }));
            }
        } else if !arguments.is_empty() {
            events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: ir_index,
                partial_json: arguments,
                extensions,
            }));
        } else if !extensions.is_empty() {
            events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: ir_index,
                partial_json: String::new(),
                extensions,
            }));
        }
        Ok(())
    }
}

fn is_chat_chunk(object: &Object) -> bool {
    object.contains_key("choices")
        || object.contains_key("usage")
        || object
            .get("object")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("chat.completion"))
}

fn decode_non_chat(mut object: Object) -> StreamEvent {
    let kind = match object.remove("type") {
        Some(Value::String(text)) => Some(text),
        Some(other) => {
            object.insert("type".to_owned(), other);
            None
        }
        None => None,
    };
    match kind.as_deref() {
        Some("ping") => StreamEvent::Ping(Ping {
            extensions: Extensions::from_map(object),
        }),
        Some(name) => {
            if object.len() == 1
                && let Some(payload) = object.remove("payload")
            {
                return StreamEvent::Unknown(UnknownEvent {
                    name: name.to_owned(),
                    payload,
                });
            }
            object.insert("type".to_owned(), Value::String(name.to_owned()));
            StreamEvent::Unknown(UnknownEvent {
                name: name.to_owned(),
                payload: Value::Object(object),
            })
        }
        None => StreamEvent::Unknown(UnknownEvent {
            name: "data".to_owned(),
            payload: Value::Object(object),
        }),
    }
}

fn block_start(index: u32, block: BlockStart) -> StreamEvent {
    StreamEvent::ContentBlockStart(ContentBlockStart {
        index,
        block,
        extensions: Extensions::new(),
    })
}

fn attach_usage(events: &mut Vec<StreamEvent>, usage: Usage) {
    if let Some(StreamEvent::MessageDelta(delta)) = events.last_mut()
        && delta.usage.is_none()
    {
        delta.usage = Some(usage);
        return;
    }
    events.push(StreamEvent::MessageDelta(MessageDelta {
        stop: None,
        usage: Some(usage),
        extensions: Extensions::new(),
    }));
}

fn attach_extensions(events: &mut Vec<StreamEvent>, extra: Extensions) {
    if extra.is_empty() {
        return;
    }
    if let Some(event) = events.iter_mut().find(|event| survives(event)) {
        add_extensions(event, extra);
        return;
    }
    events.push(StreamEvent::MessageDelta(MessageDelta {
        stop: None,
        usage: None,
        extensions: extra,
    }));
}

fn survives(event: &StreamEvent) -> bool {
    match event {
        StreamEvent::ContentBlockStart(start) => {
            matches!(
                start.block,
                BlockStart::ToolCall { .. } | BlockStart::Unknown { .. }
            )
        }
        StreamEvent::TextDelta(delta) => !delta.text.is_empty(),
        StreamEvent::ThinkingDelta(delta) => !delta.text.is_empty(),
        StreamEvent::ToolArgumentsDelta(delta) => {
            !delta.partial_json.is_empty() || !delta.extensions.is_empty()
        }
        StreamEvent::ContentBlockStop(_)
        | StreamEvent::SignatureDelta(_)
        | StreamEvent::MessageStop(_) => false,
        StreamEvent::MessageStart(_)
        | StreamEvent::MessageDelta(_)
        | StreamEvent::Ping(_)
        | StreamEvent::Unknown(_) => true,
    }
}

fn add_extensions(event: &mut StreamEvent, extra: Extensions) {
    let Some(slot) = event_extensions_mut(event) else {
        return;
    };
    slot.absorb(&extra);
}

fn event_extensions_mut(event: &mut StreamEvent) -> Option<&mut Extensions> {
    Some(match event {
        StreamEvent::MessageStart(event) => &mut event.extensions,
        StreamEvent::ContentBlockStart(event) => &mut event.extensions,
        StreamEvent::TextDelta(event) => &mut event.extensions,
        StreamEvent::ThinkingDelta(event) => &mut event.extensions,
        StreamEvent::SignatureDelta(event) => &mut event.extensions,
        StreamEvent::ToolArgumentsDelta(event) => &mut event.extensions,
        StreamEvent::ContentBlockStop(event) => &mut event.extensions,
        StreamEvent::MessageDelta(event) => &mut event.extensions,
        StreamEvent::MessageStop(event) => &mut event.extensions,
        StreamEvent::Ping(event) => &mut event.extensions,
        StreamEvent::Unknown(_) => return None,
    })
}

fn decode_request(bytes: &[u8]) -> Result<Request, CodecError> {
    let mut object = parse_object(bytes)?;
    let model = require_string(&mut object, "model")?;
    let raw_messages = match object.remove("messages") {
        Some(value) => expect_array(value, "messages")?,
        None => return Err(CodecError::MissingField("messages")),
    };
    let mut messages = Vec::new();
    for message in raw_messages {
        messages.push(decode_message(message)?);
    }
    let tools = decode_tools(object.remove("tools"))?;
    let tool_choice = match object.remove("tool_choice") {
        Some(value) if !value.is_null() => Some(decode_tool_choice(value)?),
        _ => None,
    };
    let max_completion = take_u64(&mut object, "max_completion_tokens")?;
    let max_tokens = take_u64(&mut object, "max_tokens")?;
    let effort = object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .and_then(Effort::from_label);
    let original_max_tokens = max_completion.and(max_tokens);
    let mut params = GenerationParams {
        max_tokens: max_completion.or(max_tokens),
        temperature: take_f64(&mut object, "temperature")?,
        top_p: take_f64(&mut object, "top_p")?,
        stop: take_string_list(&mut object, "stop")?,
        seed: take_i64(&mut object, "seed")?,
        presence_penalty: take_f64(&mut object, "presence_penalty")?,
        frequency_penalty: take_f64(&mut object, "frequency_penalty")?,
        effort,
        ..GenerationParams::default()
    };
    if let Some(original) = original_max_tokens {
        params
            .extensions
            .insert("originalChatMaxTokens", Value::from(original));
    } else if max_tokens.is_some() {
        params.extensions.insert(
            "originalChatTokenField",
            Value::String("max_tokens".to_owned()),
        );
    }
    let stream = take_bool(&mut object, "stream")?.unwrap_or(false);
    Ok(Request::new(model)
        .with_messages(messages)
        .with_tools(tools)
        .with_tool_choice(tool_choice)
        .with_params(params)
        .with_stream(stream)
        .with_extensions(Extensions::from_map(object)))
}

fn encode_request(request: &Request) -> Result<Vec<u8>, CodecError> {
    let mut object = Object::new();
    object.insert("model".to_owned(), Value::String(request.model.clone()));
    object.insert(
        "messages".to_owned(),
        Value::Array(encode_messages(&request.messages)?),
    );
    if !request.tools.is_empty() {
        object.insert(
            "tools".to_owned(),
            Value::Array(encode_tools(&request.tools)?),
        );
    }
    if let Some(choice) = &request.tool_choice {
        object.insert("tool_choice".to_owned(), encode_tool_choice(choice));
    }
    let token_field = if request
        .params
        .extensions
        .get("originalChatMaxTokens")
        .is_none()
        && request
            .params
            .extensions
            .get("originalChatTokenField")
            .and_then(Value::as_str)
            == Some("max_tokens")
    {
        "max_tokens"
    } else {
        "max_completion_tokens"
    };
    insert_u64(&mut object, token_field, request.params.max_tokens);
    insert_f64(&mut object, "temperature", request.params.temperature)?;
    insert_f64(&mut object, "top_p", request.params.top_p)?;
    if !request.params.stop.is_empty() {
        object.insert(
            "stop".to_owned(),
            Value::Array(
                request
                    .params
                    .stop
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    insert_i64(&mut object, "seed", request.params.seed);
    insert_f64(
        &mut object,
        "presence_penalty",
        request.params.presence_penalty,
    )?;
    insert_f64(
        &mut object,
        "frequency_penalty",
        request.params.frequency_penalty,
    )?;
    if request.stream {
        object.insert("stream".to_owned(), Value::Bool(true));
    }
    write_chat_effort(&mut object, request);
    request.extensions.merge_into(&mut object);
    for (key, value) in request.params.extensions.iter() {
        if !matches!(
            key.as_str(),
            "originalChatMaxTokens" | "originalChatTokenField"
        ) {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    to_bytes(&Value::Object(object))
}

fn write_chat_effort(object: &mut Object, request: &Request) {
    let Some(effort) = request.params.effort else {
        return;
    };
    if request.extensions.get("reasoning_effort").is_some() {
        return;
    }
    object.insert(
        "reasoning_effort".to_owned(),
        Value::String(effort.openai_label().to_owned()),
    );
}

fn decode_response(bytes: &[u8]) -> Result<Response, CodecError> {
    let mut object = parse_object(bytes)?;
    let _ = object.remove("object");
    let id = take_string(&mut object, "id")?;
    let model = take_string(&mut object, "model")?;
    let choices = match object.remove("choices") {
        None => return Err(CodecError::MissingField("choices")),
        Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(CodecError::WrongType { field: "choices" }),
    };
    if choices.len() > 1 {
        return Err(CodecError::Unsupported {
            message: "only choices[0] is supported".to_owned(),
        });
    }
    let usage = match take_object(&mut object, "usage")? {
        Some(usage) => Some(decode_usage(usage)?),
        None => None,
    };
    let (message, stop, choice_extra) = match choices.into_iter().next() {
        Some(choice) => decode_response_choice(choice)?,
        None => (Message::new(Role::Assistant), None, Object::new()),
    };
    let mut extensions = Extensions::from_map(object);
    extensions.absorb(&Extensions::from_map(choice_extra));
    Ok(Response::new(message)
        .with_id(id)
        .with_model(model)
        .with_stop(stop)
        .with_usage(usage)
        .with_extensions(extensions))
}

fn decode_response_choice(choice: Value) -> Result<(Message, Option<Stop>, Object), CodecError> {
    let mut choice = expect_object(choice, "choices")?;
    if let Some(index) = take_u64(&mut choice, "index")?
        && index != 0
    {
        return Err(CodecError::Unsupported {
            message: format!("choice index {index} is not supported"),
        });
    }
    let finish = take_string(&mut choice, "finish_reason")?;
    let mut message = take_object(&mut choice, "message")?.unwrap_or_default();
    if !message.contains_key("role") {
        message.insert("role".to_owned(), Value::String("assistant".to_owned()));
    }
    let message = decode_message(Value::Object(message))?;
    if message.role != Role::Assistant {
        return Err(CodecError::Unsupported {
            message: "Chat Completions responses are assistant turns".to_owned(),
        });
    }
    let stop = finish.map(|reason| Stop {
        reason: decode_finish(&reason),
        sequence: None,
    });
    Ok((message, stop, choice))
}

fn encode_response(response: &Response) -> Result<Vec<u8>, CodecError> {
    if response.message.role != Role::Assistant {
        return Err(CodecError::Unsupported {
            message: "Chat Completions responses are assistant turns".to_owned(),
        });
    }
    let mut wrapper = encode_assistant(&response.message)?;
    let message = wrapper
        .remove("message")
        .ok_or(CodecError::MissingField("message"))?;
    let mut choice = Object::new();
    choice.insert("index".to_owned(), Value::from(0));
    choice.insert("message".to_owned(), message);
    if let Some(stop) = &response.stop {
        choice.insert(
            "finish_reason".to_owned(),
            Value::String(encode_finish(&stop.reason).to_owned()),
        );
    }
    let mut object = Object::new();
    if let Some(id) = &response.id {
        object.insert("id".to_owned(), Value::String(id.clone()));
    }
    object.insert(
        "object".to_owned(),
        Value::String("chat.completion".to_owned()),
    );
    if let Some(model) = &response.model {
        object.insert("model".to_owned(), Value::String(model.clone()));
    }
    object.insert(
        "choices".to_owned(),
        Value::Array(vec![Value::Object(choice)]),
    );
    if let Some(usage) = &response.usage {
        object.insert("usage".to_owned(), encode_usage(usage));
    }
    response.extensions.merge_into(&mut object);
    to_bytes(&Value::Object(object))
}

fn decode_message(value: Value) -> Result<Message, CodecError> {
    let mut object = expect_object(value, "messages")?;
    let role = match require_string(&mut object, "role")?.as_str() {
        "system" => Role::System,
        "developer" => Role::Developer,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => return Err(CodecError::UnknownRole(other.to_owned())),
    };
    if role == Role::Tool {
        let id = require_string(&mut object, "tool_call_id")?;
        let content = decode_body(object.remove("content"), "content")?;
        return Ok(Message {
            role,
            content: vec![ContentPart::ToolResult(ToolResultPart {
                id,
                is_error: false,
                content,
                extensions: Extensions::new(),
            })],
            extensions: Extensions::from_map(object),
        });
    }
    let reasoning = take_string(&mut object, "reasoning_content")?;
    let tool_calls = match object.remove("tool_calls") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(CodecError::WrongType {
                field: "tool_calls",
            });
        }
    };
    let body = decode_body(object.remove("content"), "content")?;
    let mut content = Vec::new();
    if let Some(reasoning) = reasoning.filter(|text| !text.is_empty()) {
        content.push(ContentPart::thinking(reasoning));
    }
    content.extend(body);
    for call in tool_calls {
        content.push(decode_tool_call(call)?);
    }
    Ok(Message {
        role,
        content,
        extensions: Extensions::from_map(object),
    })
}

fn encode_messages(messages: &[Message]) -> Result<Vec<Value>, CodecError> {
    let mut encoded = Vec::new();
    for message in messages {
        encoded.extend(encode_message(message)?);
    }
    Ok(encoded)
}

fn encode_message(message: &Message) -> Result<Vec<Value>, CodecError> {
    match message.role {
        Role::System => Ok(vec![plain_role("system", message)?]),
        Role::Developer => Ok(vec![plain_role("developer", message)?]),
        Role::User => encode_user(message),
        Role::Assistant => {
            let mut object = encode_assistant(message)?;
            let message_value = object
                .remove("message")
                .ok_or(CodecError::MissingField("message"))?;
            Ok(vec![message_value])
        }
        Role::Tool => encode_tool_role(message),
    }
}

fn plain_role(role: &str, message: &Message) -> Result<Value, CodecError> {
    let mut object = Object::new();
    object.insert("role".to_owned(), Value::String(role.to_owned()));
    if let Some(content) = encode_generic_content(&message.content)? {
        object.insert("content".to_owned(), content);
    }
    message.extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn encode_user(message: &Message) -> Result<Vec<Value>, CodecError> {
    let mut encoded = Vec::new();
    let mut pending = Vec::new();
    let mut attach = true;
    for part in &message.content {
        if let ContentPart::ToolResult(result) = part {
            if !pending.is_empty() {
                encoded.push(user_message(&pending, message, &mut attach)?);
                pending.clear();
            }
            encoded.push(tool_message(result, message, &mut attach)?);
        } else {
            pending.push(part);
        }
    }
    if !pending.is_empty() || encoded.is_empty() {
        encoded.push(user_message(&pending, message, &mut attach)?);
    }
    Ok(encoded)
}

fn user_message(
    parts: &[&ContentPart],
    message: &Message,
    attach: &mut bool,
) -> Result<Value, CodecError> {
    let owned: Vec<ContentPart> = parts.iter().map(|part| (*part).clone()).collect();
    let mut object = Object::new();
    object.insert("role".to_owned(), Value::String("user".to_owned()));
    if let Some(content) = encode_generic_content(&owned)? {
        object.insert("content".to_owned(), content);
    }
    if *attach {
        message.extensions.merge_into(&mut object);
        *attach = false;
    }
    Ok(Value::Object(object))
}

fn encode_tool_role(message: &Message) -> Result<Vec<Value>, CodecError> {
    let mut encoded = Vec::new();
    let mut attach = true;
    for part in &message.content {
        match part {
            ContentPart::ToolResult(result) => {
                encoded.push(tool_message(result, message, &mut attach)?);
            }
            _ => {
                return Err(CodecError::Unsupported {
                    message: "a tool message can only contain tool results".to_owned(),
                });
            }
        }
    }
    if encoded.is_empty() {
        return Err(CodecError::MissingField("tool_call_id"));
    }
    Ok(encoded)
}

fn tool_message(
    result: &ToolResultPart,
    message: &Message,
    attach: &mut bool,
) -> Result<Value, CodecError> {
    let mut object = Object::new();
    object.insert("role".to_owned(), Value::String("tool".to_owned()));
    object.insert("tool_call_id".to_owned(), Value::String(result.id.clone()));
    if let Some(content) = encode_generic_content(&result.content)? {
        object.insert("content".to_owned(), content);
    }
    result.extensions.merge_into(&mut object);
    if *attach {
        message.extensions.merge_into(&mut object);
        *attach = false;
    }
    Ok(Value::Object(object))
}

fn encode_assistant(message: &Message) -> Result<Object, CodecError> {
    let mut reasoning = String::new();
    let mut saw_reasoning = false;
    let mut calls = Vec::new();
    let mut body = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Thinking(part) => {
                if !part.redacted && !part.text.is_empty() {
                    reasoning.push_str(&part.text);
                    saw_reasoning = true;
                }
            }
            ContentPart::ToolCall(call) => calls.push(encode_tool_call(call)?),
            ContentPart::ToolResult(_) => {
                return Err(CodecError::Unsupported {
                    message: "a tool result cannot be encoded on an assistant message".to_owned(),
                });
            }
            other => body.push(other),
        }
    }
    let mut object = Object::new();
    object.insert("role".to_owned(), Value::String("assistant".to_owned()));
    if let Some(content) = encode_assistant_body(&body)? {
        object.insert("content".to_owned(), content);
    }
    if saw_reasoning {
        object.insert("reasoning_content".to_owned(), Value::String(reasoning));
    }
    if !calls.is_empty() {
        object.insert("tool_calls".to_owned(), Value::Array(calls));
    }
    message.extensions.merge_into(&mut object);
    let mut wrapper = Object::new();
    wrapper.insert("message".to_owned(), Value::Object(object));
    Ok(wrapper)
}

fn encode_assistant_body(parts: &[&ContentPart]) -> Result<Option<Value>, CodecError> {
    if parts.is_empty() {
        return Ok(None);
    }
    if parts
        .iter()
        .all(|part| matches!(part, ContentPart::Text(_)))
    {
        let mut text = String::new();
        for part in parts {
            if let ContentPart::Text(part) = part {
                text.push_str(&part.text);
            }
        }
        return Ok(Some(Value::String(text)));
    }
    let owned: Vec<ContentPart> = parts.iter().map(|part| (*part).clone()).collect();
    encode_generic_content(&owned)
}

fn encode_generic_content(parts: &[ContentPart]) -> Result<Option<Value>, CodecError> {
    if parts.is_empty() {
        return Ok(None);
    }
    if let Some(text) = single_plain_text(parts) {
        return Ok(Some(Value::String(text.to_owned())));
    }
    let mut items = Vec::new();
    for part in parts {
        items.push(encode_content_part(part)?);
    }
    Ok(Some(Value::Array(items)))
}

fn single_plain_text(parts: &[ContentPart]) -> Option<&str> {
    if let [ContentPart::Text(part)] = parts
        && part.extensions.is_empty()
    {
        return Some(part.text.as_str());
    }
    None
}

fn encode_content_part(part: &ContentPart) -> Result<Value, CodecError> {
    let mut object = Object::new();
    match part {
        ContentPart::Text(part) => {
            object.insert("type".to_owned(), Value::String("text".to_owned()));
            object.insert("text".to_owned(), Value::String(part.text.clone()));
            part.extensions.merge_into(&mut object);
        }
        ContentPart::Image(part) => return encode_image(part),
        ContentPart::Unknown(part) => {
            object.insert("type".to_owned(), Value::String(part.kind.clone()));
            part.extensions.merge_into(&mut object);
        }
        ContentPart::ToolCall(_) | ContentPart::ToolResult(_) | ContentPart::Thinking(_) => {
            return Err(CodecError::Unsupported {
                message: "this content part has its own Chat Completions field".to_owned(),
            });
        }
    }
    Ok(Value::Object(object))
}

fn encode_image(part: &ImagePart) -> Result<Value, CodecError> {
    let url = match &part.source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    };
    let mut image = Object::new();
    image.insert("url".to_owned(), Value::String(url));
    if let Some(detail) = &part.detail {
        image.insert("detail".to_owned(), serde_json::to_value(detail)?);
    }
    let mut object = Object::new();
    object.insert("type".to_owned(), Value::String("image_url".to_owned()));
    object.insert("image_url".to_owned(), Value::Object(image));
    part.extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn encode_tool_call(call: &ToolCallPart) -> Result<Value, CodecError> {
    let mut function = Object::new();
    function.insert("name".to_owned(), Value::String(call.name.clone()));
    function.insert(
        "arguments".to_owned(),
        Value::String(call.arguments.to_text()),
    );
    let mut object = Object::new();
    object.insert("id".to_owned(), Value::String(call.id.clone()));
    object.insert("type".to_owned(), Value::String("function".to_owned()));
    object.insert("function".to_owned(), Value::Object(function));
    call.extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn decode_body(value: Option<Value>, field: &'static str) -> Result<Vec<ContentPart>, CodecError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![ContentPart::text(text)]),
        Some(Value::Array(items)) => items.into_iter().map(decode_part).collect(),
        Some(_) => Err(CodecError::WrongType { field }),
    }
}

fn decode_part(value: Value) -> Result<ContentPart, CodecError> {
    let mut object = expect_object(value, "content")?;
    let kind = require_string(&mut object, "type")?;
    match kind.as_str() {
        "text" => Ok(ContentPart::Text(TextPart {
            text: require_string(&mut object, "text")?,
            extensions: Extensions::from_map(object),
        })),
        "image_url" => decode_image(object),
        other => Ok(ContentPart::Unknown(UnknownPart {
            kind: other.to_owned(),
            extensions: Extensions::from_map(object),
        })),
    }
}

fn decode_image(mut object: Object) -> Result<ContentPart, CodecError> {
    let mut image =
        take_object(&mut object, "image_url")?.ok_or(CodecError::MissingField("image_url"))?;
    let url = require_string(&mut image, "url")?;
    let detail = match image.remove("detail") {
        None | Some(Value::Null) => None,
        Some(value) => Some(decode_detail(value)?),
    };
    Ok(ContentPart::Image(ImagePart {
        source: decode_image_source(url),
        detail,
        extensions: combine_extensions(image, object),
    }))
}

fn decode_detail(value: Value) -> Result<ImageDetail, CodecError> {
    serde_json::from_value(value).map_err(|_| CodecError::WrongType { field: "detail" })
}

fn decode_image_source(url: String) -> ImageSource {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((media_type, data)) = rest.split_once(";base64,")
        && !media_type.is_empty()
    {
        return ImageSource::Base64 {
            media_type: media_type.to_owned(),
            data: data.to_owned(),
        };
    }
    ImageSource::Url { url }
}

fn decode_tool_call(value: Value) -> Result<ContentPart, CodecError> {
    let mut object = expect_object(value, "tool_calls")?;
    let id = require_string(&mut object, "id")?;
    match take_string(&mut object, "type")? {
        Some(kind) if kind == "function" => {}
        Some(kind) => {
            return Err(CodecError::Unsupported {
                message: format!("tool type `{kind}` is not supported"),
            });
        }
        None => return Err(CodecError::MissingField("type")),
    }
    let mut function =
        take_object(&mut object, "function")?.ok_or(CodecError::MissingField("function"))?;
    let name = require_string(&mut function, "name")?;
    let arguments = take_string(&mut function, "arguments")?.unwrap_or_default();
    Ok(ContentPart::ToolCall(ToolCallPart {
        id,
        name,
        arguments: ToolArguments::from_text(&arguments),
        extensions: combine_extensions(function, object),
    }))
}

fn decode_tools(value: Option<Value>) -> Result<Vec<Tool>, CodecError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    expect_array(value, "tools")?
        .into_iter()
        .map(decode_tool)
        .collect()
}

fn decode_tool(value: Value) -> Result<Tool, CodecError> {
    let mut object = expect_object(value, "tools")?;
    match take_string(&mut object, "type")? {
        Some(kind) if kind == "function" => {}
        Some(kind) => {
            return Err(CodecError::Unsupported {
                message: format!("tool type `{kind}` is not supported"),
            });
        }
        None => return Err(CodecError::MissingField("type")),
    }
    let mut function =
        take_object(&mut object, "function")?.ok_or(CodecError::MissingField("function"))?;
    let name = require_string(&mut function, "name")?;
    let description = take_string(&mut function, "description")?;
    let input_schema = function
        .remove("parameters")
        .unwrap_or_else(|| Value::Object(Object::new()));
    Ok(Tool {
        name,
        description,
        input_schema,
        extensions: combine_extensions(function, object),
    })
}

fn encode_tools(tools: &[Tool]) -> Result<Vec<Value>, CodecError> {
    tools.iter().map(encode_tool).collect()
}

fn encode_tool(tool: &Tool) -> Result<Value, CodecError> {
    let mut function = Object::new();
    function.insert("name".to_owned(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        function.insert("description".to_owned(), Value::String(description.clone()));
    }
    function.insert("parameters".to_owned(), tool.input_schema.clone());
    let mut object = Object::new();
    object.insert("type".to_owned(), Value::String("function".to_owned()));
    object.insert("function".to_owned(), Value::Object(function));
    tool.extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn decode_tool_choice(value: Value) -> Result<ToolChoice, CodecError> {
    match value {
        Value::String(text) => tool_choice_from_name(&text, Object::new()),
        Value::Object(mut object) => {
            let kind = require_string(&mut object, "type")?;
            if kind == "function" {
                let mut function = take_object(&mut object, "function")?
                    .ok_or(CodecError::MissingField("function"))?;
                let name = require_string(&mut function, "name")?;
                return Ok(ToolChoice {
                    mode: ToolChoiceMode::Named { name },
                    extensions: combine_extensions(function, object),
                });
            }
            tool_choice_from_name(&kind, object)
        }
        _ => Err(CodecError::WrongType {
            field: "tool_choice",
        }),
    }
}

fn tool_choice_from_name(kind: &str, object: Object) -> Result<ToolChoice, CodecError> {
    let mode = match kind {
        "auto" => ToolChoiceMode::Auto,
        "none" => ToolChoiceMode::None,
        "required" => ToolChoiceMode::Required,
        other => {
            return Err(CodecError::Unsupported {
                message: format!("tool_choice `{other}` is not supported"),
            });
        }
    };
    Ok(ToolChoice {
        mode,
        extensions: Extensions::from_map(object),
    })
}

fn encode_tool_choice(choice: &ToolChoice) -> Value {
    match &choice.mode {
        ToolChoiceMode::Named { name } => named_tool_choice(name, &choice.extensions),
        ToolChoiceMode::Auto if choice.extensions.is_empty() => Value::String("auto".to_owned()),
        ToolChoiceMode::None if choice.extensions.is_empty() => Value::String("none".to_owned()),
        ToolChoiceMode::Required if choice.extensions.is_empty() => {
            Value::String("required".to_owned())
        }
        ToolChoiceMode::Auto => typed_choice("auto", &choice.extensions),
        ToolChoiceMode::None => typed_choice("none", &choice.extensions),
        ToolChoiceMode::Required => typed_choice("required", &choice.extensions),
    }
}

fn named_tool_choice(name: &str, extensions: &Extensions) -> Value {
    let mut object = Object::new();
    object.insert("type".to_owned(), Value::String("function".to_owned()));
    object.insert(
        "function".to_owned(),
        Value::Object({
            let mut function = Object::new();
            function.insert("name".to_owned(), Value::String(name.to_owned()));
            function
        }),
    );
    extensions.merge_into(&mut object);
    Value::Object(object)
}

fn typed_choice(kind: &str, extensions: &Extensions) -> Value {
    let mut object = Object::new();
    object.insert("type".to_owned(), Value::String(kind.to_owned()));
    extensions.merge_into(&mut object);
    Value::Object(object)
}

fn decode_usage(mut object: Object) -> Result<Usage, CodecError> {
    let input_tokens = take_u64(&mut object, "prompt_tokens")?;
    let output_tokens = take_u64(&mut object, "completion_tokens")?;
    let total_tokens = take_u64(&mut object, "total_tokens")?;
    let mut details = Object::new();
    if let Some(mut prompt) = take_object(&mut object, "prompt_tokens_details")? {
        let cached = take_u64(&mut prompt, "cached_tokens")?;
        for (key, value) in prompt {
            details.entry(key).or_insert(value);
        }
        let cache_read_tokens = cached;
        if let Some(mut completion) = take_object(&mut object, "completion_tokens_details")? {
            let reasoning = take_u64(&mut completion, "reasoning_tokens")?;
            for (key, value) in completion {
                details.entry(key).or_insert(value);
            }
            return usage_from(
                input_tokens,
                output_tokens,
                total_tokens,
                cache_read_tokens,
                reasoning,
                object,
                details,
            );
        }
        return usage_from(
            input_tokens,
            output_tokens,
            total_tokens,
            cache_read_tokens,
            None,
            object,
            details,
        );
    }
    if let Some(mut completion) = take_object(&mut object, "completion_tokens_details")? {
        let reasoning = take_u64(&mut completion, "reasoning_tokens")?;
        for (key, value) in completion {
            details.entry(key).or_insert(value);
        }
        return usage_from(
            input_tokens,
            output_tokens,
            total_tokens,
            None,
            reasoning,
            object,
            details,
        );
    }
    usage_from(
        input_tokens,
        output_tokens,
        total_tokens,
        None,
        None,
        object,
        details,
    )
}

fn usage_from(
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    object: Object,
    details: Object,
) -> Result<Usage, CodecError> {
    Ok(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_tokens,
        cache_write_tokens: None,
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        reasoning_tokens,
        extensions: combine_extensions(details, object),
    })
}

fn encode_usage(usage: &Usage) -> Value {
    let (input, output, total) = usage.filled_counts();
    let mut object = Object::new();
    object.insert("prompt_tokens".to_owned(), Value::from(input));
    object.insert("completion_tokens".to_owned(), Value::from(output));
    object.insert("total_tokens".to_owned(), Value::from(total));
    if let Some(cached) = usage.cache_read_tokens {
        object.insert(
            "prompt_tokens_details".to_owned(),
            Value::Object({
                let mut details = Object::new();
                details.insert("cached_tokens".to_owned(), Value::from(cached));
                details
            }),
        );
    }
    if let Some(reasoning) = usage.reasoning_tokens {
        object.insert(
            "completion_tokens_details".to_owned(),
            Value::Object({
                let mut details = Object::new();
                details.insert("reasoning_tokens".to_owned(), Value::from(reasoning));
                details
            }),
        );
    }
    usage.extensions.merge_into(&mut object);
    Value::Object(object)
}

fn decode_finish(token: &str) -> StopReason {
    match token {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "content_filter" | "sensitive" => StopReason::ContentFilter,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

fn encode_finish(reason: &StopReason) -> &str {
    match reason {
        StopReason::EndTurn | StopReason::StopSequence => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFilter => "content_filter",
        StopReason::Refusal => "refusal",
        StopReason::Other(token) => token.as_str(),
    }
}

fn take_content_string(object: &mut Object) -> Result<Option<String>, CodecError> {
    match object.remove("content") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(CodecError::WrongType { field: "content" }),
    }
}

fn encode_stream(events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder {
        include_usage: true,
        ..Encoder::default()
    };
    let mut out = Vec::new();
    for event in events {
        encoder.write(&mut out, event)?;
    }
    Ok(out)
}

#[derive(Default)]
struct Encoder {
    id: Option<String>,
    model: Option<String>,
    include_usage: bool,
    usage: Option<Usage>,
    usage_sent: bool,
}

impl OpenAiChat {
    fn stream_encoder_for_request(request: &Request) -> Encoder {
        Encoder {
            include_usage: client_wants_usage(request),
            ..Encoder::default()
        }
    }
}

fn client_wants_usage(request: &Request) -> bool {
    request
        .extensions
        .get("stream_options")
        .and_then(Value::as_object)
        .and_then(|options| options.get("include_usage"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

impl crate::StreamEncoder for Encoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        let mut out = Vec::new();
        self.write(&mut out, event)?;
        Ok(out)
    }
}

impl Encoder {
    fn write(&mut self, out: &mut Vec<u8>, event: &StreamEvent) -> Result<(), CodecError> {
        encode_stream_event(self, out, event)
    }
}

fn encode_stream_event(
    encoder: &mut Encoder,
    out: &mut Vec<u8>,
    event: &StreamEvent,
) -> Result<(), CodecError> {
    match event {
        StreamEvent::MessageStart(start) => {
            if start.id.is_some() {
                encoder.id.clone_from(&start.id);
            }
            if start.model.is_some() {
                encoder.model.clone_from(&start.model);
            }
            Usage::merge_patch(&mut encoder.usage, start.usage.as_ref());
            write_chunk(
                out,
                encoder.id.as_deref(),
                encoder.model.as_deref(),
                &start.extensions,
                {
                    let mut delta = Object::new();
                    delta.insert("role".to_owned(), Value::String("assistant".to_owned()));
                    delta
                },
                None,
                None,
            )
        }
        StreamEvent::ContentBlockStart(start) => encode_block_start(encoder, out, start),
        StreamEvent::TextDelta(delta) => {
            if delta.text.is_empty() {
                return Ok(());
            }
            expect_stream_index(delta.index, TEXT_INDEX, "text")?;
            let mut inner = Object::new();
            inner.insert("content".to_owned(), Value::String(delta.text.clone()));
            write_chunk(
                out,
                encoder.id.as_deref(),
                encoder.model.as_deref(),
                &delta.extensions,
                inner,
                None,
                None,
            )
        }
        StreamEvent::ThinkingDelta(delta) => {
            if delta.text.is_empty() {
                return Ok(());
            }
            expect_stream_index(delta.index, THINKING_INDEX, "thinking")?;
            let mut inner = Object::new();
            inner.insert(
                "reasoning_content".to_owned(),
                Value::String(delta.text.clone()),
            );
            write_chunk(
                out,
                encoder.id.as_deref(),
                encoder.model.as_deref(),
                &delta.extensions,
                inner,
                None,
                None,
            )
        }
        StreamEvent::SignatureDelta(_) => Ok(()),
        StreamEvent::ToolArgumentsDelta(delta) => {
            if delta.partial_json.is_empty() && delta.extensions.is_empty() {
                return Ok(());
            }
            let index = openai_tool_index(delta.index)?;
            let mut function = Object::new();
            function.insert(
                "arguments".to_owned(),
                Value::String(delta.partial_json.clone()),
            );
            let mut call = Object::new();
            call.insert("index".to_owned(), Value::from(index));
            call.insert("function".to_owned(), Value::Object(function));
            delta.extensions.merge_into(&mut call);
            let mut inner = Object::new();
            inner.insert(
                "tool_calls".to_owned(),
                Value::Array(vec![Value::Object(call)]),
            );
            write_chunk(
                out,
                encoder.id.as_deref(),
                encoder.model.as_deref(),
                &Extensions::new(),
                inner,
                None,
                None,
            )
        }
        StreamEvent::ContentBlockStop(_) => Ok(()),
        StreamEvent::MessageDelta(delta) => {
            let finish = delta.stop.as_ref().map(|stop| encode_finish(&stop.reason));
            Usage::merge_patch(&mut encoder.usage, delta.usage.as_ref());
            write_chunk(
                out,
                encoder.id.as_deref(),
                encoder.model.as_deref(),
                &delta.extensions,
                Object::new(),
                finish,
                None,
            )?;
            if encoder.include_usage
                && let Some(usage) = encoder.usage.as_ref()
            {
                write_usage_chunk(out, encoder.id.as_deref(), encoder.model.as_deref(), usage)?;
                encoder.usage_sent = true;
            }
            Ok(())
        }
        StreamEvent::MessageStop(_) => {
            if encoder.include_usage
                && !encoder.usage_sent
                && let Some(usage) = encoder.usage.clone()
            {
                write_usage_chunk(out, encoder.id.as_deref(), encoder.model.as_deref(), &usage)?;
                encoder.usage_sent = true;
            }
            sse::write_event(out, None, "[DONE]");
            Ok(())
        }
        StreamEvent::Ping(ping) => {
            let mut object = Object::new();
            object.insert("type".to_owned(), Value::String("ping".to_owned()));
            ping.extensions.merge_into(&mut object);
            write_data(out, &Value::Object(object))
        }
        StreamEvent::Unknown(event) => write_data(out, &encode_unknown(event)),
    }
}

fn encode_block_start(
    encoder: &Encoder,
    out: &mut Vec<u8>,
    start: &ContentBlockStart,
) -> Result<(), CodecError> {
    match &start.block {
        BlockStart::Text
        | BlockStart::Thinking {
            redacted: false, ..
        } => Ok(()),
        BlockStart::Thinking { redacted: true, .. } => Ok(()),
        BlockStart::ToolCall { id, name } => {
            let index = openai_tool_index(start.index)?;
            let mut function = Object::new();
            function.insert("name".to_owned(), Value::String(name.clone()));
            function.insert("arguments".to_owned(), Value::String(String::new()));
            let mut call = Object::new();
            call.insert("index".to_owned(), Value::from(index));
            if !id.is_empty() {
                call.insert("id".to_owned(), Value::String(id.clone()));
            }
            call.insert("type".to_owned(), Value::String("function".to_owned()));
            call.insert("function".to_owned(), Value::Object(function));
            start.extensions.merge_into(&mut call);
            let mut inner = Object::new();
            inner.insert(
                "tool_calls".to_owned(),
                Value::Array(vec![Value::Object(call)]),
            );
            write_chunk(
                out,
                encoder.id.as_deref(),
                encoder.model.as_deref(),
                &Extensions::new(),
                inner,
                None,
                None,
            )
        }
        BlockStart::Unknown { kind } => {
            let mut object = Object::new();
            object.insert("type".to_owned(), Value::String(kind.clone()));
            object.insert("index".to_owned(), Value::from(start.index));
            start.extensions.merge_into(&mut object);
            write_data(out, &Value::Object(object))
        }
    }
}

fn encode_unknown(event: &UnknownEvent) -> Value {
    match &event.payload {
        Value::Object(object) => {
            let mut object = object.clone();
            object
                .entry("type".to_owned())
                .or_insert_with(|| Value::String(event.name.clone()));
            Value::Object(object)
        }
        other => {
            let mut object = Object::new();
            object.insert("type".to_owned(), Value::String(event.name.clone()));
            object.insert("payload".to_owned(), other.clone());
            Value::Object(object)
        }
    }
}

fn expect_stream_index(actual: u32, expected: u32, what: &str) -> Result<(), CodecError> {
    if actual == expected {
        return Ok(());
    }
    Err(CodecError::Unsupported {
        message: format!("Chat Completions {what} block index {actual} is not {expected}"),
    })
}

fn openai_tool_index(ir_index: u32) -> Result<u32, CodecError> {
    ir_index
        .checked_sub(TOOL_INDEX_BASE)
        .ok_or_else(|| CodecError::Unsupported {
            message: format!(
                "Chat Completions tool block index {ir_index} is below {TOOL_INDEX_BASE}"
            ),
        })
}

fn write_chunk(
    out: &mut Vec<u8>,
    id: Option<&str>,
    model: Option<&str>,
    extensions: &Extensions,
    delta: Object,
    finish: Option<&str>,
    usage: Option<&Usage>,
) -> Result<(), CodecError> {
    let mut object = Object::new();
    object.insert(
        "object".to_owned(),
        Value::String("chat.completion.chunk".to_owned()),
    );
    if let Some(id) = id {
        object.insert("id".to_owned(), Value::String(id.to_owned()));
    }
    if let Some(model) = model {
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    }
    let mut choice = Object::new();
    choice.insert("index".to_owned(), Value::from(0));
    choice.insert("delta".to_owned(), Value::Object(delta));
    if let Some(finish) = finish {
        choice.insert("finish_reason".to_owned(), Value::String(finish.to_owned()));
    }
    object.insert(
        "choices".to_owned(),
        Value::Array(vec![Value::Object(choice)]),
    );
    if let Some(usage) = usage {
        object.insert("usage".to_owned(), encode_usage(usage));
    }
    extensions.merge_into(&mut object);
    write_data(out, &Value::Object(object))
}

fn write_usage_chunk(
    out: &mut Vec<u8>,
    id: Option<&str>,
    model: Option<&str>,
    usage: &Usage,
) -> Result<(), CodecError> {
    let mut object = Object::new();
    object.insert(
        "object".to_owned(),
        Value::String("chat.completion.chunk".to_owned()),
    );
    if let Some(id) = id {
        object.insert("id".to_owned(), Value::String(id.to_owned()));
    }
    if let Some(model) = model {
        object.insert("model".to_owned(), Value::String(model.to_owned()));
    }
    object.insert("choices".to_owned(), Value::Array(Vec::new()));
    object.insert("usage".to_owned(), encode_usage(usage));
    write_data(out, &Value::Object(object))
}

fn write_data(out: &mut Vec<u8>, value: &Value) -> Result<(), CodecError> {
    let data = serde_json::to_string(value)?;
    sse::write_event(out, None, &data);
    Ok(())
}

fn encode_error(status: u16, message: &str) -> Vec<u8> {
    openai_error_body(status, message)
}

pub(crate) fn openai_error_body(status: u16, message: &str) -> Vec<u8> {
    let kind = match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        500..=599 => "server_error",
        _ => "api_error",
    };
    serde_json::to_vec(&serde_json::json!({
        "error": { "message": message, "type": kind, "param": null, "code": null }
    }))
    .unwrap_or_else(|_| {
        b"{\"error\":{\"message\":\"request failed\",\"type\":\"api_error\"}}".to_vec()
    })
}
