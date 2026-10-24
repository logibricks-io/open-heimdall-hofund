//! Anthropic Messages codec.
//!
//! System and developer messages are hoisted into the top-level system channel.
//! Tool results stay on user messages. See `docs/codecs.md` for the field mapping
//! and the conversions that drop data.

use std::collections::BTreeSet;

use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Extensions, GenerationParams,
    ImagePart, ImageSource, Message, MessageDelta, MessageStart, MessageStop, Ping, Request,
    Response, Role, SignatureDelta, Stop, StopReason, StreamEvent, TextDelta, TextPart,
    ThinkingDelta, ThinkingPart, Tool, ToolArguments, ToolArgumentsDelta, ToolCallPart, ToolChoice,
    ToolChoiceMode, ToolResultPart, UnknownEvent, UnknownPart, Usage,
};
use serde_json::Value;

use crate::error::CodecError;
use crate::jsonutil::{
    Object, combine_extensions, expect_array, expect_object, insert_f64, insert_u64, parse_object,
    require_index, require_string, take_bool, take_f64, take_object, take_string, take_string_list,
    take_u64, to_bytes,
};
use crate::sse::{self, SseFrame, SseParser};
use crate::{Codec, StreamDecoder};

/// Codec for the Anthropic Messages protocol.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicMessages;

impl Codec for AnthropicMessages {
    fn id(&self) -> &'static str {
        "anthropic-messages"
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
}

#[derive(Debug, Default)]
struct Decoder {
    parser: SseParser,
}

impl StreamDecoder for Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.push(chunk)?;
        frames_to_events(frames)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.finish()?;
        frames_to_events(frames)
    }
}

fn frames_to_events(frames: Vec<SseFrame>) -> Result<Vec<StreamEvent>, CodecError> {
    let mut events = Vec::new();
    for frame in frames {
        events.extend(decode_frame(frame)?);
    }
    Ok(events)
}

fn decode_request(bytes: &[u8]) -> Result<Request, CodecError> {
    let mut object = parse_object(bytes)?;
    let model = require_string(&mut object, "model")?;
    let mut messages = Vec::new();
    if let Some(system) = object.remove("system")
        && !system.is_null()
    {
        messages.push(Message {
            role: Role::System,
            content: decode_body(system, "system")?,
            extensions: Extensions::new(),
        });
    }
    let raw_messages = match object.remove("messages") {
        Some(value) => expect_array(value, "messages")?,
        None => return Err(CodecError::MissingField("messages")),
    };
    for message in raw_messages {
        messages.push(decode_message(message)?);
    }
    let tools = decode_tools(object.remove("tools"))?;
    let tool_choice = match object.remove("tool_choice") {
        Some(value) if !value.is_null() => Some(decode_tool_choice(value)?),
        _ => None,
    };
    let params = GenerationParams {
        max_tokens: take_u64(&mut object, "max_tokens")?,
        temperature: take_f64(&mut object, "temperature")?,
        top_p: take_f64(&mut object, "top_p")?,
        top_k: take_u64(&mut object, "top_k")?,
        stop: take_string_list(&mut object, "stop_sequences")?,
        ..GenerationParams::default()
    };
    let stream = take_bool(&mut object, "stream")?.unwrap_or(false);
    Ok(Request {
        model,
        messages,
        tools,
        tool_choice,
        params,
        stream,
        extensions: Extensions::from_map(object),
    })
}

fn encode_request(request: &Request) -> Result<Vec<u8>, CodecError> {
    let max_tokens = request
        .params
        .max_tokens
        .ok_or(CodecError::MissingField("max_tokens"))?;
    let mut object = Object::new();
    object.insert("model".to_owned(), Value::String(request.model.clone()));
    object.insert("max_tokens".to_owned(), Value::from(max_tokens));
    if let Some(system) = encode_system(&request.messages)? {
        object.insert("system".to_owned(), system);
    }
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
    insert_f64(&mut object, "temperature", request.params.temperature)?;
    insert_f64(&mut object, "top_p", request.params.top_p)?;
    insert_u64(&mut object, "top_k", request.params.top_k);
    if !request.params.stop.is_empty() {
        object.insert(
            "stop_sequences".to_owned(),
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
    if request.stream {
        object.insert("stream".to_owned(), Value::Bool(true));
    }
    request.extensions.merge_into(&mut object);
    request.params.extensions.merge_into(&mut object);
    to_bytes(&Value::Object(object))
}

fn decode_response(bytes: &[u8]) -> Result<Response, CodecError> {
    let mut object = parse_object(bytes)?;
    let _ = object.remove("type");
    let _ = object.remove("role");
    let id = take_string(&mut object, "id")?;
    let model = take_string(&mut object, "model")?;
    let content = match object.remove("content") {
        Some(value) => decode_body(value, "content")?,
        None => Vec::new(),
    };
    let stop_reason = take_string(&mut object, "stop_reason")?;
    let stop_sequence = take_string(&mut object, "stop_sequence")?;
    let stop = stop_reason.map(|reason| Stop {
        reason: decode_stop_reason(&reason),
        sequence: nonempty(stop_sequence),
    });
    let usage = match take_object(&mut object, "usage")? {
        Some(usage) => Some(decode_usage(usage)?),
        None => None,
    };
    Ok(Response {
        id,
        model,
        message: Message {
            role: Role::Assistant,
            content,
            extensions: Extensions::new(),
        },
        stop,
        usage,
        extensions: Extensions::from_map(object),
    })
}

fn encode_response(response: &Response) -> Result<Vec<u8>, CodecError> {
    if response.message.role != Role::Assistant {
        return Err(CodecError::Unsupported {
            message: "Messages responses are assistant turns".to_owned(),
        });
    }
    let mut object = Object::new();
    if let Some(id) = &response.id {
        object.insert("id".to_owned(), Value::String(id.clone()));
    }
    object.insert("type".to_owned(), Value::String("message".to_owned()));
    object.insert("role".to_owned(), Value::String("assistant".to_owned()));
    if let Some(model) = &response.model {
        object.insert("model".to_owned(), Value::String(model.clone()));
    }
    object.insert(
        "content".to_owned(),
        Value::Array(encode_parts(&response.message.content)?),
    );
    if let Some(stop) = &response.stop {
        object.insert(
            "stop_reason".to_owned(),
            Value::String(encode_stop_reason(&stop.reason).to_owned()),
        );
        object.insert(
            "stop_sequence".to_owned(),
            stop.sequence.clone().map_or(Value::Null, Value::String),
        );
    }
    if let Some(usage) = &response.usage {
        object.insert("usage".to_owned(), encode_usage(usage));
    }
    response.extensions.merge_into(&mut object);
    to_bytes(&Value::Object(object))
}

fn decode_message(value: Value) -> Result<Message, CodecError> {
    let mut object = expect_object(value, "messages")?;
    let role = match require_string(&mut object, "role")?.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        other => return Err(CodecError::UnknownRole(other.to_owned())),
    };
    let content = match object.remove("content") {
        Some(value) => decode_body(value, "content")?,
        None => Vec::new(),
    };
    Ok(Message {
        role,
        content,
        extensions: Extensions::from_map(object),
    })
}

fn encode_system(messages: &[Message]) -> Result<Option<Value>, CodecError> {
    let system: Vec<&Message> = messages
        .iter()
        .filter(|message| matches!(message.role, Role::System | Role::Developer))
        .collect();
    if system.is_empty() {
        return Ok(None);
    }
    if system.len() == 1
        && system[0].extensions.is_empty()
        && system[0].content.len() == 1
        && matches!(&system[0].content[0], ContentPart::Text(part) if part.extensions.is_empty())
        && let ContentPart::Text(part) = &system[0].content[0]
    {
        return Ok(Some(Value::String(part.text.clone())));
    }
    if system.len() == 1 && system[0].content.is_empty() && system[0].extensions.is_empty() {
        return Ok(Some(Value::Array(Vec::new())));
    }
    let mut blocks = Vec::new();
    for message in system {
        if message.content.is_empty() {
            let mut encoded = encode_part(&ContentPart::text(""))?;
            if let Value::Object(object) = &mut encoded {
                message.extensions.merge_into(object);
            }
            blocks.push(encoded);
            continue;
        }
        for (index, part) in message.content.iter().enumerate() {
            let mut encoded = encode_part(part)?;
            if index == 0
                && let Value::Object(object) = &mut encoded
            {
                message.extensions.merge_into(object);
            }
            blocks.push(encoded);
        }
    }
    Ok(Some(Value::Array(blocks)))
}

fn encode_messages(messages: &[Message]) -> Result<Vec<Value>, CodecError> {
    let mut encoded = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match messages[index].role {
            Role::System | Role::Developer => index += 1,
            Role::Tool => {
                let start = index;
                while index < messages.len() && messages[index].role == Role::Tool {
                    index += 1;
                }
                encoded.push(encode_tool_group(&messages[start..index])?);
            }
            Role::User | Role::Assistant => {
                encoded.push(encode_chat_message(&messages[index])?);
                index += 1;
            }
        }
    }
    Ok(encoded)
}

fn encode_tool_group(messages: &[Message]) -> Result<Value, CodecError> {
    let mut content = Vec::new();
    let mut extensions = Extensions::new();
    for message in messages {
        extensions.absorb(&message.extensions);
        content.extend(encode_parts(&message.content)?);
    }
    let mut object = Object::new();
    object.insert("role".to_owned(), Value::String("user".to_owned()));
    object.insert("content".to_owned(), Value::Array(content));
    extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn encode_chat_message(message: &Message) -> Result<Value, CodecError> {
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::User => "user",
        Role::System | Role::Developer | Role::Tool => "user",
    };
    let mut object = Object::new();
    object.insert("role".to_owned(), Value::String(role.to_owned()));
    object.insert(
        "content".to_owned(),
        Value::Array(encode_parts(&message.content)?),
    );
    message.extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn decode_body(value: Value, field: &'static str) -> Result<Vec<ContentPart>, CodecError> {
    match value {
        Value::String(text) => Ok(vec![ContentPart::text(text)]),
        Value::Array(items) => items.into_iter().map(decode_part).collect(),
        Value::Null => Ok(Vec::new()),
        _ => Err(CodecError::WrongType { field }),
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
        "image" => decode_image(object),
        "tool_use" => decode_tool_use(object),
        "tool_result" => decode_tool_result(object),
        "thinking" => {
            let text = take_string(&mut object, "thinking")?.unwrap_or_default();
            let signature = nonempty(take_string(&mut object, "signature")?);
            Ok(ContentPart::Thinking(ThinkingPart {
                text,
                signature,
                redacted: false,
                data: None,
                extensions: Extensions::from_map(object),
            }))
        }
        "redacted_thinking" => Ok(ContentPart::Thinking(ThinkingPart {
            text: String::new(),
            signature: None,
            redacted: true,
            data: take_string(&mut object, "data")?,
            extensions: Extensions::from_map(object),
        })),
        other => Ok(ContentPart::Unknown(UnknownPart {
            kind: other.to_owned(),
            extensions: Extensions::from_map(object),
        })),
    }
}

fn decode_image(mut object: Object) -> Result<ContentPart, CodecError> {
    let mut source_object =
        take_object(&mut object, "source")?.ok_or(CodecError::MissingField("source"))?;
    let kind = require_string(&mut source_object, "type")?;
    let source = match kind.as_str() {
        "base64" => ImageSource::Base64 {
            media_type: require_string(&mut source_object, "media_type")?,
            data: require_string(&mut source_object, "data")?,
        },
        "url" => ImageSource::Url {
            url: require_string(&mut source_object, "url")?,
        },
        other => {
            return Err(CodecError::Unsupported {
                message: format!("image source `{other}` is not supported"),
            });
        }
    };
    Ok(ContentPart::Image(ImagePart {
        source,
        detail: None,
        extensions: combine_extensions(source_object, object),
    }))
}

fn decode_tool_use(mut object: Object) -> Result<ContentPart, CodecError> {
    let id = require_string(&mut object, "id")?;
    let name = require_string(&mut object, "name")?;
    let input = object
        .remove("input")
        .unwrap_or(Value::Object(Object::new()));
    Ok(ContentPart::ToolCall(ToolCallPart {
        id,
        name,
        arguments: ToolArguments::Json { value: input },
        extensions: Extensions::from_map(object),
    }))
}

fn decode_tool_result(mut object: Object) -> Result<ContentPart, CodecError> {
    let id = require_string(&mut object, "tool_use_id")?;
    let is_error = take_bool(&mut object, "is_error")?.unwrap_or(false);
    let content = match object.remove("content") {
        Some(value) => decode_body(value, "content")?,
        None => Vec::new(),
    };
    Ok(ContentPart::ToolResult(ToolResultPart {
        id,
        is_error,
        content,
        extensions: Extensions::from_map(object),
    }))
}

fn encode_parts(parts: &[ContentPart]) -> Result<Vec<Value>, CodecError> {
    parts.iter().map(encode_part).collect()
}

fn encode_part(part: &ContentPart) -> Result<Value, CodecError> {
    let mut object = Object::new();
    match part {
        ContentPart::Text(part) => {
            object.insert("type".to_owned(), Value::String("text".to_owned()));
            object.insert("text".to_owned(), Value::String(part.text.clone()));
            part.extensions.merge_into(&mut object);
        }
        ContentPart::Image(part) => {
            object.insert("type".to_owned(), Value::String("image".to_owned()));
            object.insert("source".to_owned(), encode_image_source(&part.source));
            part.extensions.merge_into(&mut object);
        }
        ContentPart::ToolCall(part) => {
            object.insert("type".to_owned(), Value::String("tool_use".to_owned()));
            object.insert("id".to_owned(), Value::String(part.id.clone()));
            object.insert("name".to_owned(), Value::String(part.name.clone()));
            object.insert("input".to_owned(), encode_tool_input(&part.arguments)?);
            part.extensions.merge_into(&mut object);
        }
        ContentPart::ToolResult(part) => {
            object.insert("type".to_owned(), Value::String("tool_result".to_owned()));
            object.insert("tool_use_id".to_owned(), Value::String(part.id.clone()));
            if let Some(content) = encode_tool_result_content(&part.content)? {
                object.insert("content".to_owned(), content);
            }
            if part.is_error {
                object.insert("is_error".to_owned(), Value::Bool(true));
            }
            part.extensions.merge_into(&mut object);
        }
        ContentPart::Thinking(part) => encode_thinking(&mut object, part),
        ContentPart::Unknown(part) => {
            object.insert("type".to_owned(), Value::String(part.kind.clone()));
            part.extensions.merge_into(&mut object);
        }
    }
    Ok(Value::Object(object))
}

fn encode_thinking(object: &mut Object, part: &ThinkingPart) {
    if part.redacted {
        object.insert(
            "type".to_owned(),
            Value::String("redacted_thinking".to_owned()),
        );
        if let Some(data) = &part.data {
            object.insert("data".to_owned(), Value::String(data.clone()));
        }
    } else {
        object.insert("type".to_owned(), Value::String("thinking".to_owned()));
        object.insert("thinking".to_owned(), Value::String(part.text.clone()));
        if let Some(signature) = part.signature.as_ref().filter(|value| !value.is_empty()) {
            object.insert("signature".to_owned(), Value::String(signature.clone()));
        }
    }
    part.extensions.merge_into(object);
}

fn encode_image_source(source: &ImageSource) -> Value {
    let mut object = Object::new();
    match source {
        ImageSource::Base64 { media_type, data } => {
            object.insert("type".to_owned(), Value::String("base64".to_owned()));
            object.insert("media_type".to_owned(), Value::String(media_type.clone()));
            object.insert("data".to_owned(), Value::String(data.clone()));
        }
        ImageSource::Url { url } => {
            object.insert("type".to_owned(), Value::String("url".to_owned()));
            object.insert("url".to_owned(), Value::String(url.clone()));
        }
    }
    Value::Object(object)
}

fn encode_tool_input(arguments: &ToolArguments) -> Result<Value, CodecError> {
    match arguments {
        ToolArguments::Json { value } => Ok(value.clone()),
        ToolArguments::Raw { text } if text.is_empty() => Ok(Value::Object(Object::new())),
        ToolArguments::Raw { text } => {
            serde_json::from_str(text).map_err(|_| CodecError::Unsupported {
                message: "tool arguments are not JSON".to_owned(),
            })
        }
    }
}

fn encode_tool_result_content(parts: &[ContentPart]) -> Result<Option<Value>, CodecError> {
    if parts.is_empty() {
        return Ok(None);
    }
    if let [ContentPart::Text(part)] = parts
        && part.extensions.is_empty()
    {
        return Ok(Some(Value::String(part.text.clone())));
    }
    Ok(Some(Value::Array(encode_parts(parts)?)))
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
    let name = require_string(&mut object, "name")?;
    let description = take_string(&mut object, "description")?;
    let input_schema = object
        .remove("input_schema")
        .ok_or(CodecError::MissingField("input_schema"))?;
    Ok(Tool {
        name,
        description,
        input_schema,
        extensions: Extensions::from_map(object),
    })
}

fn encode_tools(tools: &[Tool]) -> Result<Vec<Value>, CodecError> {
    tools.iter().map(encode_tool).collect()
}

fn encode_tool(tool: &Tool) -> Result<Value, CodecError> {
    let mut object = Object::new();
    object.insert("name".to_owned(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        object.insert("description".to_owned(), Value::String(description.clone()));
    }
    object.insert("input_schema".to_owned(), tool.input_schema.clone());
    tool.extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn decode_tool_choice(value: Value) -> Result<ToolChoice, CodecError> {
    match value {
        Value::String(text) => tool_choice_from_type(&text, Object::new()),
        Value::Object(mut object) => {
            let kind = require_string(&mut object, "type")?;
            tool_choice_from_type(&kind, object)
        }
        _ => Err(CodecError::WrongType {
            field: "tool_choice",
        }),
    }
}

fn tool_choice_from_type(kind: &str, mut object: Object) -> Result<ToolChoice, CodecError> {
    let mode = match kind {
        "auto" => ToolChoiceMode::Auto,
        "any" => ToolChoiceMode::Required,
        "none" => ToolChoiceMode::None,
        "tool" => ToolChoiceMode::Named {
            name: require_string(&mut object, "name")?,
        },
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
    let mut object = Object::new();
    match &choice.mode {
        ToolChoiceMode::Auto => {
            object.insert("type".to_owned(), Value::String("auto".to_owned()));
        }
        ToolChoiceMode::None => {
            object.insert("type".to_owned(), Value::String("none".to_owned()));
        }
        ToolChoiceMode::Required => {
            object.insert("type".to_owned(), Value::String("any".to_owned()));
        }
        ToolChoiceMode::Named { name } => {
            object.insert("type".to_owned(), Value::String("tool".to_owned()));
            object.insert("name".to_owned(), Value::String(name.clone()));
        }
    }
    choice.extensions.merge_into(&mut object);
    Value::Object(object)
}

fn decode_usage(mut object: Object) -> Result<Usage, CodecError> {
    Ok(Usage {
        input_tokens: take_u64(&mut object, "input_tokens")?,
        output_tokens: take_u64(&mut object, "output_tokens")?,
        total_tokens: None,
        cache_read_tokens: take_u64(&mut object, "cache_read_input_tokens")?,
        cache_write_tokens: take_u64(&mut object, "cache_creation_input_tokens")?,
        reasoning_tokens: None,
        extensions: Extensions::from_map(object),
    })
}

fn encode_usage(usage: &Usage) -> Value {
    let mut object = Object::new();
    insert_u64(&mut object, "input_tokens", usage.input_tokens);
    insert_u64(&mut object, "output_tokens", usage.output_tokens);
    insert_u64(
        &mut object,
        "cache_creation_input_tokens",
        usage.cache_write_tokens,
    );
    insert_u64(
        &mut object,
        "cache_read_input_tokens",
        usage.cache_read_tokens,
    );
    usage.extensions.merge_into(&mut object);
    Value::Object(object)
}

fn decode_stop_reason(token: &str) -> StopReason {
    match token {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

fn encode_stop_reason(reason: &StopReason) -> &str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::ContentFilter => "content_filter",
        StopReason::Refusal => "refusal",
        StopReason::Other(token) => token.as_str(),
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

fn decode_frame(frame: SseFrame) -> Result<Vec<StreamEvent>, CodecError> {
    if frame.data.trim().is_empty() {
        if frame.event.as_deref() == Some("ping") {
            return Ok(vec![StreamEvent::Ping(Ping::default())]);
        }
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_str(&frame.data)?;
    let Value::Object(mut object) = value else {
        let name = frame.event.unwrap_or_else(|| "message".to_owned());
        return Ok(vec![StreamEvent::Unknown(UnknownEvent {
            name,
            payload: value,
        })]);
    };
    let json_type = take_string(&mut object, "type")?;
    let name = frame
        .event
        .or(json_type)
        .unwrap_or_else(|| "message".to_owned());
    match name.as_str() {
        "message_start" => Ok(vec![decode_message_start(object)?]),
        "content_block_start" => decode_block_start(object),
        "content_block_delta" => Ok(decode_block_delta(object)?.into_iter().collect()),
        "content_block_stop" => Ok(vec![decode_block_stop(object)?]),
        "message_delta" => Ok(vec![decode_message_delta(object)?]),
        "message_stop" => Ok(vec![StreamEvent::MessageStop(MessageStop {
            extensions: Extensions::from_map(object),
        })]),
        "ping" => Ok(vec![StreamEvent::Ping(Ping {
            extensions: Extensions::from_map(object),
        })]),
        _ => Ok(vec![StreamEvent::Unknown(UnknownEvent {
            name,
            payload: Value::Object(object),
        })]),
    }
}

fn decode_message_start(mut event: Object) -> Result<StreamEvent, CodecError> {
    let mut message =
        take_object(&mut event, "message")?.ok_or(CodecError::MissingField("message"))?;
    let _ = message.remove("type");
    let _ = message.remove("role");
    let _ = message.remove("content");
    let _ = message.remove("stop_reason");
    let _ = message.remove("stop_sequence");
    let id = take_string(&mut message, "id")?;
    let model = take_string(&mut message, "model")?;
    let usage = match take_object(&mut message, "usage")? {
        Some(usage) => Some(decode_usage(usage)?),
        None => None,
    };
    Ok(StreamEvent::MessageStart(MessageStart {
        id,
        model,
        usage,
        extensions: combine_extensions(message, event),
    }))
}

fn decode_block_start(mut event: Object) -> Result<Vec<StreamEvent>, CodecError> {
    let index = require_index(&mut event, "index")?;
    let mut block = take_object(&mut event, "content_block")?
        .ok_or(CodecError::MissingField("content_block"))?;
    let kind = require_string(&mut block, "type")?;
    let mut deltas = Vec::new();
    let started = match kind.as_str() {
        "text" => {
            let text = take_string(&mut block, "text")?.unwrap_or_default();
            if !text.is_empty() {
                deltas.push(StreamEvent::TextDelta(TextDelta {
                    index,
                    text,
                    extensions: Extensions::new(),
                }));
            }
            BlockStart::Text
        }
        "thinking" => {
            let text = take_string(&mut block, "thinking")?.unwrap_or_default();
            let signature = nonempty(take_string(&mut block, "signature")?);
            if !text.is_empty() {
                deltas.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                    index,
                    text,
                    extensions: Extensions::new(),
                }));
            }
            if let Some(signature) = signature {
                deltas.push(StreamEvent::SignatureDelta(SignatureDelta {
                    index,
                    signature,
                    extensions: Extensions::new(),
                }));
            }
            BlockStart::Thinking {
                redacted: false,
                data: None,
            }
        }
        "redacted_thinking" => BlockStart::Thinking {
            redacted: true,
            data: take_string(&mut block, "data")?,
        },
        "tool_use" => {
            let id = require_string(&mut block, "id")?;
            let name = require_string(&mut block, "name")?;
            if let Some(input) = block.remove("input")
                && !json_is_empty(&input)
            {
                deltas.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                    index,
                    partial_json: serde_json::to_string(&input)?,
                    extensions: Extensions::new(),
                }));
            }
            BlockStart::ToolCall { id, name }
        }
        other => BlockStart::Unknown {
            kind: other.to_owned(),
        },
    };
    let mut events = vec![StreamEvent::ContentBlockStart(ContentBlockStart {
        index,
        block: started,
        extensions: combine_extensions(block, event),
    })];
    events.extend(deltas);
    Ok(events)
}

fn json_is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(object) if object.is_empty() => true,
        _ => false,
    }
}

fn decode_block_delta(mut event: Object) -> Result<Option<StreamEvent>, CodecError> {
    let index = require_index(&mut event, "index")?;
    let mut delta = take_object(&mut event, "delta")?.ok_or(CodecError::MissingField("delta"))?;
    let kind = require_string(&mut delta, "type")?;
    match kind.as_str() {
        "text_delta" => {
            let text = require_string(&mut delta, "text")?;
            Ok(Some(StreamEvent::TextDelta(TextDelta {
                index,
                text,
                extensions: combine_extensions(delta, event),
            })))
        }
        "thinking_delta" => {
            let text = require_string(&mut delta, "thinking")?;
            Ok(Some(StreamEvent::ThinkingDelta(ThinkingDelta {
                index,
                text,
                extensions: combine_extensions(delta, event),
            })))
        }
        "signature_delta" => {
            let signature = require_string(&mut delta, "signature")?;
            if signature.is_empty() {
                return Ok(None);
            }
            Ok(Some(StreamEvent::SignatureDelta(SignatureDelta {
                index,
                signature,
                extensions: combine_extensions(delta, event),
            })))
        }
        "input_json_delta" => {
            let partial_json = require_string(&mut delta, "partial_json")?;
            Ok(Some(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index,
                partial_json,
                extensions: combine_extensions(delta, event),
            })))
        }
        other => {
            delta.insert("type".to_owned(), Value::String(other.to_owned()));
            event.insert("index".to_owned(), Value::from(index));
            event.insert("delta".to_owned(), Value::Object(delta));
            Ok(Some(StreamEvent::Unknown(UnknownEvent {
                name: "content_block_delta".to_owned(),
                payload: Value::Object(event),
            })))
        }
    }
}

fn decode_block_stop(mut event: Object) -> Result<StreamEvent, CodecError> {
    let index = require_index(&mut event, "index")?;
    Ok(StreamEvent::ContentBlockStop(ContentBlockStop {
        index,
        extensions: Extensions::from_map(event),
    }))
}

fn decode_message_delta(mut event: Object) -> Result<StreamEvent, CodecError> {
    let mut delta = take_object(&mut event, "delta")?.unwrap_or_default();
    let stop_reason = take_string(&mut delta, "stop_reason")?;
    let stop_sequence = nonempty(take_string(&mut delta, "stop_sequence")?);
    let usage = match take_object(&mut event, "usage")? {
        Some(usage) => Some(decode_usage(usage)?),
        None => None,
    };
    let stop = stop_reason.map(|reason| Stop {
        reason: decode_stop_reason(&reason),
        sequence: stop_sequence,
    });
    Ok(StreamEvent::MessageDelta(MessageDelta {
        stop,
        usage,
        extensions: combine_extensions(delta, event),
    }))
}

fn encode_stream(events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    let mut started = BTreeSet::new();
    for event in events {
        encode_stream_event(&mut out, &mut started, event)?;
    }
    Ok(out)
}

fn encode_stream_event(
    out: &mut Vec<u8>,
    started: &mut BTreeSet<u32>,
    event: &StreamEvent,
) -> Result<(), CodecError> {
    match event {
        StreamEvent::MessageStart(start) => {
            write_json(out, "message_start", &encode_message_start(start))
        }
        StreamEvent::ContentBlockStart(start) => {
            started.insert(start.index);
            write_json(out, "content_block_start", &encode_block_start(start))
        }
        StreamEvent::TextDelta(delta) => {
            ensure_start(out, started, delta.index, &BlockStart::Text)?;
            write_json(out, "content_block_delta", &text_delta_json(delta))
        }
        StreamEvent::ThinkingDelta(delta) => {
            ensure_start(
                out,
                started,
                delta.index,
                &BlockStart::Thinking {
                    redacted: false,
                    data: None,
                },
            )?;
            write_json(out, "content_block_delta", &thinking_delta_json(delta))
        }
        StreamEvent::SignatureDelta(delta) => {
            if delta.signature.is_empty() {
                return Ok(());
            }
            ensure_start(
                out,
                started,
                delta.index,
                &BlockStart::Thinking {
                    redacted: false,
                    data: None,
                },
            )?;
            write_json(out, "content_block_delta", &signature_delta_json(delta))
        }
        StreamEvent::ToolArgumentsDelta(delta) => {
            ensure_start(
                out,
                started,
                delta.index,
                &BlockStart::ToolCall {
                    id: String::new(),
                    name: String::new(),
                },
            )?;
            write_json(out, "content_block_delta", &arguments_delta_json(delta))
        }
        StreamEvent::ContentBlockStop(stop) => {
            write_json(out, "content_block_stop", &block_stop_json(stop))
        }
        StreamEvent::MessageDelta(delta) => {
            write_json(out, "message_delta", &message_delta_json(delta))
        }
        StreamEvent::MessageStop(stop) => write_json(out, "message_stop", &message_stop_json(stop)),
        StreamEvent::Ping(ping) => write_json(out, "ping", &ping_json(ping)),
        StreamEvent::Unknown(event) => {
            let data = serde_json::to_string(&event.payload)?;
            sse::write_event(out, Some(&event.name), &data);
            Ok(())
        }
    }
}

fn ensure_start(
    out: &mut Vec<u8>,
    started: &mut BTreeSet<u32>,
    index: u32,
    block: &BlockStart,
) -> Result<(), CodecError> {
    if started.insert(index) {
        let start = ContentBlockStart {
            index,
            block: block.clone(),
            extensions: Extensions::new(),
        };
        write_json(out, "content_block_start", &encode_block_start(&start))?;
    }
    Ok(())
}

fn encode_message_start(start: &MessageStart) -> Value {
    let mut message = Object::new();
    if let Some(id) = &start.id {
        message.insert("id".to_owned(), Value::String(id.clone()));
    }
    message.insert("type".to_owned(), Value::String("message".to_owned()));
    message.insert("role".to_owned(), Value::String("assistant".to_owned()));
    message.insert("content".to_owned(), Value::Array(Vec::new()));
    if let Some(model) = &start.model {
        message.insert("model".to_owned(), Value::String(model.clone()));
    }
    message.insert("stop_reason".to_owned(), Value::Null);
    message.insert("stop_sequence".to_owned(), Value::Null);
    if let Some(usage) = &start.usage {
        message.insert("usage".to_owned(), encode_usage(usage));
    }
    let mut event = Object::new();
    event.insert("type".to_owned(), Value::String("message_start".to_owned()));
    event.insert("message".to_owned(), Value::Object(message));
    start.extensions.merge_into(&mut event);
    Value::Object(event)
}

fn encode_block_start(start: &ContentBlockStart) -> Value {
    let block = match &start.block {
        BlockStart::Text => object_with_type("text", "text", ""),
        BlockStart::Thinking {
            redacted: false, ..
        } => object_with_type("thinking", "thinking", ""),
        BlockStart::Thinking {
            redacted: true,
            data,
        } => {
            let mut block = Object::new();
            block.insert(
                "type".to_owned(),
                Value::String("redacted_thinking".to_owned()),
            );
            if let Some(data) = data {
                block.insert("data".to_owned(), Value::String(data.clone()));
            }
            block
        }
        BlockStart::ToolCall { id, name } => {
            let mut block = Object::new();
            block.insert("type".to_owned(), Value::String("tool_use".to_owned()));
            block.insert("id".to_owned(), Value::String(id.clone()));
            block.insert("name".to_owned(), Value::String(name.clone()));
            block.insert("input".to_owned(), Value::Object(Object::new()));
            block
        }
        BlockStart::Unknown { kind } => {
            let mut block = Object::new();
            block.insert("type".to_owned(), Value::String(kind.clone()));
            block
        }
    };
    let mut event = Object::new();
    event.insert(
        "type".to_owned(),
        Value::String("content_block_start".to_owned()),
    );
    event.insert("index".to_owned(), Value::from(start.index));
    event.insert("content_block".to_owned(), Value::Object(block));
    start.extensions.merge_into(&mut event);
    Value::Object(event)
}

fn object_with_type(kind: &str, field: &str, value: &str) -> Object {
    let mut block = Object::new();
    block.insert("type".to_owned(), Value::String(kind.to_owned()));
    block.insert(field.to_owned(), Value::String(value.to_owned()));
    block
}

fn text_delta_json(delta: &TextDelta) -> Value {
    delta_event(
        delta.index,
        "text_delta",
        "text",
        &delta.text,
        &delta.extensions,
    )
}

fn thinking_delta_json(delta: &ThinkingDelta) -> Value {
    delta_event(
        delta.index,
        "thinking_delta",
        "thinking",
        &delta.text,
        &delta.extensions,
    )
}

fn signature_delta_json(delta: &SignatureDelta) -> Value {
    delta_event(
        delta.index,
        "signature_delta",
        "signature",
        &delta.signature,
        &delta.extensions,
    )
}

fn arguments_delta_json(delta: &ToolArgumentsDelta) -> Value {
    delta_event(
        delta.index,
        "input_json_delta",
        "partial_json",
        &delta.partial_json,
        &delta.extensions,
    )
}

fn delta_event(index: u32, kind: &str, field: &str, value: &str, extensions: &Extensions) -> Value {
    let mut inner = Object::new();
    inner.insert("type".to_owned(), Value::String(kind.to_owned()));
    inner.insert(field.to_owned(), Value::String(value.to_owned()));
    let mut event = Object::new();
    event.insert(
        "type".to_owned(),
        Value::String("content_block_delta".to_owned()),
    );
    event.insert("index".to_owned(), Value::from(index));
    event.insert("delta".to_owned(), Value::Object(inner));
    extensions.merge_into(&mut event);
    Value::Object(event)
}

fn block_stop_json(stop: &ContentBlockStop) -> Value {
    let mut event = Object::new();
    event.insert(
        "type".to_owned(),
        Value::String("content_block_stop".to_owned()),
    );
    event.insert("index".to_owned(), Value::from(stop.index));
    stop.extensions.merge_into(&mut event);
    Value::Object(event)
}

fn message_delta_json(delta: &MessageDelta) -> Value {
    let mut inner = Object::new();
    if let Some(stop) = &delta.stop {
        inner.insert(
            "stop_reason".to_owned(),
            Value::String(encode_stop_reason(&stop.reason).to_owned()),
        );
        inner.insert(
            "stop_sequence".to_owned(),
            stop.sequence.clone().map_or(Value::Null, Value::String),
        );
    }
    let mut event = Object::new();
    event.insert("type".to_owned(), Value::String("message_delta".to_owned()));
    event.insert("delta".to_owned(), Value::Object(inner));
    if let Some(usage) = &delta.usage {
        event.insert("usage".to_owned(), encode_usage(usage));
    }
    delta.extensions.merge_into(&mut event);
    Value::Object(event)
}

fn message_stop_json(stop: &MessageStop) -> Value {
    let mut event = Object::new();
    event.insert("type".to_owned(), Value::String("message_stop".to_owned()));
    stop.extensions.merge_into(&mut event);
    Value::Object(event)
}

fn ping_json(ping: &Ping) -> Value {
    let mut event = Object::new();
    event.insert("type".to_owned(), Value::String("ping".to_owned()));
    ping.extensions.merge_into(&mut event);
    Value::Object(event)
}

fn write_json(out: &mut Vec<u8>, event: &str, value: &Value) -> Result<(), CodecError> {
    let data = serde_json::to_string(value)?;
    sse::write_event(out, Some(event), &data);
    Ok(())
}
