//! OpenAI Responses API codec.
//!
//! A turn is a list of items (`message`, `function_call`, `function_call_output`,
//! `reasoning`). Consecutive assistant items are one IR message. See
//! `docs/codecs.md` for the conversions that drop data.

use std::collections::{BTreeMap, BTreeSet};

use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Extensions, GenerationParams,
    ImageDetail, ImagePart, ImageSource, Message, MessageDelta, MessageStart, MessageStop, Ping,
    Request, Response, Role, Stop, StopReason, StreamEvent, TextDelta, TextPart, ThinkingDelta,
    ThinkingPart, Tool, ToolArguments, ToolArgumentsDelta, ToolCallPart, ToolChoice,
    ToolChoiceMode, ToolResultPart, UnknownEvent, UnknownPart, Usage,
};
use serde_json::Value;

use crate::error::CodecError;
use crate::jsonutil::{
    Object, expect_array, expect_object, insert_f64, insert_u64, parse_object, require_string,
    take_bool, take_f64, take_object, take_string, take_u64, to_bytes,
};
use crate::openai_chat::openai_error_body;
use crate::sse::{self, SseFrame, SseParser};
use crate::{Codec, StreamDecoder, StreamEncoder as StreamEncoderTrait};

const SEALED_ITEM: &str = "sealed_item";

/// Codec for the OpenAI Responses API.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAiResponses;

impl OpenAiResponses {
    /// Prepares a request body for an upstream that speaks this protocol.
    ///
    /// When `store` is absent it is set to false, so the upstream does not
    /// persist the response unless the client asked. A function call with no
    /// later output gets an empty output item, which is what this protocol
    /// requires before it will accept the next turn.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when `bytes` is not a JSON object.
    pub fn prepare_upstream_body(bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
        let mut object = parse_object(bytes)?;
        if !object.contains_key("store") {
            object.insert("store".to_owned(), Value::Bool(false));
        }
        repair_orphaned_outputs(&mut object);
        to_bytes(&Value::Object(object))
    }
}

impl Codec for OpenAiResponses {
    fn id(&self) -> &'static str {
        "openai-responses"
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

    fn encode_error(&self, status: u16, message: &str) -> Vec<u8> {
        openai_error_body(status, message)
    }
}

fn decode_request(bytes: &[u8]) -> Result<Request, CodecError> {
    let mut object = parse_object(bytes)?;
    let model = require_string(&mut object, "model")?;
    let mut messages = Vec::new();
    if let Some(instructions) = object.remove("instructions")
        && !instructions.is_null()
    {
        messages.push(Message {
            role: Role::System,
            content: decode_body(instructions, "instructions")?,
            extensions: Extensions::new(),
        });
    }
    let input = object
        .remove("input")
        .ok_or(CodecError::MissingField("input"))?;
    if input.is_null() {
        return Err(CodecError::MissingField("input"));
    }
    messages.extend(decode_input(input)?);
    messages = merge_assistant_runs(messages);
    let tools = decode_tools(object.remove("tools"))?;
    let (tool_choice, tools) = decode_tool_choice(object.remove("tool_choice"), tools)?;
    let max_tokens = take_u64(&mut object, "max_output_tokens")?;
    let temperature = take_f64(&mut object, "temperature")?;
    let top_p = take_f64(&mut object, "top_p")?;
    let stream = take_bool(&mut object, "stream")?.unwrap_or(false);
    Ok(Request {
        model,
        messages,
        tools,
        tool_choice,
        params: GenerationParams {
            max_tokens,
            temperature,
            top_p,
            ..GenerationParams::default()
        },
        stream,
        extensions: Extensions::from_map(object),
    })
}

fn decode_input(input: Value) -> Result<Vec<Message>, CodecError> {
    if let Value::String(text) = input {
        return Ok(vec![Message {
            role: Role::User,
            content: vec![text_part(text)],
            extensions: Extensions::new(),
        }]);
    }
    let items = expect_array(input, "input")?;
    let mut messages = Vec::new();
    for item in items {
        if let Some(message) = decode_item(item)? {
            messages.push(message);
        }
    }
    Ok(messages)
}

fn decode_item(item: Value) -> Result<Option<Message>, CodecError> {
    let mut object = expect_object(item, "input")?;
    let kind = take_string(&mut object, "type")?.unwrap_or_default();
    match kind.as_str() {
        "" | "message" => {
            let role = match take_string(&mut object, "role")?.as_deref() {
                Some("assistant") => Role::Assistant,
                Some("system") => Role::System,
                Some("developer") => Role::Developer,
                Some("user") | None => Role::User,
                Some(other) => return Err(CodecError::UnknownRole(other.to_owned())),
            };
            let content = match object.remove("content") {
                Some(content) if !content.is_null() => decode_body(content, "content")?,
                _ => Vec::new(),
            };
            let _ = object.remove("status");
            Ok(Some(Message {
                role,
                content,
                extensions: Extensions::from_map(object),
            }))
        }
        "function_call" => Ok(Some(decode_function_call(object)?)),
        "function_call_output" => Ok(Some(decode_function_output(object)?)),
        "reasoning" => Ok(decode_reasoning(object)?),
        other => {
            let mut extensions = Extensions::from_map(object);
            extensions.insert("type", Value::String(other.to_owned()));
            Ok(Some(Message {
                role: Role::User,
                content: vec![ContentPart::Unknown(UnknownPart {
                    kind: other.to_owned(),
                    extensions,
                })],
                extensions: Extensions::new(),
            }))
        }
    }
}

fn decode_function_call(mut object: Object) -> Result<Message, CodecError> {
    let id = take_string(&mut object, "call_id")?.unwrap_or_default();
    let name = take_string(&mut object, "name")?.unwrap_or_default();
    let arguments = take_string(&mut object, "arguments")?.unwrap_or_default();
    let _ = object.remove("status");
    Ok(Message {
        role: Role::Assistant,
        content: vec![ContentPart::ToolCall(ToolCallPart {
            id,
            name,
            arguments: ToolArguments::from_text(&arguments),
            extensions: Extensions::from_map(object),
        })],
        extensions: Extensions::new(),
    })
}

fn decode_function_output(mut object: Object) -> Result<Message, CodecError> {
    let id = take_string(&mut object, "call_id")?.unwrap_or_default();
    let content = match object.remove("output") {
        Some(Value::String(text)) => vec![text_part(text)],
        Some(Value::Array(items)) => decode_parts(items)?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(CodecError::WrongType { field: "output" });
        }
    };
    Ok(Message {
        role: Role::Tool,
        content: vec![ContentPart::ToolResult(ToolResultPart {
            id,
            is_error: false,
            content,
            extensions: Extensions::from_map(object),
        })],
        extensions: Extensions::new(),
    })
}

/// Keeps a non-empty `encrypted_content` as the original reasoning item.
///
/// `type` is restored because the stream decoder removes it before this runs.
/// An empty or missing seal is not stored.
fn seal_extensions(item: &Object) -> Extensions {
    let Some(value) = item.get("encrypted_content").and_then(Value::as_str) else {
        return Extensions::new();
    };
    if value.is_empty() {
        return Extensions::new();
    }
    let mut object = item.clone();
    object.insert("type".to_owned(), Value::String("reasoning".to_owned()));
    let mut extensions = Extensions::new();
    extensions.insert(SEALED_ITEM, Value::Object(object));
    extensions
}

fn decode_reasoning(mut object: Object) -> Result<Option<Message>, CodecError> {
    let sealed = object
        .get("encrypted_content")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let text = reasoning_text(&object);
    if text.is_empty() && !sealed {
        return Ok(None);
    }
    let mut extensions = Extensions::new();
    if sealed {
        object.insert("type".to_owned(), Value::String("reasoning".to_owned()));
        extensions.insert(SEALED_ITEM, Value::Object(object));
    }
    Ok(Some(Message {
        role: Role::Assistant,
        content: vec![ContentPart::Thinking(ThinkingPart {
            text,
            signature: None,
            redacted: false,
            data: None,
            extensions,
        })],
        extensions: Extensions::new(),
    }))
}

fn reasoning_text(object: &Object) -> String {
    let mut text = String::new();
    if let Some(Value::Array(parts)) = object.get("content") {
        for part in parts {
            let Some(part) = part.as_object() else {
                continue;
            };
            if part.get("type").and_then(Value::as_str) == Some("reasoning_text")
                && let Some(piece) = part.get("text").and_then(Value::as_str)
            {
                text.push_str(piece);
            }
        }
    }
    if !text.is_empty() {
        return text;
    }
    if let Some(Value::Array(parts)) = object.get("summary") {
        for part in parts {
            if let Some(piece) = part.get("text").and_then(Value::as_str) {
                text.push_str(piece);
            }
        }
    }
    text
}

fn decode_body(value: Value, field: &'static str) -> Result<Vec<ContentPart>, CodecError> {
    if let Value::String(text) = value {
        return Ok(vec![text_part(text)]);
    }
    decode_parts(expect_array(value, field)?)
}

fn decode_parts(items: Vec<Value>) -> Result<Vec<ContentPart>, CodecError> {
    let mut parts = Vec::new();
    for item in items {
        parts.push(decode_part(item)?);
    }
    Ok(parts)
}

fn decode_part(item: Value) -> Result<ContentPart, CodecError> {
    if let Value::String(text) = item {
        return Ok(text_part(text));
    }
    let mut object = expect_object(item, "content")?;
    let kind = require_string(&mut object, "type")?;
    match kind.as_str() {
        "input_text" | "output_text" | "text" => {
            let text = take_string(&mut object, "text")?.unwrap_or_default();
            Ok(ContentPart::Text(TextPart {
                text,
                extensions: Extensions::from_map(object),
            }))
        }
        "input_image" => {
            let (source, detail) = take_image(&mut object)?;
            Ok(ContentPart::Image(ImagePart {
                source,
                detail,
                extensions: Extensions::from_map(object),
            }))
        }
        other => Ok(ContentPart::Unknown(UnknownPart {
            kind: other.to_owned(),
            extensions: Extensions::from_map(object),
        })),
    }
}

fn take_image(object: &mut Object) -> Result<(ImageSource, Option<ImageDetail>), CodecError> {
    let Some(value) = object.remove("image_url") else {
        return Err(CodecError::MissingField("image_url"));
    };
    let (url, inner_detail) = match value {
        Value::String(url) => (url, None),
        Value::Object(mut inner) => {
            let url = require_string(&mut inner, "url")?;
            let detail = take_detail(&mut inner)?;
            Ok::<_, CodecError>((url, detail))
        }?,
        _ => {
            return Err(CodecError::WrongType { field: "image_url" });
        }
    };
    let detail = inner_detail.or(take_detail(object)?);
    Ok((image_source(&url), detail))
}

fn take_detail(object: &mut Object) -> Result<Option<ImageDetail>, CodecError> {
    Ok(
        take_string(object, "detail")?.map(|value| match value.as_str() {
            "auto" => ImageDetail::Auto,
            "low" => ImageDetail::Low,
            "high" => ImageDetail::High,
            other => ImageDetail::Other(other.to_owned()),
        }),
    )
}

fn image_source(url: &str) -> ImageSource {
    if let Some((media_type, data)) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        .and_then(|(meta, data)| meta.strip_suffix(";base64").map(|media| (media, data)))
    {
        return ImageSource::Base64 {
            media_type: media_type.to_owned(),
            data: data.to_owned(),
        };
    }
    ImageSource::Url {
        url: url.to_owned(),
    }
}

fn decode_tools(value: Option<Value>) -> Result<Vec<Tool>, CodecError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let mut tools = Vec::new();
    for item in expect_array(value, "tools")? {
        let mut object = expect_object(item, "tools")?;
        let kind = take_string(&mut object, "type")?.unwrap_or_else(|| "function".to_owned());
        if kind != "function" {
            return Err(CodecError::Unsupported {
                message: format!("tool type `{kind}` is not supported"),
            });
        }
        let name = require_string(&mut object, "name")?;
        let description = take_string(&mut object, "description")?;
        let input_schema = object
            .remove("parameters")
            .unwrap_or_else(|| Value::Object(Object::new()));
        tools.push(Tool {
            name,
            description,
            input_schema,
            extensions: Extensions::from_map(object),
        });
    }
    Ok(tools)
}

fn decode_tool_choice(
    value: Option<Value>,
    mut tools: Vec<Tool>,
) -> Result<(Option<ToolChoice>, Vec<Tool>), CodecError> {
    let Some(value) = value else {
        return Ok((None, tools));
    };
    if value.is_null() {
        return Ok((None, tools));
    }
    if let Some(mode) = value.as_str() {
        return Ok((Some(choice_mode(mode)?), tools));
    }
    let mut object = expect_object(value, "tool_choice")?;
    let kind = require_string(&mut object, "type")?;
    match kind.as_str() {
        "function" => {
            let name = require_string(&mut object, "name")?;
            let mut choice = ToolChoice::named(name);
            choice.extensions = Extensions::from_map(object);
            Ok((Some(choice), tools))
        }
        "allowed_tools" => {
            let mode = require_string(&mut object, "mode")?;
            if mode != "auto" && mode != "required" {
                return Err(CodecError::Unsupported {
                    message: format!("allowed_tools mode `{mode}` is not supported"),
                });
            }
            let allowed = allowed_names(object.remove("tools"))?;
            tools.retain(|tool| allowed.contains(&tool.name));
            Ok((Some(choice_mode(&mode)?), tools))
        }
        other => Err(CodecError::Unsupported {
            message: format!("tool_choice type `{other}` is not supported"),
        }),
    }
}

fn allowed_names(value: Option<Value>) -> Result<BTreeSet<String>, CodecError> {
    let Some(value) = value else {
        return Ok(BTreeSet::new());
    };
    let mut names = BTreeSet::new();
    for item in expect_array(value, "tools")? {
        let mut object = expect_object(item, "tools")?;
        if take_string(&mut object, "type")?.as_deref() == Some("function")
            && let Some(name) = take_string(&mut object, "name")?
        {
            names.insert(name);
        }
    }
    Ok(names)
}

fn choice_mode(mode: &str) -> Result<ToolChoice, CodecError> {
    Ok(match mode {
        "auto" => ToolChoice::auto(),
        "none" => ToolChoice::none(),
        "required" => ToolChoice::required(),
        other => {
            return Err(CodecError::Unsupported {
                message: format!("tool_choice `{other}` is not supported"),
            });
        }
    })
}

fn merge_assistant_runs(messages: Vec<Message>) -> Vec<Message> {
    let mut merged: Vec<Message> = Vec::new();
    for message in messages {
        if let Some(last) = merged.last_mut()
            && last.role == Role::Assistant
            && message.role == Role::Assistant
        {
            last.content.extend(message.content);
            last.extensions.absorb(&message.extensions);
            continue;
        }
        merged.push(message);
    }
    merged
}

fn encode_request(request: &Request) -> Result<Vec<u8>, CodecError> {
    let mut object = Object::new();
    object.insert("model".to_owned(), Value::String(request.model.clone()));
    let (instructions, messages) = split_instructions(&request.messages);
    if let Some(text) = instructions {
        object.insert("instructions".to_owned(), Value::String(text.to_owned()));
    }
    object.insert("input".to_owned(), Value::Array(encode_items(messages)?));
    if !request.tools.is_empty() {
        object.insert("tools".to_owned(), encode_tools(&request.tools));
    }
    if let Some(choice) = &request.tool_choice {
        object.insert("tool_choice".to_owned(), encode_tool_choice(choice));
    }
    insert_u64(&mut object, "max_output_tokens", request.params.max_tokens);
    insert_f64(&mut object, "temperature", request.params.temperature)?;
    insert_f64(&mut object, "top_p", request.params.top_p)?;
    if request.stream {
        object.insert("stream".to_owned(), Value::Bool(true));
    }
    request.extensions.merge_into(&mut object);
    to_bytes(&Value::Object(object))
}

fn split_instructions(messages: &[Message]) -> (Option<&str>, &[Message]) {
    let Some(first) = messages.first() else {
        return (None, messages);
    };
    if first.role != Role::System
        || messages
            .iter()
            .skip(1)
            .any(|message| message.role == Role::System)
    {
        return (None, messages);
    }
    let Some(ContentPart::Text(text)) = first.content.first() else {
        return (None, messages);
    };
    if first.content.len() == 1 && first.extensions.is_empty() && text.extensions.is_empty() {
        (Some(text.text.as_str()), &messages[1..])
    } else {
        (None, messages)
    }
}

fn encode_items(messages: &[Message]) -> Result<Vec<Value>, CodecError> {
    let mut items = Vec::new();
    for message in messages {
        push_message_items(message, &mut items)?;
    }
    Ok(items)
}

fn push_message_items(message: &Message, items: &mut Vec<Value>) -> Result<(), CodecError> {
    let mut content = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text(text) => content.push(text_item(message.role, text)),
            ContentPart::Image(image) => content.push(image_item(image)),
            ContentPart::Unknown(unknown) => content.push(unknown_item(unknown)),
            ContentPart::Thinking(thinking) => {
                flush_content(message, &mut content, items);
                if let Some(item) = thinking_item(thinking) {
                    items.push(item);
                }
            }
            ContentPart::ToolCall(call) => {
                flush_content(message, &mut content, items);
                items.push(function_call_item(call));
            }
            ContentPart::ToolResult(result) => {
                flush_content(message, &mut content, items);
                items.push(function_output_item(result));
            }
        }
    }
    flush_content(message, &mut content, items);
    Ok(())
}

fn flush_content(message: &Message, content: &mut Vec<Value>, items: &mut Vec<Value>) {
    if content.is_empty() {
        return;
    }
    let mut item = Object::new();
    item.insert("type".to_owned(), Value::String("message".to_owned()));
    item.insert(
        "role".to_owned(),
        Value::String(role_name(message.role).to_owned()),
    );
    item.insert("content".to_owned(), Value::Array(std::mem::take(content)));
    message.extensions.merge_into(&mut item);
    items.push(Value::Object(item));
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
    }
}

fn text_item(role: Role, text: &TextPart) -> Value {
    let mut item = Object::new();
    let kind = if role == Role::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    item.insert("type".to_owned(), Value::String(kind.to_owned()));
    item.insert("text".to_owned(), Value::String(text.text.clone()));
    text.extensions.merge_into(&mut item);
    Value::Object(item)
}

fn image_item(image: &ImagePart) -> Value {
    let mut item = Object::new();
    item.insert("type".to_owned(), Value::String("input_image".to_owned()));
    item.insert(
        "image_url".to_owned(),
        Value::String(image_url(&image.source)),
    );
    if let Some(detail) = &image.detail {
        item.insert(
            "detail".to_owned(),
            Value::String(detail_name(detail).to_owned()),
        );
    }
    image.extensions.merge_into(&mut item);
    Value::Object(item)
}

fn image_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    }
}

fn detail_name(detail: &ImageDetail) -> &str {
    match detail {
        ImageDetail::Auto => "auto",
        ImageDetail::Low => "low",
        ImageDetail::High => "high",
        ImageDetail::Other(value) => value,
    }
}

fn unknown_item(part: &UnknownPart) -> Value {
    let mut item = Object::new();
    item.insert("type".to_owned(), Value::String(part.kind.clone()));
    part.extensions.merge_into(&mut item);
    Value::Object(item)
}

fn thinking_item(part: &ThinkingPart) -> Option<Value> {
    if let Some(Value::Object(sealed)) = part.extensions.get(SEALED_ITEM) {
        return Some(Value::Object(sealed.clone()));
    }
    if part.text.is_empty() {
        return None;
    }
    let mut item = Object::new();
    item.insert("type".to_owned(), Value::String("reasoning".to_owned()));
    item.insert(
        "content".to_owned(),
        Value::Array(vec![serde_json::json!({
            "type": "reasoning_text",
            "text": part.text,
        })]),
    );
    merge_except(&part.extensions, &mut item, SEALED_ITEM);
    Some(Value::Object(item))
}

fn function_call_item(call: &ToolCallPart) -> Value {
    let mut item = Object::new();
    item.insert("type".to_owned(), Value::String("function_call".to_owned()));
    item.insert("call_id".to_owned(), Value::String(call.id.clone()));
    item.insert("name".to_owned(), Value::String(call.name.clone()));
    item.insert(
        "arguments".to_owned(),
        Value::String(call.arguments.to_text()),
    );
    call.extensions.merge_into(&mut item);
    Value::Object(item)
}

fn function_output_item(result: &ToolResultPart) -> Value {
    let mut item = Object::new();
    item.insert(
        "type".to_owned(),
        Value::String("function_call_output".to_owned()),
    );
    item.insert("call_id".to_owned(), Value::String(result.id.clone()));
    item.insert("output".to_owned(), encode_tool_output(&result.content));
    result.extensions.merge_into(&mut item);
    Value::Object(item)
}

fn encode_tool_output(parts: &[ContentPart]) -> Value {
    if parts.is_empty() {
        return Value::String(String::new());
    }
    if let [ContentPart::Text(text)] = parts
        && text.extensions.is_empty()
    {
        return Value::String(text.text.clone());
    }
    Value::Array(parts.iter().map(output_part).collect())
}

fn output_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text(text) => text_item(Role::User, text),
        ContentPart::Image(image) => image_item(image),
        ContentPart::Unknown(unknown) => unknown_item(unknown),
        ContentPart::Thinking(_) | ContentPart::ToolCall(_) | ContentPart::ToolResult(_) => {
            serde_json::json!({ "type": "input_text", "text": "" })
        }
    }
}

fn encode_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                let mut item = Object::new();
                item.insert("type".to_owned(), Value::String("function".to_owned()));
                item.insert("name".to_owned(), Value::String(tool.name.clone()));
                if let Some(description) = &tool.description {
                    item.insert("description".to_owned(), Value::String(description.clone()));
                }
                item.insert("parameters".to_owned(), tool.input_schema.clone());
                tool.extensions.merge_into(&mut item);
                Value::Object(item)
            })
            .collect(),
    )
}

fn encode_tool_choice(choice: &ToolChoice) -> Value {
    let value = match &choice.mode {
        ToolChoiceMode::Auto => Value::String("auto".to_owned()),
        ToolChoiceMode::None => Value::String("none".to_owned()),
        ToolChoiceMode::Required => Value::String("required".to_owned()),
        ToolChoiceMode::Named { name } => serde_json::json!({
            "type": "function",
            "name": name,
        }),
    };
    match value {
        Value::Object(mut object) => {
            choice.extensions.merge_into(&mut object);
            Value::Object(object)
        }
        other => other,
    }
}

fn decode_response(bytes: &[u8]) -> Result<Response, CodecError> {
    let mut object = parse_object(bytes)?;
    let _ = object.remove("object");
    let id = take_string(&mut object, "id")?;
    let model = take_string(&mut object, "model")?;
    let status = take_string(&mut object, "status")?.unwrap_or_else(|| "completed".to_owned());
    let reason = incomplete_reason(object.remove("incomplete_details"))?;
    if object.get("error").is_some_and(Value::is_null) {
        object.remove("error");
    }
    let output = match object.remove("output") {
        Some(value) if !value.is_null() => decode_input(value)?,
        _ => Vec::new(),
    };
    let message = flatten_assistant(output);
    let has_tool = message
        .content
        .iter()
        .any(|part| matches!(part, ContentPart::ToolCall(_)));
    let usage = match take_object(&mut object, "usage")? {
        Some(usage) => Some(decode_usage(usage)?),
        None => None,
    };
    Ok(Response {
        id,
        model,
        message,
        stop: Some(stop_from(&status, reason.as_deref(), has_tool)),
        usage,
        extensions: Extensions::from_map(object),
    })
}

fn flatten_assistant(messages: Vec<Message>) -> Message {
    let mut content = Vec::new();
    let mut extensions = Extensions::new();
    for message in messages {
        content.extend(message.content);
        extensions.absorb(&message.extensions);
    }
    Message {
        role: Role::Assistant,
        content,
        extensions,
    }
}

fn incomplete_reason(value: Option<Value>) -> Result<Option<String>, CodecError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let mut object = expect_object(value, "incomplete_details")?;
    take_string(&mut object, "reason")
}

fn stop_from(status: &str, reason: Option<&str>, has_tool: bool) -> Stop {
    let mapped = match status {
        "incomplete" => match reason {
            Some("max_output_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::ContentFilter,
            Some("refusal") => StopReason::Refusal,
            Some("stop_sequence") => StopReason::StopSequence,
            Some(other) => StopReason::Other(other.to_owned()),
            None => StopReason::Other("incomplete".to_owned()),
        },
        "failed" => StopReason::Other("failed".to_owned()),
        "cancelled" => StopReason::Other("cancelled".to_owned()),
        "completed" if has_tool => StopReason::ToolUse,
        "completed" | "" => StopReason::EndTurn,
        other => StopReason::Other(other.to_owned()),
    };
    Stop {
        reason: mapped,
        sequence: None,
    }
}

fn decode_usage(mut object: Object) -> Result<Usage, CodecError> {
    let input_tokens = take_u64(&mut object, "input_tokens")?;
    let output_tokens = take_u64(&mut object, "output_tokens")?;
    let total_tokens = take_u64(&mut object, "total_tokens")?;
    let (cache_read_tokens, cache_write_tokens) =
        match take_object(&mut object, "input_tokens_details")? {
            Some(mut details) => {
                let read = take_u64(&mut details, "cached_tokens")?;
                let write = take_u64(&mut details, "cache_write_tokens")?;
                if !details.is_empty() {
                    object.insert("input_tokens_details".to_owned(), Value::Object(details));
                }
                (read, write)
            }
            None => (None, None),
        };
    let reasoning_tokens = match take_object(&mut object, "output_tokens_details")? {
        Some(mut details) => {
            let reasoning = take_u64(&mut details, "reasoning_tokens")?;
            if !details.is_empty() {
                object.insert("output_tokens_details".to_owned(), Value::Object(details));
            }
            reasoning
        }
        None => None,
    };
    Ok(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        extensions: Extensions::from_map(object),
    })
}

fn encode_response(response: &Response) -> Result<Vec<u8>, CodecError> {
    if response.message.role != Role::Assistant {
        return Err(CodecError::Unsupported {
            message: "a response message must use the assistant role".to_owned(),
        });
    }
    let mut object = Object::new();
    object.insert("object".to_owned(), Value::String("response".to_owned()));
    if let Some(id) = &response.id {
        object.insert("id".to_owned(), Value::String(id.clone()));
    }
    if let Some(model) = &response.model {
        object.insert("model".to_owned(), Value::String(model.clone()));
    }
    let (status, details) = encode_status(response.stop.as_ref());
    object.insert("status".to_owned(), Value::String(status));
    if let Some(details) = details {
        object.insert("incomplete_details".to_owned(), details);
    }
    object.insert(
        "output".to_owned(),
        Value::Array(encode_items(std::slice::from_ref(&response.message))?),
    );
    if let Some(usage) = &response.usage {
        object.insert("usage".to_owned(), encode_usage(usage));
    }
    response.extensions.merge_into(&mut object);
    to_bytes(&Value::Object(object))
}

fn encode_status(stop: Option<&Stop>) -> (String, Option<Value>) {
    let Some(stop) = stop else {
        return ("completed".to_owned(), None);
    };
    match &stop.reason {
        StopReason::EndTurn | StopReason::ToolUse => ("completed".to_owned(), None),
        StopReason::MaxTokens => (
            "incomplete".to_owned(),
            Some(serde_json::json!({"reason": "max_output_tokens"})),
        ),
        StopReason::ContentFilter => (
            "incomplete".to_owned(),
            Some(serde_json::json!({"reason": "content_filter"})),
        ),
        StopReason::Refusal => (
            "incomplete".to_owned(),
            Some(serde_json::json!({"reason": "refusal"})),
        ),
        StopReason::StopSequence => (
            "incomplete".to_owned(),
            Some(serde_json::json!({"reason": "stop_sequence"})),
        ),
        StopReason::Other(token)
            if matches!(
                token.as_str(),
                "failed" | "cancelled" | "incomplete" | "in_progress" | "queued"
            ) =>
        {
            (token.clone(), None)
        }
        StopReason::Other(token) => (
            "incomplete".to_owned(),
            Some(serde_json::json!({ "reason": token })),
        ),
    }
}

fn encode_usage(usage: &Usage) -> Value {
    let mut object = Object::new();
    insert_u64(&mut object, "input_tokens", usage.input_tokens);
    insert_u64(&mut object, "output_tokens", usage.output_tokens);
    insert_u64(&mut object, "total_tokens", usage.total_tokens);
    if usage.cache_read_tokens.is_some() || usage.cache_write_tokens.is_some() {
        let mut details = Object::new();
        insert_u64(&mut details, "cached_tokens", usage.cache_read_tokens);
        insert_u64(&mut details, "cache_write_tokens", usage.cache_write_tokens);
        object.insert("input_tokens_details".to_owned(), Value::Object(details));
    }
    if usage.reasoning_tokens.is_some() {
        let mut details = Object::new();
        insert_u64(&mut details, "reasoning_tokens", usage.reasoning_tokens);
        object.insert("output_tokens_details".to_owned(), Value::Object(details));
    }
    usage.extensions.merge_into(&mut object);
    Value::Object(object)
}

fn encode_stream(events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
    let mut encoder = Encoder::default();
    let mut out = Vec::new();
    for event in events {
        out.extend(encoder.push(event)?);
    }
    out.extend(encoder.finish()?);
    Ok(out)
}

#[derive(Default)]
struct Encoder {
    inner: StreamEncoder,
}

impl crate::StreamEncoder for Encoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        let before = self.inner.out.len();
        self.inner.push(event)?;
        Ok(self.inner.out.split_off(before))
    }

    fn finish(&mut self) -> Result<Vec<u8>, CodecError> {
        let before = self.inner.out.len();
        self.inner.complete();
        Ok(self.inner.out.split_off(before))
    }
}

struct ItemState {
    kind: ItemKind,
    id: String,
    call_id: String,
    name: String,
    text: String,
}

#[derive(Default)]
struct StreamEncoder {
    out: Vec<u8>,
    id: Option<String>,
    model: Option<String>,
    open: BTreeMap<u32, ItemState>,
    done: BTreeMap<u32, Value>,
    stop: Option<Stop>,
    usage: Option<Usage>,
    sequence: u64,
    completed: bool,
}

#[derive(Clone, Copy)]
enum ItemKind {
    Message,
    Reasoning,
    Function,
}

impl StreamEncoder {
    fn push(&mut self, event: &StreamEvent) -> Result<(), CodecError> {
        match event {
            StreamEvent::MessageStart(start) => {
                self.id.clone_from(&start.id);
                self.model.clone_from(&start.model);
                self.write(
                    "response.created",
                    serde_json::json!({
                        "type": "response.created",
                        "response": self.skeleton("in_progress"),
                    }),
                );
            }
            StreamEvent::ContentBlockStart(start) => self.open_block(start)?,
            StreamEvent::TextDelta(delta) => {
                self.ensure(delta.index, ItemKind::Message);
                if let Some(item) = self.open.get_mut(&delta.index) {
                    item.text.push_str(&delta.text);
                }
                self.write(
                    "response.output_text.delta",
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "output_index": delta.index,
                        "content_index": 0,
                        "delta": delta.text,
                    }),
                );
            }
            StreamEvent::ThinkingDelta(delta) => {
                self.ensure(delta.index, ItemKind::Reasoning);
                if let Some(item) = self.open.get_mut(&delta.index) {
                    item.text.push_str(&delta.text);
                }
                self.write(
                    "response.reasoning_text.delta",
                    serde_json::json!({
                        "type": "response.reasoning_text.delta",
                        "output_index": delta.index,
                        "content_index": 0,
                        "delta": delta.text,
                    }),
                );
            }
            StreamEvent::ToolArgumentsDelta(delta) => {
                self.ensure(delta.index, ItemKind::Function);
                if let Some(item) = self.open.get_mut(&delta.index) {
                    item.text.push_str(&delta.partial_json);
                }
                self.write(
                    "response.function_call_arguments.delta",
                    serde_json::json!({
                        "type": "response.function_call_arguments.delta",
                        "output_index": delta.index,
                        "delta": delta.partial_json,
                    }),
                );
            }
            StreamEvent::SignatureDelta(_) => {}
            StreamEvent::ContentBlockStop(stop) => self.close(stop.index, &stop.extensions),
            StreamEvent::MessageDelta(delta) => {
                if delta.stop.is_some() {
                    self.stop.clone_from(&delta.stop);
                }
                if delta.usage.is_some() {
                    self.usage.clone_from(&delta.usage);
                }
            }
            StreamEvent::MessageStop(_) => self.complete(),
            StreamEvent::Ping(ping) => {
                let mut payload = Object::new();
                ping.extensions.merge_into(&mut payload);
                self.write("ping", Value::Object(payload));
            }
            StreamEvent::Unknown(unknown) => {
                self.write(&unknown.name, unknown.payload.clone());
            }
        }
        Ok(())
    }

    fn open_block(&mut self, start: &ContentBlockStart) -> Result<(), CodecError> {
        if self.open.contains_key(&start.index) {
            return Ok(());
        }
        match &start.block {
            BlockStart::Unknown { .. } => Err(CodecError::Unsupported {
                message: "unknown stream blocks cannot be encoded".to_owned(),
            }),
            block => {
                self.write_open(start.index, block, &start.extensions);
                Ok(())
            }
        }
    }

    fn ensure(&mut self, index: u32, kind: ItemKind) {
        if self.open.contains_key(&index) {
            return;
        }
        let block = match kind {
            ItemKind::Message => BlockStart::Text,
            ItemKind::Reasoning => BlockStart::Thinking {
                redacted: false,
                data: None,
            },
            ItemKind::Function => BlockStart::ToolCall {
                id: String::new(),
                name: String::new(),
            },
        };
        self.write_open(index, &block, &Extensions::new());
    }

    fn write_open(&mut self, index: u32, block: &BlockStart, extensions: &Extensions) {
        if self.open.contains_key(&index) {
            return;
        }
        let Some(state) = ItemState::from_block(index, block) else {
            return;
        };
        let kind = state.kind;
        let item = state.added_item(extensions);
        self.open.insert(index, state);
        self.write(
            "response.output_item.added",
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": item,
            }),
        );
        if matches!(kind, ItemKind::Message) {
            self.write(
                "response.content_part.added",
                serde_json::json!({
                    "type": "response.content_part.added",
                    "output_index": index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            );
        }
    }

    fn close(&mut self, index: u32, extensions: &Extensions) {
        let Some(state) = self.open.remove(&index) else {
            return;
        };
        let item = match state.kind {
            ItemKind::Message => {
                let text = state.text;
                let part = serde_json::json!({
                    "type": "output_text",
                    "text": text.clone(),
                    "annotations": [],
                });
                self.write(
                    "response.output_text.done",
                    serde_json::json!({
                        "type": "response.output_text.done",
                        "output_index": index,
                        "content_index": 0,
                        "text": text,
                    }),
                );
                self.write(
                    "response.content_part.done",
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "output_index": index,
                        "content_index": 0,
                        "part": part.clone(),
                    }),
                );
                serde_json::json!({
                    "id": state.id,
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                    "content": [part],
                })
            }
            ItemKind::Function => serde_json::json!({
                "id": state.id,
                "type": "function_call",
                "status": "completed",
                "call_id": state.call_id,
                "name": state.name,
                "arguments": state.text,
            }),
            ItemKind::Reasoning => match extensions.get(SEALED_ITEM) {
                Some(Value::Object(sealed)) => {
                    let mut sealed = sealed.clone();
                    sealed
                        .entry("id".to_owned())
                        .or_insert_with(|| Value::String(state.id.clone()));
                    Value::Object(sealed)
                }
                _ => {
                    let text = state.text;
                    let summary = text.clone();
                    serde_json::json!({
                        "id": state.id,
                        "type": "reasoning",
                        "status": "completed",
                        "content": [{"type": "reasoning_text", "text": text}],
                        "summary": [{"type": "summary_text", "text": summary}],
                    })
                }
            },
        };
        self.done.insert(index, item.clone());
        self.write(
            "response.output_item.done",
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": item,
            }),
        );
    }

    fn complete(&mut self) {
        if self.completed {
            return;
        }
        let indexes: Vec<u32> = self.open.keys().copied().collect();
        for index in indexes {
            self.close(index, &Extensions::new());
        }
        let (status, details) = encode_status(self.stop.as_ref());
        let event = match status.as_str() {
            "incomplete" => "response.incomplete",
            "failed" => "response.failed",
            _ => "response.completed",
        };
        let mut response = self.skeleton(&status);
        let output: Vec<Value> = self.done.values().cloned().collect();
        let object = response.as_object_mut().expect("skeleton is an object");
        object.insert("output".to_owned(), Value::Array(output));
        if let Some(details) = details {
            object.insert("incomplete_details".to_owned(), details);
        }
        if let Some(usage) = &self.usage {
            object.insert("usage".to_owned(), encode_usage(usage));
        }
        self.completed = true;
        self.write(
            event,
            serde_json::json!({
                "type": event,
                "response": response,
            }),
        );
    }

    fn skeleton(&self, status: &str) -> Value {
        let mut response = Object::new();
        response.insert("object".to_owned(), Value::String("response".to_owned()));
        response.insert("status".to_owned(), Value::String(status.to_owned()));
        response.insert("output".to_owned(), Value::Array(Vec::new()));
        if let Some(id) = &self.id {
            response.insert("id".to_owned(), Value::String(id.clone()));
        }
        if let Some(model) = &self.model {
            response.insert("model".to_owned(), Value::String(model.clone()));
        }
        Value::Object(response)
    }

    fn write(&mut self, event: &str, mut value: Value) {
        if let Some(object) = value.as_object_mut() {
            object
                .entry("sequence_number".to_owned())
                .or_insert_with(|| Value::from(self.sequence));
            self.sequence += 1;
        }
        let Ok(data) = serde_json::to_string(&value) else {
            return;
        };
        sse::write_event(&mut self.out, Some(event), &data);
    }
}

impl ItemState {
    fn from_block(index: u32, block: &BlockStart) -> Option<Self> {
        let state = match block {
            BlockStart::Text => Self {
                kind: ItemKind::Message,
                id: format!("msg_{index}"),
                call_id: String::new(),
                name: String::new(),
                text: String::new(),
            },
            BlockStart::Thinking { .. } => Self {
                kind: ItemKind::Reasoning,
                id: format!("rs_{index}"),
                call_id: String::new(),
                name: String::new(),
                text: String::new(),
            },
            BlockStart::ToolCall { id, name } => Self {
                kind: ItemKind::Function,
                id: format!("fc_{index}"),
                call_id: id.clone(),
                name: name.clone(),
                text: String::new(),
            },
            BlockStart::Unknown { .. } => return None,
        };
        Some(state)
    }

    fn added_item(&self, extensions: &Extensions) -> Value {
        match self.kind {
            ItemKind::Message => serde_json::json!({
                "id": self.id,
                "type": "message",
                "role": "assistant",
                "status": "in_progress",
                "content": [],
            }),
            ItemKind::Reasoning => match extensions.get(SEALED_ITEM) {
                Some(Value::Object(sealed)) => {
                    let mut sealed = sealed.clone();
                    sealed
                        .entry("id".to_owned())
                        .or_insert_with(|| Value::String(self.id.clone()));
                    Value::Object(sealed)
                }
                _ => serde_json::json!({
                    "id": self.id,
                    "type": "reasoning",
                    "summary": [],
                }),
            },
            ItemKind::Function => serde_json::json!({
                "id": self.id,
                "type": "function_call",
                "call_id": self.call_id,
                "name": self.name,
                "arguments": "",
                "status": "in_progress",
            }),
        }
    }
}

#[derive(Debug, Default)]
struct Decoder {
    parser: SseParser,
    started: bool,
    called: bool,
    open: BTreeSet<u32>,
    deltas: BTreeSet<u32>,
    args: BTreeMap<u32, String>,
}

impl StreamDecoder for Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.push(chunk)?;
        self.frames(frames)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.finish()?;
        self.frames(frames)
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
            if frame.event.as_deref() == Some("ping") {
                return Ok(vec![StreamEvent::Ping(Ping::default())]);
            }
            return Ok(Vec::new());
        }
        let value: Value = serde_json::from_str(&frame.data)?;
        let Value::Object(mut object) = value else {
            return Ok(vec![StreamEvent::Unknown(UnknownEvent {
                name: frame.event.unwrap_or_else(|| "data".to_owned()),
                payload: value,
            })]);
        };
        let name = object
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(frame.event)
            .unwrap_or_else(|| "message".to_owned());
        let mut events = Vec::new();
        match name.as_str() {
            "response.created" | "response.in_progress" => {
                let _ = object.remove("type");
                self.created(object, &mut events)?;
            }
            "response.output_item.added" => {
                let _ = object.remove("type");
                self.item_added(object, &mut events)?;
            }
            "response.content_part.added" => {
                let _ = object.remove("type");
                self.part_added(object, &mut events)?;
            }
            "response.output_text.delta" => {
                let _ = object.remove("type");
                self.text_delta(object, &mut events)?;
            }
            "response.output_text.done" => {
                let _ = object.remove("type");
                self.text_done(object, &mut events)?;
            }
            "response.content_part.done" => {}
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let _ = object.remove("type");
                self.thinking_delta(object, &mut events)?;
            }
            "response.function_call_arguments.delta" => {
                let _ = object.remove("type");
                self.args_delta(object, &mut events)?;
            }
            "response.function_call_arguments.done" => {
                let _ = object.remove("type");
                self.args_done(object, &mut events)?;
            }
            "response.output_item.done" => {
                let _ = object.remove("type");
                self.item_done(object, &mut events)?;
            }
            "response.failed" if !self.started && output_is_empty(&object) => {
                events.push(StreamEvent::Unknown(UnknownEvent {
                    name,
                    payload: Value::Object(object),
                }));
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let _ = object.remove("type");
                self.terminal(object, &mut events)?;
            }
            "ping" => events.push(StreamEvent::Ping(Ping::default())),
            _ => events.push(StreamEvent::Unknown(UnknownEvent {
                name,
                payload: Value::Object(object),
            })),
        }
        Ok(events)
    }

    fn created(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        if self.started {
            return Ok(());
        }
        let mut response = take_object(&mut object, "response")?.unwrap_or_default();
        let id = take_string(&mut response, "id")?;
        let model = take_string(&mut response, "model")?;
        self.started = true;
        events.push(StreamEvent::MessageStart(MessageStart {
            id,
            model,
            usage: None,
            extensions: Extensions::new(),
        }));
        Ok(())
    }

    fn item_added(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        self.ensure_started(events);
        let index = index_of(&mut object)?;
        let Some(mut item) = take_object(&mut object, "item")? else {
            return Ok(());
        };
        let kind = take_string(&mut item, "type")?.unwrap_or_default();
        match kind.as_str() {
            "function_call" => {
                self.called = true;
                let id = take_string(&mut item, "call_id")?.unwrap_or_default();
                let name = take_string(&mut item, "name")?.unwrap_or_default();
                self.open_tool(index, id, name, events);
            }
            "reasoning" => {
                let extensions = seal_extensions(&item);
                self.open_thinking(index, extensions, events);
            }
            _ => {}
        }
        Ok(())
    }

    fn part_added(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        self.ensure_started(events);
        let index = index_of(&mut object)?;
        let Some(mut part) = take_object(&mut object, "part")? else {
            return Ok(());
        };
        if take_string(&mut part, "type")?.as_deref() == Some("output_text") {
            self.open_text(index, events);
        }
        Ok(())
    }

    fn text_delta(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        self.ensure_started(events);
        let index = index_of(&mut object)?;
        let text = take_string(&mut object, "delta")?.unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        self.open_text(index, events);
        self.deltas.insert(index);
        events.push(StreamEvent::TextDelta(TextDelta {
            index,
            text,
            extensions: Extensions::new(),
        }));
        Ok(())
    }

    /// A done frame is the whole text when no deltas arrived. After deltas it
    /// repeats that text, so it does not append a second copy.
    fn text_done(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        self.ensure_started(events);
        let index = index_of(&mut object)?;
        if self.deltas.contains(&index) {
            return Ok(());
        }
        let text = take_string(&mut object, "text")?.unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        self.open_text(index, events);
        self.deltas.insert(index);
        events.push(StreamEvent::TextDelta(TextDelta {
            index,
            text,
            extensions: Extensions::new(),
        }));
        Ok(())
    }

    fn thinking_delta(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        self.ensure_started(events);
        let index = index_of(&mut object)?;
        let text = take_string(&mut object, "delta")?.unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        self.open_thinking(index, Extensions::new(), events);
        self.deltas.insert(index);
        events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
            index,
            text,
            extensions: Extensions::new(),
        }));
        Ok(())
    }

    fn args_delta(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        self.ensure_started(events);
        let index = index_of(&mut object)?;
        let text = take_string(&mut object, "delta")?.unwrap_or_default();
        if text.is_empty() {
            return Ok(());
        }
        self.called = true;
        self.open_tool(index, String::new(), String::new(), events);
        self.args.entry(index).or_default().push_str(&text);
        self.deltas.insert(index);
        events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
            index,
            partial_json: text,
            extensions: Extensions::new(),
        }));
        Ok(())
    }

    fn args_done(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        let index = index_of(&mut object)?;
        let done = take_string(&mut object, "arguments")?.unwrap_or_default();
        self.emit_arg_suffix(index, &done, events);
        Ok(())
    }

    fn item_done(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        self.ensure_started(events);
        let index = index_of(&mut object)?;
        let mut seal = Extensions::new();
        if let Some(mut item) = take_object(&mut object, "item")? {
            let kind = take_string(&mut item, "type")?.unwrap_or_default();
            if kind == "reasoning" {
                seal = seal_extensions(&item);
            }
            if !self.deltas.contains(&index) {
                match kind.as_str() {
                    "message" => {
                        if let Some(content) = item.remove("content") {
                            let text = join_text(&decode_body(content, "content")?);
                            if !text.is_empty() {
                                self.open_text(index, events);
                                self.deltas.insert(index);
                                events.push(StreamEvent::TextDelta(TextDelta {
                                    index,
                                    text,
                                    extensions: Extensions::new(),
                                }));
                            }
                        }
                    }
                    "function_call" => {
                        self.called = true;
                        let id = take_string(&mut item, "call_id")?.unwrap_or_default();
                        let name = take_string(&mut item, "name")?.unwrap_or_default();
                        let arguments = take_string(&mut item, "arguments")?.unwrap_or_default();
                        self.open_tool(index, id, name, events);
                        self.emit_arg_suffix(index, &arguments, events);
                    }
                    "reasoning" => {
                        let text = reasoning_text(&item);
                        if !text.is_empty() || !seal.is_empty() {
                            self.open_thinking(index, seal.clone(), events);
                            if !text.is_empty() {
                                self.deltas.insert(index);
                                events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                                    index,
                                    text,
                                    extensions: Extensions::new(),
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            } else if kind == "function_call" {
                let arguments = take_string(&mut item, "arguments")?.unwrap_or_default();
                self.emit_arg_suffix(index, &arguments, events);
            }
        }
        self.close(index, seal, events);
        Ok(())
    }

    fn terminal(
        &mut self,
        mut object: Object,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        let mut response = take_object(&mut object, "response")?.unwrap_or_default();
        if !self.started {
            let id = take_string(&mut response, "id")?;
            let model = take_string(&mut response, "model")?;
            self.started = true;
            events.push(StreamEvent::MessageStart(MessageStart {
                id,
                model,
                usage: None,
                extensions: Extensions::new(),
            }));
        } else {
            let _ = response.remove("id");
            let _ = response.remove("model");
        }
        if self.deltas.is_empty()
            && let Some(output) = response.remove("output")
            && !output.is_null()
        {
            self.expand_output(output, events)?;
        }
        let status =
            take_string(&mut response, "status")?.unwrap_or_else(|| "completed".to_owned());
        let reason = incomplete_reason(response.remove("incomplete_details"))?;
        let usage = match take_object(&mut response, "usage")? {
            Some(usage) => Some(decode_usage(usage)?),
            None => None,
        };
        let indexes: Vec<u32> = self.open.iter().copied().collect();
        for index in indexes {
            self.close(index, Extensions::new(), events);
        }
        events.push(StreamEvent::MessageDelta(MessageDelta {
            stop: Some(stop_from(&status, reason.as_deref(), self.called)),
            usage,
            extensions: Extensions::new(),
        }));
        events.push(StreamEvent::MessageStop(MessageStop {
            extensions: Extensions::new(),
        }));
        Ok(())
    }

    fn expand_output(
        &mut self,
        output: Value,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        let messages = decode_input(output)?;
        let mut index = 0_u32;
        for message in messages {
            for part in message.content {
                match part {
                    ContentPart::Text(text) if !text.text.is_empty() => {
                        self.open_text(index, events);
                        self.deltas.insert(index);
                        events.push(StreamEvent::TextDelta(TextDelta {
                            index,
                            text: text.text,
                            extensions: Extensions::new(),
                        }));
                        self.close(index, Extensions::new(), events);
                        index += 1;
                    }
                    ContentPart::Thinking(thinking)
                        if !thinking.text.is_empty() || !thinking.extensions.is_empty() =>
                    {
                        self.open_thinking(index, thinking.extensions.clone(), events);
                        if !thinking.text.is_empty() {
                            self.deltas.insert(index);
                            events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                                index,
                                text: thinking.text,
                                extensions: Extensions::new(),
                            }));
                        }
                        self.close(index, thinking.extensions, events);
                        index += 1;
                    }
                    ContentPart::ToolCall(call) => {
                        self.called = true;
                        self.open_tool(index, call.id, call.name, events);
                        let arguments = call.arguments.to_text();
                        if !arguments.is_empty() {
                            self.emit_arg_suffix(index, &arguments, events);
                        }
                        self.close(index, Extensions::new(), events);
                        index += 1;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn emit_arg_suffix(&mut self, index: u32, done: &str, events: &mut Vec<StreamEvent>) {
        if done.is_empty() {
            return;
        }
        let sent = self.args.get(&index).map(String::as_str).unwrap_or("");
        let rest = if let Some(suffix) = done.strip_prefix(sent) {
            suffix.to_owned()
        } else if sent.is_empty() {
            done.to_owned()
        } else {
            return;
        };
        if rest.is_empty() {
            return;
        }
        self.called = true;
        self.open_tool(index, String::new(), String::new(), events);
        self.args.entry(index).or_default().push_str(&rest);
        self.deltas.insert(index);
        events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
            index,
            partial_json: rest,
            extensions: Extensions::new(),
        }));
    }

    fn ensure_started(&mut self, events: &mut Vec<StreamEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        events.push(StreamEvent::MessageStart(MessageStart {
            id: None,
            model: None,
            usage: None,
            extensions: Extensions::new(),
        }));
    }

    fn open_text(&mut self, index: u32, events: &mut Vec<StreamEvent>) {
        if !self.open.insert(index) {
            return;
        }
        events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
            index,
            block: BlockStart::Text,
            extensions: Extensions::new(),
        }));
    }

    fn open_thinking(&mut self, index: u32, extensions: Extensions, events: &mut Vec<StreamEvent>) {
        if !self.open.insert(index) {
            return;
        }
        events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
            index,
            block: BlockStart::Thinking {
                redacted: false,
                data: None,
            },
            extensions,
        }));
    }

    fn open_tool(&mut self, index: u32, id: String, name: String, events: &mut Vec<StreamEvent>) {
        if !self.open.insert(index) {
            return;
        }
        events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
            index,
            block: BlockStart::ToolCall { id, name },
            extensions: Extensions::new(),
        }));
    }

    fn close(&mut self, index: u32, extensions: Extensions, events: &mut Vec<StreamEvent>) {
        if !self.open.remove(&index) {
            return;
        }
        events.push(StreamEvent::ContentBlockStop(ContentBlockStop {
            index,
            extensions,
        }));
    }
}

/// A failed turn that never started and has no output is not a reply yet.
fn output_is_empty(object: &Object) -> bool {
    let Some(response) = object.get("response").and_then(Value::as_object) else {
        return true;
    };
    match response.get("output") {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        Some(_) => false,
    }
}

fn index_of(object: &mut Object) -> Result<u32, CodecError> {
    match take_u64(object, "output_index")? {
        Some(value) => u32::try_from(value).map_err(|_| CodecError::Unsupported {
            message: "output_index does not fit in u32".to_owned(),
        }),
        None => Ok(0),
    }
}

fn join_text(parts: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in parts {
        if let ContentPart::Text(part) = part {
            text.push_str(&part.text);
        }
    }
    text
}

fn repair_orphaned_outputs(object: &mut Object) {
    let Some(Value::Array(items)) = object.get_mut("input") else {
        return;
    };
    let mut pending = Vec::new();
    let mut answered = BTreeSet::new();
    for item in items.iter() {
        let Some(item) = item.as_object() else {
            continue;
        };
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "function_call" => pending.push(id.to_owned()),
            "function_call_output" => {
                answered.insert(id.to_owned());
            }
            _ => {}
        }
    }
    for id in pending {
        if answered.insert(id.clone()) {
            items.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": id,
                "output": ""
            }));
        }
    }
}

fn merge_except(extensions: &Extensions, object: &mut Object, skip: &str) {
    for (key, value) in extensions.iter() {
        if key == skip {
            continue;
        }
        object.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn text_part(text: String) -> ContentPart {
    ContentPart::Text(TextPart {
        text,
        extensions: Extensions::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::OpenAiResponses;
    use crate::Codec;

    #[test]
    fn error_body_uses_the_status() {
        let body = OpenAiResponses.encode_error(429, "slow down");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["error"]["type"], "rate_limit_error");
        assert_eq!(value["error"]["message"], "slow down");
    }
}
