//! OpenAI Responses API codec.
//!
//! A turn is a list of items (`message`, `function_call`, `function_call_output`,
//! `reasoning`). Consecutive assistant items are one IR message. See
//! `docs/codecs.md` for the conversions that drop data.

use std::collections::{BTreeMap, BTreeSet};

use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Effort, Extensions,
    GenerationParams, ImageDetail, ImagePart, ImageSource, Message, MessageDelta, MessageStart,
    MessageStop, Ping, Request, Response, Role, Stop, StopReason, StreamEvent, TextDelta, TextPart,
    ThinkingDelta, ThinkingPart, Tool, ToolArguments, ToolArgumentsDelta, ToolCallPart, ToolChoice,
    ToolChoiceMode, ToolResultPart, UnknownEvent, UnknownPart, Usage,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::custom_input::{CustomStream, custom_input_text};
use crate::error::CodecError;
use crate::jsonutil::{
    Object, expect_array, expect_object, insert_f64, insert_u64, parse_object, require_string,
    take_bool, take_f64, take_object, take_string, take_u64, to_bytes,
};
use crate::openai_chat::openai_error_body;
use crate::sse::{self, SseFrame, SseParser};
use crate::{Codec, StreamDecoder, StreamEncoder as StreamEncoderTrait};

const SEALED_ITEM: &str = "sealed_item";
const RAW_TOOLS: &str = "hofund_responses_tools";

/// Request-extension key whose array names tools this codec did not map.
///
/// The provider logs them when the upstream protocol is different, then the
/// extension bag is dropped. Same-protocol encode skips the key.
pub const UNMAPPED_TOOLS: &str = "hofund_unmapped_tools";

/// Request-extension object mapping a flattened tool name back to the client's
/// namespace and original name. Same-protocol encode skips the key.
const TOOL_NAMES: &str = "hofund_tool_names";
/// Original tool name when the IR name was flattened for another protocol.
const CALL_NAME: &str = "hofund_call_name";
/// `custom` when the input item was a custom tool call or its output.
const CALL_KIND: &str = "hofund_call_kind";

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
        encode_response(response, &ToolMap::new())
    }

    fn encode_response_for(
        &self,
        request: &Request,
        response: &Response,
    ) -> Result<Vec<u8>, CodecError> {
        encode_response(response, &tool_map(&request.extensions))
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
        Box::new(Encoder {
            inner: StreamEncoder::with_tools(tool_map(&request.extensions)),
        })
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
    let choice_value = object.remove("tool_choice");
    let mut raw_tools = object.remove("tools");
    if let Some(raw) = raw_tools.as_mut()
        && let Some((names, web)) = allowed_spec(choice_value.as_ref())?
    {
        apply_allowed_filter(raw, &names, web);
    }
    let (tools, dropped, named) = decode_tools(raw_tools.clone())?;
    let (tool_choice, tools) = decode_tool_choice(choice_value, tools)?;
    let max_tokens = take_u64(&mut object, "max_output_tokens")?;
    let temperature = take_f64(&mut object, "temperature")?;
    let top_p = take_f64(&mut object, "top_p")?;
    let stream = take_bool(&mut object, "stream")?.unwrap_or(false);
    let effort = object
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .and_then(Effort::from_label);
    let mut extensions = Extensions::from_map(object);
    if let Some(raw) = raw_tools
        && needs_raw_tools(&raw)
    {
        extensions.insert(RAW_TOOLS, raw);
    }
    if !dropped.is_empty() {
        extensions.insert(
            UNMAPPED_TOOLS,
            Value::Array(dropped.into_iter().map(Value::String).collect()),
        );
    }
    if !named.is_empty() {
        extensions.insert(TOOL_NAMES, Value::Object(named));
    }
    Ok(Request {
        model,
        messages,
        tools,
        tool_choice,
        params: GenerationParams {
            max_tokens,
            temperature,
            top_p,
            effort,
            ..GenerationParams::default()
        },
        stream,
        extensions,
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
        "custom_tool_call" => Ok(Some(decode_custom_call(object)?)),
        "custom_tool_call_output" => Ok(Some(decode_custom_output(object)?)),
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
    let namespace = take_string(&mut object, "namespace")?.filter(|value| !value.is_empty());
    let _ = object.remove("status");
    remember_call(&mut object, &name, namespace.as_deref(), None);
    Ok(tool_call_message(
        id,
        offered_name(&name, namespace.as_deref()),
        ToolArguments::from_text(&arguments),
        object,
    ))
}

fn decode_custom_call(mut object: Object) -> Result<Message, CodecError> {
    let id = take_string(&mut object, "call_id")?.unwrap_or_default();
    let name = take_string(&mut object, "name")?.unwrap_or_default();
    let input = take_string(&mut object, "input")?.unwrap_or_default();
    let namespace = take_string(&mut object, "namespace")?.filter(|value| !value.is_empty());
    let _ = object.remove("status");
    remember_call(&mut object, &name, namespace.as_deref(), Some("custom"));
    Ok(tool_call_message(
        id,
        offered_name(&name, namespace.as_deref()),
        ToolArguments::Json {
            value: serde_json::json!({ "input": input }),
        },
        object,
    ))
}

fn decode_custom_output(mut object: Object) -> Result<Message, CodecError> {
    let id = take_string(&mut object, "call_id")?.unwrap_or_default();
    let content = match object.remove("output") {
        Some(Value::String(text)) => vec![text_part(text)],
        Some(Value::Array(items)) => decode_parts(items)?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(CodecError::WrongType { field: "output" });
        }
    };
    object.insert(CALL_KIND.to_owned(), Value::String("custom".to_owned()));
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

fn remember_call(object: &mut Object, name: &str, namespace: Option<&str>, kind: Option<&str>) {
    if let Some(kind) = kind {
        object.insert(CALL_KIND.to_owned(), Value::String(kind.to_owned()));
    }
    if namespace.is_some() || kind.is_some() {
        object.insert(CALL_NAME.to_owned(), Value::String(name.to_owned()));
    }
    if let Some(namespace) = namespace {
        object.insert("namespace".to_owned(), Value::String(namespace.to_owned()));
    }
}

fn offered_name(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(space) if !space.is_empty() => flat_name(space, name),
        _ => name.to_owned(),
    }
}

fn tool_call_message(
    id: String,
    name: String,
    arguments: ToolArguments,
    object: Object,
) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentPart::ToolCall(ToolCallPart {
            id,
            name,
            arguments,
            extensions: Extensions::from_map(object),
        })],
        extensions: Extensions::new(),
    }
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
    } else if let Some(id) = object.remove("id") {
        extensions.insert("id", id);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    Function,
    Custom,
}

#[derive(Clone)]
struct RestoredTool {
    name: String,
    namespace: String,
    kind: ToolKind,
}

type ToolMap = BTreeMap<String, RestoredTool>;

struct Emit<'a> {
    response: bool,
    tools: &'a ToolMap,
}

fn tool_map(extensions: &Extensions) -> ToolMap {
    let Some(Value::Object(object)) = extensions.get(TOOL_NAMES) else {
        return ToolMap::new();
    };
    let mut map = ToolMap::new();
    for (flat, value) in object {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let kind = match entry.get("kind").and_then(Value::as_str) {
            Some("custom") => ToolKind::Custom,
            _ => ToolKind::Function,
        };
        map.insert(
            flat.clone(),
            RestoredTool {
                name: entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(flat)
                    .to_owned(),
                namespace: entry
                    .get("namespace")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                kind,
            },
        );
    }
    map
}

fn remember(named: &mut Object, offered: &str, name: &str, namespace: &str, kind: &str) {
    if kind == "function" && namespace.is_empty() {
        return;
    }
    named.insert(
        offered.to_owned(),
        serde_json::json!({
            "name": name,
            "namespace": namespace,
            "kind": kind,
        }),
    );
}

fn decode_tools(value: Option<Value>) -> Result<(Vec<Tool>, Vec<String>, Object), CodecError> {
    let Some(value) = value else {
        return Ok((Vec::new(), Vec::new(), Object::new()));
    };
    if value.is_null() {
        return Ok((Vec::new(), Vec::new(), Object::new()));
    }
    let mut tools = Vec::new();
    let mut dropped = Vec::new();
    let mut named = Object::new();
    for item in expect_array(value, "tools")? {
        offer_tool(item, None, &mut tools, &mut dropped, &mut named)?;
    }
    Ok((tools, dropped, named))
}

fn offer_tool(
    value: Value,
    namespace: Option<&str>,
    tools: &mut Vec<Tool>,
    dropped: &mut Vec<String>,
    named: &mut Object,
) -> Result<(), CodecError> {
    let mut object = expect_object(value, "tools")?;
    let kind = take_string(&mut object, "type")?.unwrap_or_else(|| "function".to_owned());
    if kind == "namespace" && namespace.is_none() {
        let space = take_string(&mut object, "name")?.unwrap_or_default();
        let Some(nested) = object.remove("tools") else {
            dropped.push(label(&space, &kind));
            return Ok(());
        };
        for item in expect_array(nested, "tools")? {
            offer_tool(item, Some(space.as_str()), tools, dropped, named)?;
        }
        return Ok(());
    }
    if kind.starts_with("web_search") {
        dropped.push(kind);
        return Ok(());
    }
    let name = take_string(&mut object, "name")?.unwrap_or_default();
    let offered = match namespace {
        Some(space) if !space.is_empty() => flat_name(space, &name),
        _ => name.clone(),
    };
    let client_search = kind == "tool_search"
        && take_string(&mut object, "execution")?.as_deref() == Some("client");
    if kind == "function" || client_search {
        if offered.is_empty() {
            dropped.push(kind);
            return Ok(());
        }
        let description = take_string(&mut object, "description")?;
        let mut schema = object
            .remove("parameters")
            .unwrap_or_else(|| Value::Object(Object::new()));
        unseal(&mut schema);
        remember(named, &offered, &name, namespace.unwrap_or(""), "function");
        tools.push(Tool {
            name: offered,
            description,
            input_schema: schema,
            extensions: Extensions::from_map(object),
        });
        return Ok(());
    }
    if kind == "custom" {
        if offered.is_empty() {
            dropped.push(kind);
            return Ok(());
        }
        let description = take_string(&mut object, "description")?;
        let format = object.remove("format");
        remember(named, &offered, &name, namespace.unwrap_or(""), "custom");
        tools.push(Tool {
            name: offered,
            description: Some(custom_description(description.as_deref(), format.as_ref())),
            input_schema: custom_schema(),
            extensions: Extensions::new(),
        });
        return Ok(());
    }
    dropped.push(label(&name, &kind));
    Ok(())
}

fn needs_raw_tools(raw: &Value) -> bool {
    let Some(items) = raw.as_array() else {
        return !raw.is_null();
    };
    items.iter().any(|item| {
        item.get("type")
            .and_then(Value::as_str)
            .unwrap_or("function")
            != "function"
    })
}

fn label(name: &str, kind: &str) -> String {
    if name.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind}:{name}")
    }
}

fn custom_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": "The tool's free-form input, the whole of it, exactly as it is to be run."
            }
        },
        "required": ["input"],
        "additionalProperties": false
    })
}

fn custom_description(description: Option<&str>, format: Option<&Value>) -> String {
    let mut text = description.unwrap_or("").trim().to_owned();
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(
        "Put the tool's whole free-form input, as it is to be run, in the `input` string; it is passed on as written, so it is not itself JSON.",
    );
    if let Some(Value::Object(format)) = format {
        let definition = format
            .get("definition")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if format.get("type").and_then(Value::as_str) == Some("grammar") && !definition.is_empty() {
            let syntax = format
                .get("syntax")
                .and_then(Value::as_str)
                .filter(|syntax| !syntax.is_empty())
                .unwrap_or("the");
            text.push_str(&format!(
                "\n\nThe input follows this {syntax} grammar:\n{definition}"
            ));
        }
    }
    text
}

fn flat_name(namespace: &str, name: &str) -> String {
    let flat = format!("{namespace}__{name}");
    if flat.len() <= 64 {
        return flat;
    }
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update([0]);
    hasher.update(name.as_bytes());
    let sum = hasher.finalize();
    let hex: String = sum
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let prefix = flat.get(..55).unwrap_or(&flat);
    format!("{prefix}_{hex}")
}

fn unseal(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("encrypted");
            for child in object.values_mut() {
                unseal(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                unseal(item);
            }
        }
        _ => {}
    }
}

fn allowed_spec(choice: Option<&Value>) -> Result<Option<(BTreeSet<String>, bool)>, CodecError> {
    let Some(Value::Object(object)) = choice else {
        return Ok(None);
    };
    if object.get("type").and_then(Value::as_str) != Some("allowed_tools") {
        return Ok(None);
    }
    let tools = object.get("tools").cloned();
    let (names, web) = allowed_names(tools)?;
    Ok(Some((names, web)))
}

fn apply_allowed_filter(raw: &mut Value, names: &BTreeSet<String>, web: bool) {
    let Some(items) = raw.as_array_mut() else {
        return;
    };
    items.retain_mut(|item| keep_raw_tool(item, None, names, web));
}

fn keep_raw_tool(
    item: &mut Value,
    namespace: Option<&str>,
    names: &BTreeSet<String>,
    web: bool,
) -> bool {
    let Some(object) = item.as_object_mut() else {
        return false;
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function")
        .to_owned();
    if kind == "namespace" && namespace.is_none() {
        let space = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let Some(Value::Array(nested)) = object.get_mut("tools") else {
            return false;
        };
        nested.retain_mut(|child| keep_raw_tool(child, Some(space.as_str()), names, web));
        return !nested.is_empty();
    }
    if kind.starts_with("web_search") {
        return web;
    }
    let name = object.get("name").and_then(Value::as_str).unwrap_or("");
    let offered = match namespace {
        Some(space) if !space.is_empty() => flat_name(space, name),
        _ => name.to_owned(),
    };
    names.contains(&offered)
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
            let (allowed, _) = allowed_names(object.remove("tools"))?;
            tools.retain(|tool| allowed.contains(&tool.name));
            Ok((Some(choice_mode(&mode)?), tools))
        }
        other => Err(CodecError::Unsupported {
            message: format!("tool_choice type `{other}` is not supported"),
        }),
    }
}

fn allowed_names(value: Option<Value>) -> Result<(BTreeSet<String>, bool), CodecError> {
    let Some(value) = value else {
        return Ok((BTreeSet::new(), false));
    };
    let mut names = BTreeSet::new();
    let mut web = false;
    for item in expect_array(value, "tools")? {
        let mut object = expect_object(item, "tools")?;
        let kind = take_string(&mut object, "type")?.unwrap_or_default();
        if kind.starts_with("web_search") {
            web = true;
            continue;
        }
        if !matches!(kind.as_str(), "function" | "custom" | "tool_search") {
            continue;
        }
        let Some(name) = take_string(&mut object, "name")? else {
            continue;
        };
        let namespace = take_string(&mut object, "namespace")?.unwrap_or_default();
        if namespace.is_empty() {
            names.insert(name);
        } else {
            names.insert(flat_name(&namespace, &name));
        }
    }
    Ok((names, web))
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
    let tools = ToolMap::new();
    object.insert(
        "input".to_owned(),
        Value::Array(encode_items(
            messages,
            &Emit {
                response: false,
                tools: &tools,
            },
        )?),
    );
    if let Some(raw) = request.extensions.get(RAW_TOOLS) {
        object.insert("tools".to_owned(), raw.clone());
    } else if !request.tools.is_empty() {
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
    write_responses_effort(&mut object, request);
    merge_public(&request.extensions, &mut object);
    to_bytes(&Value::Object(object))
}

fn write_responses_effort(object: &mut Object, request: &Request) {
    let Some(effort) = request.params.effort else {
        return;
    };
    if request.extensions.get("reasoning").is_some() {
        return;
    }
    object.insert(
        "reasoning".to_owned(),
        serde_json::json!({ "effort": effort.openai_label() }),
    );
}

fn merge_public(extensions: &Extensions, object: &mut Object) {
    for (key, value) in extensions.iter() {
        if key.starts_with("hofund_") {
            continue;
        }
        object.entry(key.clone()).or_insert_with(|| value.clone());
    }
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

fn encode_items(messages: &[Message], emit: &Emit<'_>) -> Result<Vec<Value>, CodecError> {
    let mut items = Vec::new();
    for message in messages {
        push_message_items(message, &mut items, emit)?;
    }
    Ok(items)
}

fn push_message_items(
    message: &Message,
    items: &mut Vec<Value>,
    emit: &Emit<'_>,
) -> Result<(), CodecError> {
    let mut content = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text(text) => content.push(text_item(message.role, text)),
            ContentPart::Image(image) => content.push(image_item(image)),
            ContentPart::Unknown(unknown) => content.push(unknown_item(unknown)),
            ContentPart::Thinking(thinking) => {
                flush_content(message, &mut content, items, emit);
                if let Some(item) = thinking_item(thinking, emit) {
                    items.push(item);
                }
            }
            ContentPart::ToolCall(call) => {
                flush_content(message, &mut content, items, emit);
                items.push(function_call_item(call, emit));
            }
            ContentPart::ToolResult(result) => {
                flush_content(message, &mut content, items, emit);
                items.push(function_output_item(result));
            }
        }
    }
    flush_content(message, &mut content, items, emit);
    Ok(())
}

fn flush_content(
    message: &Message,
    content: &mut Vec<Value>,
    items: &mut Vec<Value>,
    emit: &Emit<'_>,
) {
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
    if emit.response {
        put_identity(&mut item, &message.extensions, "msg");
    }
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

fn thinking_item(part: &ThinkingPart, emit: &Emit<'_>) -> Option<Value> {
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
    if emit.response {
        put_identity(&mut item, &part.extensions, "rs");
    }
    merge_except(&part.extensions, &mut item, SEALED_ITEM);
    Some(Value::Object(item))
}

fn function_call_item(call: &ToolCallPart, emit: &Emit<'_>) -> Value {
    if call.extensions.get(CALL_KIND).and_then(Value::as_str) == Some("custom") {
        return custom_history_item(call, emit);
    }
    if emit.response
        && let Some(restored) = emit.tools.get(&call.name)
    {
        return restored_call(call, restored);
    }
    let mut item = Object::new();
    item.insert("type".to_owned(), Value::String("function_call".to_owned()));
    item.insert("call_id".to_owned(), Value::String(call.id.clone()));
    item.insert("name".to_owned(), Value::String(original_call_name(call)));
    item.insert(
        "arguments".to_owned(),
        Value::String(call.arguments.to_text()),
    );
    if emit.response {
        put_identity(&mut item, &call.extensions, "fc");
    }
    merge_public(&call.extensions, &mut item);
    Value::Object(item)
}

fn custom_history_item(call: &ToolCallPart, emit: &Emit<'_>) -> Value {
    let mut item = Object::new();
    item.insert(
        "type".to_owned(),
        Value::String("custom_tool_call".to_owned()),
    );
    item.insert("call_id".to_owned(), Value::String(call.id.clone()));
    item.insert("name".to_owned(), Value::String(original_call_name(call)));
    item.insert(
        "input".to_owned(),
        Value::String(custom_input_text(&call.arguments.to_text())),
    );
    if emit.response {
        put_identity(&mut item, &call.extensions, "ctc");
    }
    merge_public(&call.extensions, &mut item);
    Value::Object(item)
}

fn original_call_name(call: &ToolCallPart) -> String {
    call.extensions
        .get(CALL_NAME)
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(call.name.as_str())
        .to_owned()
}

fn restored_call(call: &ToolCallPart, restored: &RestoredTool) -> Value {
    let mut item = Object::new();
    let prefix = if restored.kind == ToolKind::Custom {
        "ctc"
    } else {
        "fc"
    };
    put_identity(&mut item, &call.extensions, prefix);
    item.insert("call_id".to_owned(), Value::String(call.id.clone()));
    item.insert("name".to_owned(), Value::String(restored.name.clone()));
    if !restored.namespace.is_empty() {
        item.insert(
            "namespace".to_owned(),
            Value::String(restored.namespace.clone()),
        );
    }
    match restored.kind {
        ToolKind::Custom => {
            item.insert(
                "type".to_owned(),
                Value::String("custom_tool_call".to_owned()),
            );
            item.insert(
                "input".to_owned(),
                Value::String(custom_input_text(&call.arguments.to_text())),
            );
        }
        ToolKind::Function => {
            item.insert("type".to_owned(), Value::String("function_call".to_owned()));
            item.insert(
                "arguments".to_owned(),
                Value::String(call.arguments.to_text()),
            );
        }
    }
    call.extensions.merge_into(&mut item);
    Value::Object(item)
}

fn put_identity(item: &mut Object, extensions: &Extensions, prefix: &str) {
    if !item.contains_key("id") {
        let id = extensions
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| fresh_id(prefix));
        item.insert("id".to_owned(), Value::String(id));
    }
    if !item.contains_key("status") && extensions.get("status").is_none() {
        item.insert("status".to_owned(), Value::String("completed".to_owned()));
    }
}

fn fresh_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{nanos:x}{n:x}")
}

fn function_output_item(result: &ToolResultPart) -> Value {
    let custom = result.extensions.get(CALL_KIND).and_then(Value::as_str) == Some("custom");
    let mut item = Object::new();
    item.insert(
        "type".to_owned(),
        Value::String(
            if custom {
                "custom_tool_call_output"
            } else {
                "function_call_output"
            }
            .to_owned(),
        ),
    );
    item.insert("call_id".to_owned(), Value::String(result.id.clone()));
    item.insert("output".to_owned(), encode_tool_output(&result.content));
    merge_public(&result.extensions, &mut item);
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

fn encode_response(response: &Response, tools: &ToolMap) -> Result<Vec<u8>, CodecError> {
    if response.message.role != Role::Assistant {
        return Err(CodecError::Unsupported {
            message: "a response message must use the assistant role".to_owned(),
        });
    }
    let mut object = Object::new();
    object.insert("object".to_owned(), Value::String("response".to_owned()));
    object.insert(
        "id".to_owned(),
        Value::String(responses_id(response.id.as_deref())),
    );
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
        Value::Array(encode_items(
            std::slice::from_ref(&response.message),
            &Emit {
                response: true,
                tools,
            },
        )?),
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
    let (input, output, total) = usage.filled_counts();
    let mut object = Object::new();
    object.insert("input_tokens".to_owned(), Value::from(input));
    object.insert("output_tokens".to_owned(), Value::from(output));
    object.insert("total_tokens".to_owned(), Value::from(total));
    object.insert(
        "input_tokens_details".to_owned(),
        serde_json::json!({
            "cached_tokens": usage.cache_read_tokens.unwrap_or(0),
        }),
    );
    object.insert(
        "output_tokens_details".to_owned(),
        serde_json::json!({
            "reasoning_tokens": usage.reasoning_tokens.unwrap_or(0),
        }),
    );
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
    output_index: u32,
    restored: Option<RestoredTool>,
    custom_input: CustomStream,
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
    next_output: u32,
    completed: bool,
    tools: ToolMap,
}

#[derive(Clone, Copy)]
enum ItemKind {
    Message,
    Reasoning,
    Function,
}

impl StreamEncoder {
    fn with_tools(tools: ToolMap) -> Self {
        Self {
            tools,
            ..Self::default()
        }
    }

    fn push(&mut self, event: &StreamEvent) -> Result<(), CodecError> {
        match event {
            StreamEvent::MessageStart(start) => {
                self.id.clone_from(&start.id);
                self.model.clone_from(&start.model);
                Usage::merge_patch(&mut self.usage, start.usage.as_ref());
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
                self.ensure(delta.index, ItemKind::Message, &Extensions::new());
                if let Some(item) = self.open.get_mut(&delta.index) {
                    item.text.push_str(&delta.text);
                }
                let output_index = self.output_index(delta.index);
                self.write(
                    "response.output_text.delta",
                    serde_json::json!({
                        "type": "response.output_text.delta",
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": delta.text,
                    }),
                );
            }
            StreamEvent::ThinkingDelta(delta) => {
                self.ensure(delta.index, ItemKind::Reasoning, &Extensions::new());
                if let Some(item) = self.open.get_mut(&delta.index) {
                    item.text.push_str(&delta.text);
                }
                let output_index = self.output_index(delta.index);
                self.write(
                    "response.reasoning_text.delta",
                    serde_json::json!({
                        "type": "response.reasoning_text.delta",
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": delta.text,
                    }),
                );
            }
            StreamEvent::ToolArgumentsDelta(delta) => {
                self.ensure(delta.index, ItemKind::Function, &Extensions::new());
                let output_index = self.output_index(delta.index);
                let custom = self.open.get(&delta.index).is_some_and(|item| {
                    item.restored
                        .as_ref()
                        .is_some_and(|tool| tool.kind == ToolKind::Custom)
                });
                if custom {
                    let piece = self
                        .open
                        .get_mut(&delta.index)
                        .map(|item| {
                            item.text.push_str(&delta.partial_json);
                            item.custom_input.feed(&delta.partial_json)
                        })
                        .unwrap_or_default();
                    if !piece.is_empty() {
                        self.write(
                            "response.custom_tool_call_input.delta",
                            serde_json::json!({
                                "type": "response.custom_tool_call_input.delta",
                                "output_index": output_index,
                                "delta": piece,
                            }),
                        );
                    }
                } else {
                    if let Some(item) = self.open.get_mut(&delta.index) {
                        item.text.push_str(&delta.partial_json);
                    }
                    self.write(
                        "response.function_call_arguments.delta",
                        serde_json::json!({
                            "type": "response.function_call_arguments.delta",
                            "output_index": output_index,
                            "delta": delta.partial_json,
                        }),
                    );
                }
            }
            StreamEvent::SignatureDelta(_) => {}
            StreamEvent::ContentBlockStop(stop) => self.close(stop.index, &stop.extensions),
            StreamEvent::MessageDelta(delta) => {
                if delta.stop.is_some() {
                    self.stop.clone_from(&delta.stop);
                }
                Usage::merge_patch(&mut self.usage, delta.usage.as_ref());
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

    fn ensure(&mut self, index: u32, kind: ItemKind, extensions: &Extensions) {
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
        self.write_open(index, &block, extensions);
    }

    fn output_index(&self, index: u32) -> u32 {
        self.open
            .get(&index)
            .map(|item| item.output_index)
            .unwrap_or(0)
    }

    fn write_open(&mut self, index: u32, block: &BlockStart, extensions: &Extensions) {
        if self.open.contains_key(&index) {
            return;
        }
        let restored = match block {
            BlockStart::ToolCall { name, .. } => self.tools.get(name).cloned(),
            _ => None,
        };
        let prefix = match (&restored, block) {
            (Some(tool), _) if tool.kind == ToolKind::Custom => "ctc",
            (_, BlockStart::Text) => "msg",
            (_, BlockStart::Thinking { .. }) => "rs",
            (_, BlockStart::ToolCall { .. }) => "fc",
            (_, BlockStart::Unknown { .. }) => return,
        };
        let id = extensions
            .get("item_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| fresh_id(prefix));
        let output_index = self.next_output;
        self.next_output += 1;
        let Some(mut state) = ItemState::from_block(block, id, output_index, restored) else {
            return;
        };
        if let Some(tool) = state.restored.clone() {
            state.name = tool.name;
        }
        let kind = state.kind;
        let item = state.added_item(extensions);
        self.open.insert(index, state);
        self.write(
            "response.output_item.added",
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item,
            }),
        );
        if matches!(kind, ItemKind::Message) {
            self.write(
                "response.content_part.added",
                serde_json::json!({
                    "type": "response.content_part.added",
                    "output_index": output_index,
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
        let output_index = state.output_index;
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
                        "output_index": output_index,
                        "content_index": 0,
                        "text": text,
                    }),
                );
                self.write(
                    "response.content_part.done",
                    serde_json::json!({
                        "type": "response.content_part.done",
                        "output_index": output_index,
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
            ItemKind::Function => self.finish_call(&state, output_index),
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
        self.done.insert(output_index, item.clone());
        self.write(
            "response.output_item.done",
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item,
            }),
        );
    }

    fn finish_call(&mut self, state: &ItemState, output_index: u32) -> Value {
        let Some(restored) = &state.restored else {
            return serde_json::json!({
                "id": state.id,
                "type": "function_call",
                "status": "completed",
                "call_id": state.call_id,
                "name": state.name,
                "arguments": state.text,
            });
        };
        if restored.kind == ToolKind::Custom {
            let input = custom_input_text(&state.text);
            self.write(
                "response.custom_tool_call_input.done",
                serde_json::json!({
                    "type": "response.custom_tool_call_input.done",
                    "output_index": output_index,
                    "input": input,
                }),
            );
            let mut item = serde_json::json!({
                "id": state.id,
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": state.call_id,
                "name": state.name,
                "input": input,
            });
            if !restored.namespace.is_empty()
                && let Some(object) = item.as_object_mut()
            {
                object.insert(
                    "namespace".to_owned(),
                    Value::String(restored.namespace.clone()),
                );
            }
            return item;
        }
        let mut item = serde_json::json!({
            "id": state.id,
            "type": "function_call",
            "status": "completed",
            "call_id": state.call_id,
            "name": state.name,
            "arguments": state.text,
        });
        if !restored.namespace.is_empty()
            && let Some(object) = item.as_object_mut()
        {
            object.insert(
                "namespace".to_owned(),
                Value::String(restored.namespace.clone()),
            );
        }
        item
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
        response.insert(
            "id".to_owned(),
            Value::String(responses_id(self.id.as_deref())),
        );
        response.insert("status".to_owned(), Value::String(status.to_owned()));
        response.insert("output".to_owned(), Value::Array(Vec::new()));
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
    fn from_block(
        block: &BlockStart,
        id: String,
        output_index: u32,
        restored: Option<RestoredTool>,
    ) -> Option<Self> {
        let state = match block {
            BlockStart::Text => Self {
                kind: ItemKind::Message,
                id,
                call_id: String::new(),
                name: String::new(),
                text: String::new(),
                output_index,
                restored,
                custom_input: CustomStream::default(),
            },
            BlockStart::Thinking { .. } => Self {
                kind: ItemKind::Reasoning,
                id,
                call_id: String::new(),
                name: String::new(),
                text: String::new(),
                output_index,
                restored,
                custom_input: CustomStream::default(),
            },
            BlockStart::ToolCall { id: call_id, name } => Self {
                kind: ItemKind::Function,
                id,
                call_id: call_id.clone(),
                name: name.clone(),
                text: String::new(),
                output_index,
                restored,
                custom_input: CustomStream::default(),
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
            ItemKind::Function => {
                let custom = self
                    .restored
                    .as_ref()
                    .is_some_and(|tool| tool.kind == ToolKind::Custom);
                if custom {
                    let mut item = serde_json::json!({
                        "id": self.id,
                        "type": "custom_tool_call",
                        "call_id": self.call_id,
                        "name": self.name,
                        "input": "",
                        "status": "in_progress",
                    });
                    if let Some(tool) = &self.restored
                        && !tool.namespace.is_empty()
                        && let Some(object) = item.as_object_mut()
                    {
                        object.insert(
                            "namespace".to_owned(),
                            Value::String(tool.namespace.clone()),
                        );
                    }
                    item
                } else {
                    let mut item = serde_json::json!({
                        "id": self.id,
                        "type": "function_call",
                        "call_id": self.call_id,
                        "name": self.name,
                        "arguments": "",
                        "status": "in_progress",
                    });
                    if let Some(tool) = &self.restored
                        && !tool.namespace.is_empty()
                        && let Some(object) = item.as_object_mut()
                    {
                        object.insert(
                            "namespace".to_owned(),
                            Value::String(tool.namespace.clone()),
                        );
                    }
                    item
                }
            }
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
    item_ids: BTreeMap<u32, String>,
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
        self.note_id(index, &item);
        let kind = take_string(&mut item, "type")?.unwrap_or_default();
        match kind.as_str() {
            "message" => {}
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
            self.note_id(index, &item);
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
            extensions: self.carried_id(index),
        }));
    }

    fn open_thinking(&mut self, index: u32, extensions: Extensions, events: &mut Vec<StreamEvent>) {
        if !self.open.insert(index) {
            return;
        }
        let mut extensions = extensions;
        if extensions.get("item_id").is_none()
            && let Some(id) = self.item_ids.get(&index)
        {
            extensions.insert("item_id", Value::String(id.clone()));
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
            extensions: self.carried_id(index),
        }));
    }

    fn note_id(&mut self, index: u32, item: &Object) {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        if id.is_empty() {
            return;
        }
        self.item_ids.entry(index).or_insert_with(|| id.to_owned());
    }

    fn carried_id(&self, index: u32) -> Extensions {
        let mut extensions = Extensions::new();
        if let Some(id) = self.item_ids.get(&index) {
            extensions.insert("item_id", Value::String(id.clone()));
        }
        extensions
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

fn responses_id(id: Option<&str>) -> String {
    match id {
        Some(id) if id.starts_with("resp_") => id.to_owned(),
        Some(id) if !id.is_empty() => format!("resp_{id}"),
        _ => "resp_local".to_owned(),
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
