//! Generative Language API codec (`generateContent` and `streamGenerateContent`).
//!
//! Text is block index 0, reasoning is block index 1, and each function call
//! takes the next index from 2 upward. Frames are `data: <json>` and a blank
//! line. `data: [DONE]` is ignored aside from closing open blocks. See
//! `docs/codecs.md` for lossy fields.
//!
//! Gemini CLI posts these methods at `{origin}/v1beta/models/{model}`. The
//! model is a path segment in that API. This codec also reads and writes
//! `model` and `stream` on the JSON body so a gateway can inject the path
//! model and the stream method, and so a round trip stays stable. An upstream
//! adapter strips those two fields before it posts: the model is the path,
//! and `stream` selects `streamGenerateContent`.

mod plain_schema;

pub use plain_schema::{plain_code_assist_tools, plain_schema};

use std::collections::BTreeMap;

use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Effort, Extensions,
    GenerationParams, ImagePart, ImageSource, Message, MessageDelta, MessageStart, MessageStop,
    Request, Response, Role, SignatureDelta, Stop, StopReason, StreamEvent, TextDelta, TextPart,
    ThinkingDelta, ThinkingPart, Tool, ToolArguments, ToolArgumentsDelta, ToolCallPart, ToolChoice,
    ToolChoiceMode, ToolResultPart, UnknownEvent, UnknownPart, Usage,
};
use serde_json::Value;

use crate::error::CodecError;
use crate::jsonutil::{
    Object, expect_array, expect_object, insert_f64, insert_i64, insert_u64, parse_object,
    require_string, take_bool, take_f64, take_i64, take_object, take_string, take_string_list,
    take_u64, to_bytes,
};
use crate::sse::{self, SseFrame, SseParser};
use crate::{Codec, StreamDecoder, StreamEncoder};

const TEXT_INDEX: u32 = 0;
const THINKING_INDEX: u32 = 1;
const TOOL_INDEX_BASE: u32 = 2;
const THOUGHT_SIGNATURE: &str = "thoughtSignature";

/// Codec for the Generative Language API.
#[derive(Debug, Default, Clone, Copy)]
pub struct GenerativeLanguage;

impl GenerativeLanguage {
    /// Drops `model` and `stream` before an upstream POST.
    ///
    /// The gateway stores the path model and the stream method on the JSON
    /// body. The Generative Language API takes the model as a path segment
    /// and does not define either field on the request body.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when `bytes` is not a JSON object.
    pub fn prepare_upstream_body(bytes: &[u8]) -> Result<Vec<u8>, CodecError> {
        let mut object = parse_object(bytes)?;
        object.remove("model");
        object.remove("stream");
        to_bytes(&Value::Object(object))
    }
}

impl Codec for GenerativeLanguage {
    fn id(&self) -> &'static str {
        "generative-language"
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
        let mut encoder = Encoder::default();
        let mut out = Vec::new();
        for event in events {
            out.extend(encoder.push(event)?);
        }
        Ok(out)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(Decoder::default())
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(Encoder::default())
    }

    fn encode_token_count(&self, tokens: u64, estimated: bool) -> Vec<u8> {
        format!(r#"{{"totalTokens":{tokens},"estimated":{estimated}}}"#).into_bytes()
    }

    fn model_paths(&self) -> &'static [&'static [&'static str]] {
        &[&["modelVersion"], &["model"]]
    }

    fn encode_error(&self, status: u16, message: &str) -> Vec<u8> {
        encode_error(status, message)
    }

    fn encode_stream_error(&self, status: u16, message: &str) -> Vec<u8> {
        crate::sse::data_frame(&self.encode_error(status, message))
    }

    fn encode_stream_failure(&self, message: &str) -> Vec<u8> {
        let body = serde_json::json!({"error": {"code": 502, "message": message, "status": "UNAVAILABLE"}});
        crate::sse::data_frame(body.to_string().as_bytes())
    }

    fn stream_end(&self) -> crate::StreamEnd {
        crate::StreamEnd::Candidate
    }
}

fn decode_request(bytes: &[u8]) -> Result<Request, CodecError> {
    let mut object = parse_object(bytes)?;
    let model = take_string(&mut object, "model")?.unwrap_or_default();
    let stream = take_bool(&mut object, "stream")?.unwrap_or(false);
    let mut messages = Vec::new();
    if let Some(system) = object.remove("systemInstruction") {
        messages.push(decode_system(system)?);
    }
    if let Some(contents) = object.remove("contents") {
        messages.extend(decode_contents(contents)?);
    }
    let decoded_tools = decode_tools(object.remove("tools"))?;
    let tool_choice = match object.remove("toolConfig") {
        Some(value) if !value.is_null() => Some(decode_tool_config(value)?),
        _ => None,
    };
    let params = match take_object(&mut object, "generationConfig")? {
        Some(config) => decode_generation(config)?,
        None => GenerationParams::default(),
    };
    let mut extensions = Extensions::from_map(object);
    if !decoded_tools.extras.is_empty() {
        extensions.insert("toolExtras", Value::Array(decoded_tools.extras));
    }
    if !decoded_tools.wrapper.is_empty() {
        extensions.insert("declarationWrapper", Value::Object(decoded_tools.wrapper));
    }
    Ok(Request::new(model)
        .with_messages(messages)
        .with_tools(decoded_tools.tools)
        .with_tool_choice(tool_choice)
        .with_params(params)
        .with_stream(stream)
        .with_extensions(extensions))
}

fn encode_request(request: &Request) -> Result<Vec<u8>, CodecError> {
    let mut object = Object::new();
    if !request.model.is_empty() {
        object.insert("model".to_owned(), Value::String(request.model.clone()));
    }
    let (system_parts, system_extensions, rest) = hoist_system(&request.messages)?;
    if !system_parts.is_empty() || !system_extensions.is_empty() {
        object.insert(
            "systemInstruction".to_owned(),
            content_value(None, system_parts, &system_extensions)?,
        );
    }
    object.insert("contents".to_owned(), Value::Array(encode_contents(&rest)?));
    if let Some(tools) = encode_tools(request)? {
        object.insert("tools".to_owned(), tools);
    }
    if let Some(choice) = &request.tool_choice {
        object.insert("toolConfig".to_owned(), encode_tool_config(choice));
    }
    if let Some(config) = encode_generation(&request.params)? {
        object.insert("generationConfig".to_owned(), config);
    }
    if request.stream {
        object.insert("stream".to_owned(), Value::Bool(true));
    }
    merge_except(
        &request.extensions,
        &mut object,
        &["toolExtras", "declarationWrapper"],
    );
    to_bytes(&Value::Object(object))
}

fn decode_response(bytes: &[u8]) -> Result<Response, CodecError> {
    let mut object = parse_object(bytes)?;
    let id = take_string(&mut object, "responseId")?;
    let model = match take_string(&mut object, "modelVersion")? {
        Some(model) => Some(model),
        None => take_string(&mut object, "model")?,
    };
    let usage = match take_object(&mut object, "usageMetadata")? {
        Some(usage) => Some(decode_usage(usage)?),
        None => None,
    };
    let candidates = take_candidates(&mut object)?;
    let (chosen, extra) = split_candidates(candidates)?;
    let (message, stop) = match chosen {
        Some(candidate) => decode_chosen(candidate)?,
        None => (Message::new(Role::Assistant), None),
    };
    let mut extensions = Extensions::from_map(object);
    if !extra.is_empty() {
        extensions.insert("extraCandidates", Value::Array(extra));
    }
    Ok(Response::new(message)
        .with_id(id)
        .with_model(model)
        .with_stop(stop)
        .with_usage(usage)
        .with_extensions(extensions))
}

fn encode_response(response: &Response) -> Result<Vec<u8>, CodecError> {
    if response.message.role != Role::Assistant {
        return Err(unsupported("Generative Language responses are model turns"));
    }
    let mut object = Object::new();
    if let Some(id) = &response.id {
        object.insert("responseId".to_owned(), Value::String(id.clone()));
    }
    if let Some(model) = &response.model {
        object.insert("modelVersion".to_owned(), Value::String(model.clone()));
    }
    object.insert(
        "candidates".to_owned(),
        Value::Array(encode_candidates(response)?),
    );
    if let Some(usage) = &response.usage
        && let Some(encoded) = encode_usage(usage)
    {
        object.insert("usageMetadata".to_owned(), encoded);
    }
    merge_except(&response.extensions, &mut object, &["extraCandidates"]);
    to_bytes(&Value::Object(object))
}

fn decode_system(value: Value) -> Result<Message, CodecError> {
    match value {
        Value::String(text) => {
            let mut message = Message::new(Role::System);
            message.content.push(ContentPart::text(text));
            Ok(message)
        }
        Value::Object(mut object) => {
            let _role = take_string(&mut object, "role")?;
            let parts = match object.remove("parts") {
                Some(parts) => decode_parts(parts)?,
                None => Vec::new(),
            };
            Ok(Message {
                role: Role::System,
                content: parts,
                extensions: Extensions::from_map(object),
            })
        }
        _ => Err(CodecError::WrongType {
            field: "systemInstruction",
        }),
    }
}

fn decode_contents(value: Value) -> Result<Vec<Message>, CodecError> {
    let mut messages = Vec::new();
    for item in expect_array(value, "contents")? {
        messages.push(decode_turn(item)?);
    }
    Ok(messages)
}

fn decode_turn(value: Value) -> Result<Message, CodecError> {
    let mut object = expect_object(value, "contents")?;
    let role_text = require_string(&mut object, "role")?;
    let role = match role_text.as_str() {
        "user" => Role::User,
        "model" => Role::Assistant,
        _ => return Err(CodecError::UnknownRole(role_text)),
    };
    let parts = match object.remove("parts") {
        Some(parts) => decode_parts(parts)?,
        None => return Err(CodecError::MissingField("parts")),
    };
    Ok(Message {
        role,
        content: parts,
        extensions: Extensions::from_map(object),
    })
}

fn hoist_system(
    messages: &[Message],
) -> Result<(Vec<Value>, Extensions, Vec<&Message>), CodecError> {
    let mut parts = Vec::new();
    let mut extensions = Extensions::new();
    let mut rest = Vec::new();
    for message in messages {
        match message.role {
            Role::System | Role::Developer => {
                parts.extend(encode_parts(&message.content)?);
                extensions.absorb(&message.extensions);
            }
            Role::User | Role::Assistant | Role::Tool => rest.push(message),
        }
    }
    Ok((parts, extensions, rest))
}

fn encode_contents(messages: &[&Message]) -> Result<Vec<Value>, CodecError> {
    let mut contents = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        if messages[index].role == Role::Tool {
            let mut parts = Vec::new();
            let mut extensions = Extensions::new();
            while index < messages.len() && messages[index].role == Role::Tool {
                parts.extend(encode_parts(&messages[index].content)?);
                extensions.absorb(&messages[index].extensions);
                index += 1;
            }
            contents.push(content_value(Some("user"), parts, &extensions)?);
            continue;
        }
        let message = messages[index];
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "model",
            Role::System | Role::Developer | Role::Tool => "user",
        };
        contents.push(content_value(
            Some(role),
            encode_parts(&message.content)?,
            &message.extensions,
        )?);
        index += 1;
    }
    Ok(contents)
}

fn content_value(
    role: Option<&str>,
    parts: Vec<Value>,
    extensions: &Extensions,
) -> Result<Value, CodecError> {
    let mut object = Object::new();
    if let Some(role) = role {
        object.insert("role".to_owned(), Value::String(role.to_owned()));
    }
    object.insert("parts".to_owned(), Value::Array(parts));
    extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn decode_parts(value: Value) -> Result<Vec<ContentPart>, CodecError> {
    expect_array(value, "parts")?
        .into_iter()
        .map(decode_part)
        .collect()
}

fn decode_part(value: Value) -> Result<ContentPart, CodecError> {
    let mut part = expect_object(value, "parts")?;
    if let Some(call) = take_object(&mut part, "functionCall")? {
        return decode_function_call(call, part);
    }
    if let Some(response) = take_object(&mut part, "functionResponse")? {
        return decode_function_response(response, part);
    }
    if let Some(inline) = take_object(&mut part, "inlineData")? {
        return decode_inline(inline, part);
    }
    if let Some(file) = take_object(&mut part, "fileData")? {
        return decode_file(file, part);
    }
    let thought = take_bool(&mut part, "thought")?.unwrap_or(false);
    let signature = take_string(&mut part, THOUGHT_SIGNATURE)?;
    if thought {
        let text = take_string(&mut part, "text")?.unwrap_or_default();
        return Ok(ContentPart::Thinking(ThinkingPart {
            text,
            signature,
            redacted: false,
            data: None,
            extensions: Extensions::from_map(part),
        }));
    }
    if let Some(text) = take_string(&mut part, "text")? {
        let mut extensions = Extensions::from_map(part);
        if let Some(signature) = signature {
            extensions.insert(THOUGHT_SIGNATURE, Value::String(signature));
        }
        return Ok(ContentPart::Text(TextPart { text, extensions }));
    }
    if let Some(signature) = signature {
        return Ok(ContentPart::Thinking(ThinkingPart {
            text: String::new(),
            signature: Some(signature),
            redacted: false,
            data: None,
            extensions: Extensions::from_map(part),
        }));
    }
    Ok(ContentPart::Unknown(UnknownPart {
        kind: "part".to_owned(),
        extensions: Extensions::from_map(part),
    }))
}

fn decode_function_call(mut call: Object, part: Object) -> Result<ContentPart, CodecError> {
    let name = require_string(&mut call, "name")?;
    let id = take_string(&mut call, "id")?.unwrap_or_default();
    let args = match take_object(&mut call, "args")? {
        Some(args) => Value::Object(args),
        None => Value::Object(Object::new()),
    };
    let mut extensions = Extensions::from_map(part);
    if !call.is_empty() {
        extensions.insert("functionFields", Value::Object(call));
    }
    Ok(ContentPart::ToolCall(ToolCallPart {
        id,
        name,
        arguments: ToolArguments::Json { value: args },
        extensions,
    }))
}

fn decode_function_response(mut response: Object, part: Object) -> Result<ContentPart, CodecError> {
    let name = take_string(&mut response, "name")?;
    let id = take_string(&mut response, "id")?.unwrap_or_default();
    let body = match take_object(&mut response, "response")? {
        Some(body) => Value::Object(body),
        None => Value::Object(Object::new()),
    };
    let text = serde_json::to_string(&body)?;
    let mut extensions = Extensions::from_map(part);
    if let Some(name) = name {
        extensions.insert("name", Value::String(name));
    }
    if !response.is_empty() {
        extensions.insert("functionFields", Value::Object(response));
    }
    Ok(ContentPart::ToolResult(ToolResultPart {
        id,
        is_error: false,
        content: vec![ContentPart::text(text)],
        extensions,
    }))
}

fn decode_inline(mut inline: Object, part: Object) -> Result<ContentPart, CodecError> {
    let media_type = require_string(&mut inline, "mimeType")?;
    let data = require_string(&mut inline, "data")?;
    let mut extensions = Extensions::from_map(inline);
    extensions.absorb(&Extensions::from_map(part));
    Ok(ContentPart::Image(ImagePart {
        source: ImageSource::Base64 { media_type, data },
        detail: None,
        extensions,
    }))
}

fn decode_file(mut file: Object, part: Object) -> Result<ContentPart, CodecError> {
    let url = require_string(&mut file, "fileUri")?;
    let mut extensions = Extensions::new();
    if let Some(mime) = take_string(&mut file, "mimeType")? {
        extensions.insert("mimeType", Value::String(mime));
    }
    extensions.absorb(&Extensions::from_map(file));
    extensions.absorb(&Extensions::from_map(part));
    Ok(ContentPart::Image(ImagePart {
        source: ImageSource::Url { url },
        detail: None,
        extensions,
    }))
}

fn encode_parts(parts: &[ContentPart]) -> Result<Vec<Value>, CodecError> {
    parts.iter().map(encode_part).collect()
}

fn encode_part(part: &ContentPart) -> Result<Value, CodecError> {
    let object = match part {
        ContentPart::Text(part) => {
            let mut object = Object::new();
            object.insert("text".to_owned(), Value::String(part.text.clone()));
            part.extensions.merge_into(&mut object);
            object
        }
        ContentPart::Thinking(part) => encode_thinking(part)?,
        ContentPart::ToolCall(part) => encode_tool_call(part)?,
        ContentPart::ToolResult(part) => encode_tool_result(part)?,
        ContentPart::Image(part) => encode_image(part),
        ContentPart::Unknown(part) => {
            let mut object = Object::new();
            part.extensions.merge_into(&mut object);
            object
        }
    };
    Ok(Value::Object(object))
}

fn encode_thinking(part: &ThinkingPart) -> Result<Object, CodecError> {
    let mut object = Object::new();
    object.insert("thought".to_owned(), Value::Bool(true));
    if !part.text.is_empty() {
        object.insert("text".to_owned(), Value::String(part.text.clone()));
    }
    if let Some(signature) = &part.signature
        && !signature.is_empty()
    {
        object.insert(
            THOUGHT_SIGNATURE.to_owned(),
            Value::String(signature.clone()),
        );
    }
    merge_except(
        &part.extensions,
        &mut object,
        &["thought", "text", THOUGHT_SIGNATURE],
    );
    Ok(object)
}

fn encode_tool_call(part: &ToolCallPart) -> Result<Object, CodecError> {
    let mut function = Object::new();
    function.insert("name".to_owned(), Value::String(part.name.clone()));
    function.insert("args".to_owned(), call_args(&part.arguments)?);
    if !part.id.is_empty() {
        function.insert("id".to_owned(), Value::String(part.id.clone()));
    }
    if let Some(Value::Object(fields)) = part.extensions.get("functionFields") {
        for (key, value) in fields {
            function.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    let mut object = Object::new();
    object.insert("functionCall".to_owned(), Value::Object(function));
    merge_except(&part.extensions, &mut object, &["functionFields"]);
    Ok(object)
}

fn call_args(arguments: &ToolArguments) -> Result<Value, CodecError> {
    match arguments {
        ToolArguments::Json {
            value: Value::Object(object),
        } => Ok(Value::Object(object.clone())),
        ToolArguments::Json { .. } => {
            Err(unsupported("function call arguments must be a JSON object"))
        }
        ToolArguments::Raw { text } if text.is_empty() => Ok(Value::Object(Object::new())),
        ToolArguments::Raw { text } => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => Ok(Value::Object(object)),
            _ => Err(unsupported("function call arguments must be a JSON object")),
        },
    }
}

fn encode_tool_result(part: &ToolResultPart) -> Result<Object, CodecError> {
    let mut function = Object::new();
    if let Some(name) = part.extensions.get("name").and_then(Value::as_str) {
        function.insert("name".to_owned(), Value::String(name.to_owned()));
    }
    if !part.id.is_empty() {
        function.insert("id".to_owned(), Value::String(part.id.clone()));
    }
    function.insert("response".to_owned(), tool_response_body(part)?);
    if let Some(Value::Object(fields)) = part.extensions.get("functionFields") {
        for (key, value) in fields {
            function.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    let mut object = Object::new();
    object.insert("functionResponse".to_owned(), Value::Object(function));
    merge_except(&part.extensions, &mut object, &["name", "functionFields"]);
    Ok(object)
}

fn tool_response_body(part: &ToolResultPart) -> Result<Value, CodecError> {
    if part.content.is_empty() {
        return Ok(Value::Object(Object::new()));
    }
    let mut text = String::new();
    for content in &part.content {
        match content {
            ContentPart::Text(text_part) => text.push_str(&text_part.text),
            _ => {
                return Err(unsupported("a function response can only carry text"));
            }
        }
    }
    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&text) {
        return Ok(Value::Object(object));
    }
    Ok(serde_json::json!({"result": text}))
}

fn encode_image(part: &ImagePart) -> Object {
    match &part.source {
        ImageSource::Base64 { media_type, data } => {
            let mut inline = Object::new();
            inline.insert("mimeType".to_owned(), Value::String(media_type.clone()));
            inline.insert("data".to_owned(), Value::String(data.clone()));
            let mut object = Object::new();
            object.insert("inlineData".to_owned(), Value::Object(inline));
            part.extensions.merge_into(&mut object);
            object
        }
        ImageSource::Url { url } => {
            let mut file = Object::new();
            file.insert("fileUri".to_owned(), Value::String(url.clone()));
            if let Some(mime) = part.extensions.get("mimeType") {
                file.insert("mimeType".to_owned(), mime.clone());
            }
            let mut object = Object::new();
            object.insert("fileData".to_owned(), Value::Object(file));
            merge_except(&part.extensions, &mut object, &["mimeType"]);
            object
        }
    }
}

struct DecodedTools {
    tools: Vec<Tool>,
    extras: Vec<Value>,
    wrapper: Object,
}

fn decode_tools(value: Option<Value>) -> Result<DecodedTools, CodecError> {
    let Some(value) = value else {
        return Ok(DecodedTools {
            tools: Vec::new(),
            extras: Vec::new(),
            wrapper: Object::new(),
        });
    };
    if value.is_null() {
        return Ok(DecodedTools {
            tools: Vec::new(),
            extras: Vec::new(),
            wrapper: Object::new(),
        });
    }
    let mut tools = Vec::new();
    let mut extras = Vec::new();
    let mut wrapper = Object::new();
    let mut wrapped = false;
    for item in expect_array(value, "tools")? {
        let mut object = expect_object(item, "tools")?;
        if let Some(declarations) = object.remove("functionDeclarations") {
            if !wrapped && !object.is_empty() {
                wrapper = object;
                wrapped = true;
            } else if wrapped && !object.is_empty() {
                extras.push(Value::Object(object));
            }
            for declaration in expect_array(declarations, "functionDeclarations")? {
                tools.push(decode_declaration(declaration)?);
            }
        } else if !object.is_empty() {
            extras.push(Value::Object(object));
        }
    }
    Ok(DecodedTools {
        tools,
        extras,
        wrapper,
    })
}

fn decode_declaration(value: Value) -> Result<Tool, CodecError> {
    let mut object = expect_object(value, "functionDeclarations")?;
    let name = require_string(&mut object, "name")?;
    let description = take_string(&mut object, "description")?;
    let input_schema = match object.remove("parameters") {
        Some(schema) => schema,
        None => Value::Object(Object::new()),
    };
    Ok(Tool {
        name,
        description,
        input_schema,
        extensions: Extensions::from_map(object),
    })
}

fn encode_tools(request: &Request) -> Result<Option<Value>, CodecError> {
    let wrapper = match request.extensions.get("declarationWrapper") {
        Some(Value::Object(object)) => object.clone(),
        _ => Object::new(),
    };
    let extras = match request.extensions.get("toolExtras") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    if request.tools.is_empty() && wrapper.is_empty() && extras.is_empty() {
        return Ok(None);
    }
    let mut tools = Vec::new();
    if !request.tools.is_empty() || !wrapper.is_empty() {
        let mut object = wrapper;
        if !request.tools.is_empty() {
            let mut declarations = Vec::new();
            for tool in &request.tools {
                declarations.push(encode_declaration(tool)?);
            }
            object.insert(
                "functionDeclarations".to_owned(),
                Value::Array(declarations),
            );
        }
        tools.push(Value::Object(object));
    }
    tools.extend(extras);
    Ok(Some(Value::Array(tools)))
}

fn encode_declaration(tool: &Tool) -> Result<Value, CodecError> {
    let mut object = Object::new();
    object.insert("name".to_owned(), Value::String(tool.name.clone()));
    if let Some(description) = &tool.description {
        object.insert("description".to_owned(), Value::String(description.clone()));
    }
    object.insert("parameters".to_owned(), tool.input_schema.clone());
    tool.extensions.merge_into(&mut object);
    Ok(Value::Object(object))
}

fn decode_tool_config(value: Value) -> Result<ToolChoice, CodecError> {
    let mut object = expect_object(value, "toolConfig")?;
    let function = take_object(&mut object, "functionCallingConfig")?.unwrap_or_default();
    let mut choice = decode_function_config(function)?;
    if !object.is_empty() {
        choice
            .extensions
            .insert("toolConfigFields", Value::Object(object));
    }
    Ok(choice)
}

fn decode_function_config(mut config: Object) -> Result<ToolChoice, CodecError> {
    let mode_text = take_string(&mut config, "mode")?.unwrap_or_else(|| "AUTO".to_owned());
    let names = take_string_list(&mut config, "allowedFunctionNames")?;
    let mode = match mode_text.as_str() {
        "NONE" => ToolChoiceMode::None,
        "AUTO" => ToolChoiceMode::Auto,
        "ANY" if names.len() == 1 => ToolChoiceMode::Named {
            name: names[0].clone(),
        },
        "ANY" => ToolChoiceMode::Required,
        _ => ToolChoiceMode::Required,
    };
    let mut extensions = Extensions::from_map(config);
    if !matches!(mode_text.as_str(), "AUTO" | "NONE" | "ANY") {
        extensions.insert("mode", Value::String(mode_text));
    }
    if !names.is_empty() {
        extensions.insert(
            "allowedFunctionNames",
            Value::Array(names.into_iter().map(Value::String).collect()),
        );
    }
    Ok(ToolChoice { mode, extensions })
}

fn encode_tool_config(choice: &ToolChoice) -> Value {
    let mut function = Object::new();
    function.insert("mode".to_owned(), Value::String(encode_mode(choice)));
    if let Some(names) = choice.extensions.get("allowedFunctionNames") {
        function.insert("allowedFunctionNames".to_owned(), names.clone());
    } else if let ToolChoiceMode::Named { name } = &choice.mode {
        function.insert(
            "allowedFunctionNames".to_owned(),
            Value::Array(vec![Value::String(name.clone())]),
        );
    }
    merge_except(
        &choice.extensions,
        &mut function,
        &["mode", "allowedFunctionNames", "toolConfigFields"],
    );
    let mut object = Object::new();
    object.insert("functionCallingConfig".to_owned(), Value::Object(function));
    if let Some(Value::Object(fields)) = choice.extensions.get("toolConfigFields") {
        for (key, value) in fields {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    Value::Object(object)
}

fn encode_mode(choice: &ToolChoice) -> String {
    if let Some(mode) = choice.extensions.get("mode").and_then(Value::as_str) {
        return mode.to_owned();
    }
    match choice.mode {
        ToolChoiceMode::Auto => "AUTO".to_owned(),
        ToolChoiceMode::None => "NONE".to_owned(),
        ToolChoiceMode::Required | ToolChoiceMode::Named { .. } => "ANY".to_owned(),
    }
}

fn decode_generation(mut object: Object) -> Result<GenerationParams, CodecError> {
    let effort = generative_effort(&object);
    Ok(GenerationParams {
        temperature: take_f64(&mut object, "temperature")?,
        top_p: take_f64(&mut object, "topP")?,
        top_k: take_u64(&mut object, "topK")?,
        max_tokens: take_u64(&mut object, "maxOutputTokens")?,
        stop: take_string_list(&mut object, "stopSequences")?,
        seed: take_i64(&mut object, "seed")?,
        presence_penalty: None,
        frequency_penalty: None,
        effort,
        extensions: Extensions::from_map(object),
    })
}

fn generative_effort(object: &Object) -> Option<Effort> {
    if let Some(level) = object.get("thinking_level").and_then(Value::as_str) {
        return Effort::from_label(level);
    }
    let config = object.get("thinkingConfig").and_then(Value::as_object)?;
    if let Some(level) = config
        .get("thinkingLevel")
        .or_else(|| config.get("thinking_level"))
        .and_then(Value::as_str)
    {
        return Effort::from_label(level);
    }
    let budget = config
        .get("thinkingBudget")
        .and_then(Value::as_i64)
        .or_else(|| config.get("thinking_budget").and_then(Value::as_i64))?;
    Some(match budget {
        ..0 => Effort::Medium,
        0 => Effort::None,
        1..=4096 => Effort::Low,
        4097..=12000 => Effort::Medium,
        12001..=24000 => Effort::High,
        _ => Effort::Xhigh,
    })
}

fn encode_generation(params: &GenerationParams) -> Result<Option<Value>, CodecError> {
    let mut object = Object::new();
    insert_f64(&mut object, "temperature", params.temperature)?;
    insert_f64(&mut object, "topP", params.top_p)?;
    insert_u64(&mut object, "topK", params.top_k);
    insert_u64(&mut object, "maxOutputTokens", params.max_tokens);
    if !params.stop.is_empty() {
        object.insert(
            "stopSequences".to_owned(),
            Value::Array(params.stop.iter().cloned().map(Value::String).collect()),
        );
    }
    insert_i64(&mut object, "seed", params.seed);
    write_generative_effort(&mut object, params);
    params.extensions.merge_into(&mut object);
    if object.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(object)))
    }
}

fn write_generative_effort(object: &mut Object, params: &GenerationParams) {
    let Some(effort) = params.effort else {
        return;
    };
    if params.extensions.get("thinkingConfig").is_some()
        || params.extensions.get("thinking_level").is_some()
    {
        return;
    }
    let value = match effort.generative_level() {
        Some(level) => serde_json::json!({"thinkingLevel": level}),
        None => serde_json::json!({"thinkingBudget": 0}),
    };
    object.insert("thinkingConfig".to_owned(), value);
}

fn decode_usage(mut object: Object) -> Result<Usage, CodecError> {
    Ok(Usage {
        input_tokens: take_u64(&mut object, "promptTokenCount")?,
        output_tokens: take_u64(&mut object, "candidatesTokenCount")?,
        total_tokens: take_u64(&mut object, "totalTokenCount")?,
        cache_read_tokens: take_u64(&mut object, "cachedContentTokenCount")?,
        cache_write_tokens: None,
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        reasoning_tokens: take_u64(&mut object, "thoughtsTokenCount")?,
        extensions: Extensions::from_map(object),
    })
}

fn encode_usage(usage: &Usage) -> Option<Value> {
    let (input, output, total) = usage.filled_counts();
    let mut object = Object::new();
    object.insert("promptTokenCount".to_owned(), Value::from(input));
    object.insert("candidatesTokenCount".to_owned(), Value::from(output));
    object.insert("totalTokenCount".to_owned(), Value::from(total));
    insert_u64(
        &mut object,
        "cachedContentTokenCount",
        usage.cache_read_tokens,
    );
    insert_u64(&mut object, "thoughtsTokenCount", usage.reasoning_tokens);
    usage.extensions.merge_into(&mut object);
    if object.is_empty() {
        None
    } else {
        Some(Value::Object(object))
    }
}

fn decode_finish(reason: &str, has_tool: bool) -> StopReason {
    match reason {
        "STOP" if has_tool => StopReason::ToolUse,
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        "SAFETY" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_owned()),
    }
}

fn encode_finish(reason: &StopReason) -> String {
    match reason {
        StopReason::EndTurn | StopReason::StopSequence | StopReason::ToolUse => "STOP".to_owned(),
        StopReason::MaxTokens => "MAX_TOKENS".to_owned(),
        StopReason::ContentFilter => "SAFETY".to_owned(),
        StopReason::Refusal => "REFUSAL".to_owned(),
        StopReason::Other(token) => token.clone(),
    }
}

fn take_candidates(object: &mut Object) -> Result<Vec<Value>, CodecError> {
    match object.remove("candidates") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(CodecError::WrongType {
            field: "candidates",
        }),
    }
}

fn split_candidates(candidates: Vec<Value>) -> Result<(Option<Object>, Vec<Value>), CodecError> {
    let mut parsed = Vec::new();
    for candidate in candidates {
        parsed.push(expect_object(candidate, "candidates")?);
    }
    if parsed.is_empty() {
        return Ok((None, Vec::new()));
    }
    let chosen_at = parsed
        .iter()
        .position(|candidate| candidate.get("index").and_then(Value::as_u64) == Some(0))
        .unwrap_or(0);
    let mut chosen = None;
    let mut extra = Vec::new();
    for (index, candidate) in parsed.into_iter().enumerate() {
        if index == chosen_at {
            chosen = Some(candidate);
        } else {
            extra.push(Value::Object(candidate));
        }
    }
    Ok((chosen, extra))
}

fn decode_chosen(mut candidate: Object) -> Result<(Message, Option<Stop>), CodecError> {
    let finish = take_string(&mut candidate, "finishReason")?;
    let _index = take_u64(&mut candidate, "index")?;
    let mut content = take_object(&mut candidate, "content")?.unwrap_or_default();
    if let Some(role) = take_string(&mut content, "role")?
        && role != "model"
    {
        return Err(CodecError::UnknownRole(role));
    }
    let parts = match content.remove("parts") {
        Some(parts) => decode_parts(parts)?,
        None => Vec::new(),
    };
    for (key, value) in content {
        candidate.entry(key).or_insert(value);
    }
    let has_tool = parts.iter().any(is_tool_call);
    let stop = finish.map(|reason| Stop {
        reason: decode_finish(&reason, has_tool),
        sequence: None,
    });
    Ok((
        Message {
            role: Role::Assistant,
            content: parts,
            extensions: Extensions::from_map(candidate),
        },
        stop,
    ))
}

fn encode_candidates(response: &Response) -> Result<Vec<Value>, CodecError> {
    let mut candidate = Object::new();
    candidate.insert("index".to_owned(), Value::from(0));
    let parts = encode_parts(&response.message.content)?;
    let mut content = Object::new();
    content.insert("role".to_owned(), Value::String("model".to_owned()));
    content.insert("parts".to_owned(), Value::Array(parts));
    candidate.insert("content".to_owned(), Value::Object(content));
    if let Some(stop) = &response.stop {
        candidate.insert(
            "finishReason".to_owned(),
            Value::String(encode_finish(&stop.reason)),
        );
    }
    response.message.extensions.merge_into(&mut candidate);
    let mut candidates = vec![Value::Object(candidate)];
    if let Some(Value::Array(extra)) = response.extensions.get("extraCandidates") {
        candidates.extend(extra.clone());
    }
    Ok(candidates)
}

fn is_tool_call(part: &ContentPart) -> bool {
    matches!(part, ContentPart::ToolCall(_))
}

fn is_generate_chunk(object: &Object) -> bool {
    [
        "candidates",
        "usageMetadata",
        "responseId",
        "modelVersion",
        "model",
        "promptFeedback",
        "extraCandidates",
    ]
    .iter()
    .any(|key| object.contains_key(*key))
}

fn encode_error(status: u16, message: &str) -> Vec<u8> {
    let name = match status {
        400 | 413 => "INVALID_ARGUMENT",
        401 => "UNAUTHENTICATED",
        403 => "PERMISSION_DENIED",
        404 => "NOT_FOUND",
        429 => "RESOURCE_EXHAUSTED",
        503 => "UNAVAILABLE",
        _ => "INTERNAL",
    };
    serde_json::to_vec(&serde_json::json!({
        "error": { "code": status, "message": message, "status": name }
    }))
    .unwrap_or_else(|_| {
        b"{\"error\":{\"code\":500,\"message\":\"request failed\",\"status\":\"INTERNAL\"}}"
            .to_vec()
    })
}

fn unsupported(message: impl Into<String>) -> CodecError {
    CodecError::Unsupported {
        message: message.into(),
    }
}

fn merge_except(extensions: &Extensions, object: &mut Object, skip: &[&str]) {
    for (key, value) in extensions.iter() {
        if skip.iter().any(|name| *name == key) {
            continue;
        }
        object.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn block_start(index: u32, block: BlockStart) -> StreamEvent {
    StreamEvent::ContentBlockStart(ContentBlockStart {
        index,
        block,
        extensions: Extensions::new(),
    })
}

fn block_stop(index: u32) -> StreamEvent {
    StreamEvent::ContentBlockStop(ContentBlockStop {
        index,
        extensions: Extensions::new(),
    })
}

#[derive(Debug, Default)]
struct Decoder {
    parser: SseParser,
    started: bool,
    stopped: bool,
    saw_tool: bool,
    text_open: bool,
    thinking_open: bool,
    next_tool: u32,
}

impl StreamDecoder for Decoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.push(chunk)?;
        self.frames(frames)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.parser.finish()?;
        let mut events = self.frames(frames)?;
        events.extend(self.finish_blocks());
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
            return Ok(self.finish_blocks());
        }
        let value: Value = serde_json::from_str(&frame.data)?;
        match value {
            Value::Object(object) if is_generate_chunk(&object) => self.decode_chunk(object),
            other => Ok(vec![StreamEvent::Unknown(UnknownEvent {
                name: "data".to_owned(),
                payload: other,
            })]),
        }
    }

    fn finish_blocks(&mut self) -> Vec<StreamEvent> {
        if self.stopped {
            return Vec::new();
        }
        self.stopped = true;
        let mut events = Vec::new();
        if self.text_open {
            events.push(block_stop(TEXT_INDEX));
            self.text_open = false;
        }
        if self.thinking_open {
            events.push(block_stop(THINKING_INDEX));
            self.thinking_open = false;
        }
        events.push(StreamEvent::MessageStop(MessageStop::default()));
        events
    }

    fn decode_chunk(&mut self, mut object: Object) -> Result<Vec<StreamEvent>, CodecError> {
        let id = take_string(&mut object, "responseId")?;
        let model = match take_string(&mut object, "modelVersion")? {
            Some(model) => Some(model),
            None => take_string(&mut object, "model")?,
        };
        let usage = match take_object(&mut object, "usageMetadata")? {
            Some(usage) => Some(decode_usage(usage)?),
            None => None,
        };
        let candidates = take_candidates(&mut object)?;
        let (chosen, extra) = split_candidates(candidates)?;
        if !extra.is_empty() {
            object.insert("extraCandidates".to_owned(), Value::Array(extra));
        }
        let rest = Extensions::from_map(object);
        let mut events = Vec::new();
        let delta_extensions =
            if !self.started && (id.is_some() || model.is_some() || !rest.is_empty()) {
                events.push(StreamEvent::MessageStart(MessageStart {
                    id,
                    model,
                    usage: None,
                    extensions: rest,
                }));
                self.started = true;
                Extensions::new()
            } else {
                rest
            };
        let mut finish = None;
        let mut candidate_extensions = Extensions::new();
        if let Some(candidate) = chosen {
            let decoded = self.decode_candidate(&mut events, candidate)?;
            finish = decoded.0;
            candidate_extensions = decoded.1;
        }
        let mut delta_extensions = delta_extensions;
        delta_extensions.absorb(&candidate_extensions);
        if finish.is_some() || usage.is_some() || !delta_extensions.is_empty() {
            events.push(StreamEvent::MessageDelta(MessageDelta {
                stop: finish.map(|reason| Stop {
                    reason,
                    sequence: None,
                }),
                usage,
                extensions: delta_extensions,
            }));
        }
        Ok(events)
    }

    fn decode_candidate(
        &mut self,
        events: &mut Vec<StreamEvent>,
        mut candidate: Object,
    ) -> Result<(Option<StopReason>, Extensions), CodecError> {
        let finish = take_string(&mut candidate, "finishReason")?;
        let _index = take_u64(&mut candidate, "index")?;
        let mut content = take_object(&mut candidate, "content")?.unwrap_or_default();
        let _role = take_string(&mut content, "role")?;
        if let Some(parts) = content.remove("parts") {
            for part in expect_array(parts, "parts")? {
                self.push_part(events, part)?;
            }
        }
        for (key, value) in content {
            candidate.entry(key).or_insert(value);
        }
        let finish = finish.map(|reason| decode_finish(&reason, self.saw_tool));
        Ok((finish, Extensions::from_map(candidate)))
    }

    fn push_part(&mut self, events: &mut Vec<StreamEvent>, value: Value) -> Result<(), CodecError> {
        let mut part = expect_object(value, "parts")?;
        if part.contains_key("functionCall") {
            let call = take_object(&mut part, "functionCall")?.unwrap_or_default();
            let signature = take_string(&mut part, THOUGHT_SIGNATURE)?;
            let thought = take_bool(&mut part, "thought")?;
            let decoded = decode_function_call(call, Object::new())?;
            let ContentPart::ToolCall(call) = decoded else {
                return Err(unsupported("function call did not decode"));
            };
            let mut extensions = call.extensions;
            if let Some(signature) = signature {
                extensions.insert(THOUGHT_SIGNATURE, Value::String(signature));
            }
            if thought == Some(true) {
                extensions.insert("thought", Value::Bool(true));
            }
            extensions.absorb(&Extensions::from_map(part));
            let index = self.alloc_tool()?;
            self.saw_tool = true;
            events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
                index,
                block: BlockStart::ToolCall {
                    id: call.id,
                    name: call.name,
                },
                extensions,
            }));
            let arguments = call.arguments.to_text();
            events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index,
                partial_json: arguments,
                extensions: Extensions::new(),
            }));
            events.push(block_stop(index));
            return Ok(());
        }
        let raw = Value::Object(part.clone());
        match decode_part(Value::Object(part))? {
            ContentPart::Text(part) => {
                if part.text.is_empty() && part.extensions.is_empty() {
                    return Ok(());
                }
                if !self.text_open {
                    events.push(block_start(TEXT_INDEX, BlockStart::Text));
                    self.text_open = true;
                }
                events.push(StreamEvent::TextDelta(TextDelta {
                    index: TEXT_INDEX,
                    text: part.text,
                    extensions: part.extensions,
                }));
            }
            ContentPart::Thinking(part) => {
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
                if !part.text.is_empty() || !part.extensions.is_empty() {
                    events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                        index: THINKING_INDEX,
                        text: part.text,
                        extensions: part.extensions,
                    }));
                }
                if let Some(signature) = part.signature {
                    events.push(StreamEvent::SignatureDelta(SignatureDelta {
                        index: THINKING_INDEX,
                        signature,
                        extensions: Extensions::new(),
                    }));
                }
            }
            ContentPart::ToolCall(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Image(_)
            | ContentPart::Unknown(_) => {
                events.push(StreamEvent::Unknown(UnknownEvent {
                    name: "part".to_owned(),
                    payload: raw,
                }));
            }
        }
        Ok(())
    }

    fn alloc_tool(&mut self) -> Result<u32, CodecError> {
        let index = TOOL_INDEX_BASE
            .checked_add(self.next_tool)
            .ok_or_else(|| unsupported("function call index is too large"))?;
        self.next_tool = self
            .next_tool
            .checked_add(1)
            .ok_or_else(|| unsupported("function call index is too large"))?;
        Ok(index)
    }
}

#[derive(Debug, Default)]
struct Encoder {
    calls: BTreeMap<u32, PendingCall>,
    usage: Option<Usage>,
    wrote_usage: bool,
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    extensions: Extensions,
}

impl StreamEncoder for Encoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        let mut out = Vec::new();
        match event {
            StreamEvent::MessageStart(start) => {
                Usage::merge_patch(&mut self.usage, start.usage.as_ref());
                write_chunk(
                    &mut out,
                    Chunk {
                        id: start.id.as_deref(),
                        model: start.model.as_deref(),
                        parts: Vec::new(),
                        finish: None,
                        usage: None,
                        extensions: &start.extensions,
                    },
                )?;
            }
            StreamEvent::ContentBlockStart(start) => self.start_block(start)?,
            StreamEvent::TextDelta(delta) => {
                if delta.text.is_empty() && delta.extensions.is_empty() {
                    return Ok(out);
                }
                expect_index(delta.index, TEXT_INDEX, "text")?;
                let part = encode_part(&ContentPart::Text(TextPart {
                    text: delta.text.clone(),
                    extensions: delta.extensions.clone(),
                }))?;
                write_chunk(
                    &mut out,
                    Chunk {
                        id: None,
                        model: None,
                        parts: vec![part],
                        finish: None,
                        usage: None,
                        extensions: &Extensions::new(),
                    },
                )?;
            }
            StreamEvent::ThinkingDelta(delta) => {
                if delta.text.is_empty() && delta.extensions.is_empty() {
                    return Ok(out);
                }
                expect_index(delta.index, THINKING_INDEX, "thinking")?;
                let part = encode_part(&ContentPart::Thinking(ThinkingPart {
                    text: delta.text.clone(),
                    signature: None,
                    redacted: false,
                    data: None,
                    extensions: delta.extensions.clone(),
                }))?;
                write_chunk(
                    &mut out,
                    Chunk {
                        id: None,
                        model: None,
                        parts: vec![part],
                        finish: None,
                        usage: None,
                        extensions: &Extensions::new(),
                    },
                )?;
            }
            StreamEvent::SignatureDelta(delta) => {
                if delta.signature.is_empty() {
                    return Ok(out);
                }
                expect_index(delta.index, THINKING_INDEX, "thinking")?;
                let part = encode_part(&ContentPart::Thinking(ThinkingPart {
                    text: String::new(),
                    signature: Some(delta.signature.clone()),
                    redacted: false,
                    data: None,
                    extensions: delta.extensions.clone(),
                }))?;
                write_chunk(
                    &mut out,
                    Chunk {
                        id: None,
                        model: None,
                        parts: vec![part],
                        finish: None,
                        usage: None,
                        extensions: &Extensions::new(),
                    },
                )?;
            }
            StreamEvent::ToolArgumentsDelta(delta) => {
                expect_tool_index(delta.index)?;
                let call = self.calls.entry(delta.index).or_default();
                call.arguments.push_str(&delta.partial_json);
                call.extensions.absorb(&delta.extensions);
            }
            StreamEvent::ContentBlockStop(stop) => {
                if let Some(call) = self.calls.remove(&stop.index) {
                    self.write_call(&mut out, call)?;
                }
            }
            StreamEvent::MessageDelta(delta) => {
                self.flush_calls(&mut out)?;
                Usage::merge_patch(&mut self.usage, delta.usage.as_ref());
                let finish = delta.stop.as_ref().map(|stop| encode_finish(&stop.reason));
                write_chunk(
                    &mut out,
                    Chunk {
                        id: None,
                        model: None,
                        parts: Vec::new(),
                        finish: finish.as_deref(),
                        usage: self.usage.as_ref(),
                        extensions: &delta.extensions,
                    },
                )?;
                self.wrote_usage = self.usage.is_some();
            }
            StreamEvent::MessageStop(_) => {
                self.flush_calls(&mut out)?;
                if !self.wrote_usage && self.usage.is_some() {
                    let usage = self.usage.clone();
                    write_chunk(
                        &mut out,
                        Chunk {
                            id: None,
                            model: None,
                            parts: Vec::new(),
                            finish: None,
                            usage: usage.as_ref(),
                            extensions: &Extensions::new(),
                        },
                    )?;
                    self.wrote_usage = true;
                }
            }
            StreamEvent::Ping(_) => {}
            StreamEvent::Unknown(event) => encode_unknown(&mut out, event)?,
        }
        Ok(out)
    }
}

impl Encoder {
    fn start_block(&mut self, start: &ContentBlockStart) -> Result<(), CodecError> {
        if let BlockStart::ToolCall { id, name } = &start.block {
            expect_tool_index(start.index)?;
            let call = self.calls.entry(start.index).or_default();
            call.id.clone_from(id);
            call.name.clone_from(name);
            call.extensions.absorb(&start.extensions);
        }
        Ok(())
    }

    fn flush_calls(&mut self, out: &mut Vec<u8>) -> Result<(), CodecError> {
        let calls = std::mem::take(&mut self.calls);
        for (_, call) in calls {
            self.write_call(out, call)?;
        }
        Ok(())
    }

    fn write_call(&mut self, out: &mut Vec<u8>, call: PendingCall) -> Result<(), CodecError> {
        let arguments = if call.arguments.is_empty() {
            ToolArguments::Json {
                value: Value::Object(Object::new()),
            }
        } else {
            ToolArguments::from_text(&call.arguments)
        };
        let part = encode_part(&ContentPart::ToolCall(ToolCallPart {
            id: call.id,
            name: call.name,
            arguments,
            extensions: call.extensions,
        }))?;
        write_chunk(
            out,
            Chunk {
                id: None,
                model: None,
                parts: vec![part],
                finish: None,
                usage: None,
                extensions: &Extensions::new(),
            },
        )
    }
}

struct Chunk<'a> {
    id: Option<&'a str>,
    model: Option<&'a str>,
    parts: Vec<Value>,
    finish: Option<&'a str>,
    usage: Option<&'a Usage>,
    extensions: &'a Extensions,
}

fn write_chunk(out: &mut Vec<u8>, chunk: Chunk<'_>) -> Result<(), CodecError> {
    let extras = extra_candidates(chunk.extensions);
    let mut object = Object::new();
    if let Some(id) = chunk.id {
        object.insert("responseId".to_owned(), Value::String(id.to_owned()));
    }
    if let Some(model) = chunk.model {
        object.insert("modelVersion".to_owned(), Value::String(model.to_owned()));
    }
    if !chunk.parts.is_empty() || chunk.finish.is_some() || !extras.is_empty() {
        let mut candidate = Object::new();
        candidate.insert("index".to_owned(), Value::from(0));
        if !chunk.parts.is_empty() {
            let mut content = Object::new();
            content.insert("role".to_owned(), Value::String("model".to_owned()));
            content.insert("parts".to_owned(), Value::Array(chunk.parts));
            candidate.insert("content".to_owned(), Value::Object(content));
        }
        if let Some(finish) = chunk.finish {
            candidate.insert("finishReason".to_owned(), Value::String(finish.to_owned()));
        }
        let mut candidates = vec![Value::Object(candidate)];
        candidates.extend(extras);
        object.insert("candidates".to_owned(), Value::Array(candidates));
    }
    if let Some(usage) = chunk.usage
        && let Some(encoded) = encode_usage(usage)
    {
        object.insert("usageMetadata".to_owned(), encoded);
    }
    merge_except(chunk.extensions, &mut object, &["extraCandidates"]);
    if object.is_empty() {
        return Ok(());
    }
    let data = serde_json::to_string(&Value::Object(object))?;
    sse::write_event(out, None, &data);
    Ok(())
}

fn extra_candidates(extensions: &Extensions) -> Vec<Value> {
    match extensions.get("extraCandidates") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

fn encode_unknown(out: &mut Vec<u8>, event: &UnknownEvent) -> Result<(), CodecError> {
    if event.name == "part" {
        return write_chunk(
            out,
            Chunk {
                id: None,
                model: None,
                parts: vec![event.payload.clone()],
                finish: None,
                usage: None,
                extensions: &Extensions::new(),
            },
        );
    }
    let data = serde_json::to_string(&event.payload)?;
    sse::write_event(out, None, &data);
    Ok(())
}

fn expect_index(actual: u32, expected: u32, what: &str) -> Result<(), CodecError> {
    if actual == expected {
        return Ok(());
    }
    Err(unsupported(format!(
        "Generative Language {what} block index {actual} is not {expected}"
    )))
}

fn expect_tool_index(index: u32) -> Result<(), CodecError> {
    if index >= TOOL_INDEX_BASE {
        return Ok(());
    }
    Err(unsupported(format!(
        "Generative Language function call index {index} is below {TOOL_INDEX_BASE}"
    )))
}
