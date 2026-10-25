//! Amazon Bedrock Converse and ConverseStream.
//!
//! The model id is not in the body. The caller posts
//! `/model/{modelId}/converse` or `converse-stream`. A stream is an AWS
//! event-stream, not SSE.
//!
//! Request and response shape, checked 2026-10-25:
//! <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html>
//! and
//! <https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_ConverseStream.html>.
//! `top_k` is not an `inferenceConfig` field. The Converse examples put it in
//! `additionalModelRequestFields`. Reasoning effort is not a Converse field
//! and is rejected rather than dropped. Image `detail` is a client hint and
//! is not sent. A URL image has no Converse source, so it is an error.
//! `toolChoice` is `auto`, `any`, or one named tool. An explicit choice of
//! none omits `toolConfig`.

use std::collections::BTreeSet;

use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, ContentPart, Extensions, ImageSource, Message,
    MessageDelta, MessageStart, MessageStop, Request, Response, Role, SignatureDelta, Stop,
    StopReason, StreamEvent, TextDelta, ThinkingDelta, ThinkingPart, ToolArguments,
    ToolArgumentsDelta, ToolChoiceMode, Usage,
};
use serde_json::{Map, Value, json};

use crate::error::CodecError;
use crate::eventstream::{self, Decoder as FrameDecoder};
use crate::jsonutil::{
    self, insert_f64, insert_u64, parse_object, require_index, require_string, take_object,
    take_string, take_u64, to_bytes,
};
use crate::{Codec, StreamDecoder, StreamEncoder};

const ID: &str = "bedrock-converse";

/// Bedrock Converse, unary and streamed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Converse;

impl Codec for Converse {
    fn id(&self) -> &'static str {
        ID
    }

    fn decode_request(&self, bytes: &[u8]) -> Result<Request, CodecError> {
        let mut object = parse_object(bytes)?;
        let mut request = Request::new("");
        if let Some(system) = object.remove("system") {
            for block in jsonutil::expect_array(system, "system")? {
                let mut block = jsonutil::expect_object(block, "system")?;
                if let Some(text) = take_string(&mut block, "text")? {
                    let mut message = Message::new(Role::System);
                    message.content.push(ContentPart::text(text));
                    message.extensions = Extensions::from_map(block);
                    request.messages.push(message);
                }
            }
        }
        if let Some(messages) = object.remove("messages") {
            for message in jsonutil::expect_array(messages, "messages")? {
                request.messages.push(decode_message(message)?);
            }
        }
        if let Some(mut config) = take_object(&mut object, "inferenceConfig")? {
            request.params.max_tokens = take_u64(&mut config, "maxTokens")?;
            request.params.temperature = jsonutil::take_f64(&mut config, "temperature")?;
            request.params.top_p = jsonutil::take_f64(&mut config, "topP")?;
            request.params.stop = jsonutil::take_string_list(&mut config, "stopSequences")?;
            if !config.is_empty() {
                request.params.extensions = Extensions::from_map(config);
            }
        }
        if let Some(mut extra) = take_object(&mut object, "additionalModelRequestFields")? {
            request.params.top_k = take_u64(&mut extra, "top_k")?;
            if !extra.is_empty() {
                request
                    .params
                    .extensions
                    .absorb(&Extensions::from_map(extra));
            }
        }
        if let Some(mut tools) = take_object(&mut object, "toolConfig")? {
            if let Some(list) = tools.remove("tools") {
                for tool in jsonutil::expect_array(list, "tools")? {
                    let mut tool = jsonutil::expect_object(tool, "tools")?;
                    let mut spec = take_object(&mut tool, "toolSpec")?
                        .ok_or(CodecError::MissingField("toolSpec"))?;
                    let name = require_string(&mut spec, "name")?;
                    let description = take_string(&mut spec, "description")?;
                    let mut schema = take_object(&mut spec, "inputSchema")?
                        .ok_or(CodecError::MissingField("inputSchema"))?;
                    let input_schema = schema.remove("json").unwrap_or(Value::Object(Map::new()));
                    request.tools.push(hofund_ir::Tool {
                        name,
                        description,
                        input_schema,
                        extensions: Extensions::from_map(spec),
                    });
                }
            }
            request.tool_choice = tools
                .remove("toolChoice")
                .map(decode_tool_choice)
                .transpose()?;
        }
        request.extensions = Extensions::from_map(object);
        Ok(request)
    }

    fn encode_request(&self, request: &Request) -> Result<Vec<u8>, CodecError> {
        reject_unmapped_params(request)?;
        let mut system = Vec::new();
        let mut messages = Vec::new();
        for message in &request.messages {
            match message.role {
                Role::System | Role::Developer => {
                    for part in &message.content {
                        match part {
                            ContentPart::Text(text) => {
                                let mut block = Map::new();
                                block.insert("text".to_owned(), Value::String(text.text.clone()));
                                text.extensions.merge_into(&mut block);
                                system.push(Value::Object(block));
                            }
                            _ => {
                                return Err(unsupported("Converse system content is text only"));
                            }
                        }
                    }
                }
                Role::User | Role::Assistant | Role::Tool => {
                    let role = if message.role == Role::Assistant {
                        "assistant"
                    } else {
                        "user"
                    };
                    let blocks = encode_parts(&message.content, message.role)?;
                    if blocks.is_empty() {
                        continue;
                    }
                    if let Some(Value::Object(last)) = messages.last_mut()
                        && last.get("role").and_then(Value::as_str) == Some(role)
                        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
                    {
                        content.extend(blocks);
                    } else {
                        messages.push(json!({ "role": role, "content": blocks }));
                    }
                }
            }
        }
        if messages
            .first()
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            != Some("user")
        {
            return Err(unsupported(
                "Converse requires the first message to be a user message",
            ));
        }
        let mut body = Map::new();
        if !system.is_empty() {
            body.insert("system".to_owned(), Value::Array(system));
        }
        body.insert("messages".to_owned(), Value::Array(messages));
        let mut inference = Map::new();
        insert_u64(&mut inference, "maxTokens", request.params.max_tokens);
        insert_f64(&mut inference, "temperature", request.params.temperature)?;
        insert_f64(&mut inference, "topP", request.params.top_p)?;
        if !request.params.stop.is_empty() {
            inference.insert(
                "stopSequences".to_owned(),
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
            body.insert(
                "additionalModelResponseFieldPaths".to_owned(),
                json!(["/stop_sequence"]),
            );
        }
        request.params.extensions.merge_into(&mut inference);
        if !inference.is_empty() {
            body.insert("inferenceConfig".to_owned(), Value::Object(inference));
        }
        if let Some(top_k) = request.params.top_k {
            body.insert(
                "additionalModelRequestFields".to_owned(),
                json!({ "top_k": top_k }),
            );
        }
        if let Some(tool_config) = encode_tools(request)? {
            body.insert("toolConfig".to_owned(), tool_config);
        }
        request.extensions.merge_into(&mut body);
        to_bytes(&Value::Object(body))
    }

    fn decode_response(&self, bytes: &[u8]) -> Result<Response, CodecError> {
        let mut object = parse_object(bytes)?;
        let output =
            take_object(&mut object, "output")?.ok_or(CodecError::MissingField("output"))?;
        let message = output
            .get("message")
            .cloned()
            .ok_or(CodecError::MissingField("message"))?;
        let message = decode_message(message)?;
        let mut sequence = None;
        let mut extensions = Extensions::new();
        if let Some(extra) = object.remove("additionalModelResponseFields") {
            if let Some(text) = extra.get("stop_sequence").and_then(Value::as_str) {
                sequence = Some(text.to_owned());
            }
            extensions.insert("additionalModelResponseFields", extra);
        }
        let stop = take_string(&mut object, "stopReason")?.map(|reason| Stop {
            reason: stop_reason(&reason),
            sequence,
        });
        let usage = take_object(&mut object, "usage")?
            .map(|mut usage| decode_usage(&mut usage))
            .transpose()?;
        extensions.absorb(&Extensions::from_map(object));
        Ok(Response::new(message)
            .with_stop(stop)
            .with_usage(usage)
            .with_extensions(extensions))
    }

    fn encode_response(&self, response: &Response) -> Result<Vec<u8>, CodecError> {
        let content = encode_parts(&response.message.content, Role::Assistant)?;
        let mut body = Map::new();
        body.insert(
            "output".to_owned(),
            json!({
                "message": { "role": "assistant", "content": content }
            }),
        );
        if let Some(stop) = &response.stop {
            body.insert(
                "stopReason".to_owned(),
                Value::String(stop_token(&stop.reason).to_owned()),
            );
            if let Some(sequence) = &stop.sequence {
                body.insert(
                    "additionalModelResponseFields".to_owned(),
                    json!({ "stop_sequence": sequence }),
                );
            }
        }
        if let Some(usage) = &response.usage {
            body.insert("usage".to_owned(), encode_usage(usage));
        }
        response.extensions.merge_into(&mut body);
        to_bytes(&Value::Object(body))
    }

    fn encode_stream(&self, events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
        let mut encoder = ConverseEncoder;
        let mut bytes = Vec::new();
        for event in events {
            bytes.extend(encoder.push(event)?);
        }
        bytes.extend(encoder.finish()?);
        Ok(bytes)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ConverseDecoder::default())
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ConverseEncoder)
    }
}

fn reject_unmapped_params(request: &Request) -> Result<(), CodecError> {
    if request.params.seed.is_some() {
        return Err(unsupported("Converse does not send seed"));
    }
    if request.params.presence_penalty.is_some() {
        return Err(unsupported("Converse does not send presence_penalty"));
    }
    if request.params.frequency_penalty.is_some() {
        return Err(unsupported("Converse does not send frequency_penalty"));
    }
    if request.params.effort.is_some() {
        return Err(unsupported(
            "Converse does not send reasoning effort; this build does not invent a thinking budget",
        ));
    }
    Ok(())
}

fn encode_parts(parts: &[ContentPart], role: Role) -> Result<Vec<Value>, CodecError> {
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text(text) => {
                let mut block = Map::new();
                block.insert("text".to_owned(), Value::String(text.text.clone()));
                text.extensions.merge_into(&mut block);
                blocks.push(Value::Object(block));
            }
            ContentPart::Image(image) => {
                if role == Role::Assistant {
                    return Err(unsupported(
                        "Converse does not send an image from the assistant",
                    ));
                }
                let ImageSource::Base64 { media_type, data } = &image.source else {
                    return Err(unsupported("Converse image source is bytes, not a URL"));
                };
                blocks.push(json!({
                    "image": {
                        "format": image_format(media_type)?,
                        "source": { "bytes": data }
                    }
                }));
            }
            ContentPart::ToolCall(call) => {
                if role != Role::Assistant {
                    return Err(unsupported(
                        "Converse sends tool calls on the assistant message",
                    ));
                }
                let input = tool_input(&call.arguments)?;
                blocks.push(json!({
                    "toolUse": { "toolUseId": call.id, "name": call.name, "input": input }
                }));
            }
            ContentPart::ToolResult(result) => {
                if role == Role::Assistant {
                    return Err(unsupported(
                        "Converse sends a tool result on a user message",
                    ));
                }
                let content = encode_parts(&result.content, Role::User)?;
                let mut block = json!({
                    "toolResult": { "toolUseId": result.id, "content": content }
                });
                if result.is_error {
                    block["toolResult"]["status"] = json!("error");
                }
                blocks.push(block);
            }
            ContentPart::Thinking(thinking) => {
                if role != Role::Assistant {
                    return Err(unsupported(
                        "Converse sends reasoning on the assistant message",
                    ));
                }
                blocks.push(Value::Object(thinking_block(thinking)));
            }
            ContentPart::Unknown(unknown) => {
                return Err(unsupported(format!(
                    "Converse cannot send content of type `{}`",
                    unknown.kind
                )));
            }
        }
    }
    Ok(blocks)
}

fn thinking_block(thinking: &ThinkingPart) -> Map<String, Value> {
    let mut block = Map::new();
    if thinking.redacted {
        block.insert(
            "reasoningContent".to_owned(),
            json!({ "redactedContent": thinking.data.clone().unwrap_or_default() }),
        );
    } else {
        let mut text = Map::new();
        text.insert("text".to_owned(), Value::String(thinking.text.clone()));
        if let Some(signature) = &thinking.signature {
            text.insert("signature".to_owned(), Value::String(signature.clone()));
        }
        block.insert(
            "reasoningContent".to_owned(),
            json!({ "reasoningText": Value::Object(text) }),
        );
    }
    thinking.extensions.merge_into(&mut block);
    block
}

fn tool_input(arguments: &ToolArguments) -> Result<Value, CodecError> {
    let value = match arguments {
        ToolArguments::Json { value } => value.clone(),
        ToolArguments::Raw { text } => serde_json::from_str(text)
            .map_err(|_| unsupported("Converse tool input must be a JSON object"))?,
    };
    if value.is_object() {
        Ok(value)
    } else {
        Err(unsupported("Converse tool input must be a JSON object"))
    }
}

fn image_format(media_type: &str) -> Result<&'static str, CodecError> {
    match media_type {
        "image/png" => Ok("png"),
        "image/jpeg" | "image/jpg" => Ok("jpeg"),
        "image/gif" => Ok("gif"),
        "image/webp" => Ok("webp"),
        other => Err(unsupported(format!(
            "Converse has no image format for `{other}`"
        ))),
    }
}

fn encode_tools(request: &Request) -> Result<Option<Value>, CodecError> {
    let choice = request.tool_choice.as_ref().map(|choice| &choice.mode);
    if matches!(choice, Some(ToolChoiceMode::None)) {
        return Ok(None);
    }
    if request.tools.is_empty() {
        if choice.is_some() {
            return Err(unsupported(
                "Converse tool choice is set and no tools were declared",
            ));
        }
        return Ok(None);
    }
    let mut tools = Vec::new();
    for tool in &request.tools {
        let mut spec = Map::new();
        spec.insert("name".to_owned(), Value::String(tool.name.clone()));
        if let Some(description) = &tool.description {
            spec.insert("description".to_owned(), Value::String(description.clone()));
        }
        spec.insert(
            "inputSchema".to_owned(),
            json!({ "json": tool.input_schema }),
        );
        tool.extensions.merge_into(&mut spec);
        tools.push(json!({ "toolSpec": Value::Object(spec) }));
    }
    let tool_choice = match choice {
        None | Some(ToolChoiceMode::Auto) => json!({ "auto": {} }),
        Some(ToolChoiceMode::Required) => json!({ "any": {} }),
        Some(ToolChoiceMode::Named { name }) => json!({ "tool": { "name": name } }),
        Some(ToolChoiceMode::None) => return Ok(None),
    };
    Ok(Some(json!({ "tools": tools, "toolChoice": tool_choice })))
}

fn decode_message(value: Value) -> Result<Message, CodecError> {
    let mut object = jsonutil::expect_object(value, "message")?;
    let role = match require_string(&mut object, "role")?.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        other => return Err(CodecError::UnknownRole(other.to_owned())),
    };
    let mut message = Message::new(role);
    if let Some(content) = object.remove("content") {
        for block in jsonutil::expect_array(content, "content")? {
            message.content.push(decode_block(block)?);
        }
    }
    message.extensions = Extensions::from_map(object);
    Ok(message)
}

fn decode_block(value: Value) -> Result<ContentPart, CodecError> {
    let mut block = jsonutil::expect_object(value, "content")?;
    if let Some(text) = take_string(&mut block, "text")? {
        return Ok(ContentPart::Text(hofund_ir::TextPart {
            text,
            extensions: Extensions::from_map(block),
        }));
    }
    if let Some(mut image) = take_object(&mut block, "image")? {
        let format = require_string(&mut image, "format")?;
        let mut source =
            take_object(&mut image, "source")?.ok_or(CodecError::MissingField("source"))?;
        let data = require_string(&mut source, "bytes")?;
        return Ok(ContentPart::Image(hofund_ir::ImagePart {
            source: ImageSource::Base64 {
                media_type: media_type(&format).to_owned(),
                data,
            },
            detail: None,
            extensions: Extensions::new(),
        }));
    }
    if let Some(mut call) = take_object(&mut block, "toolUse")? {
        let id = require_string(&mut call, "toolUseId")?;
        let name = require_string(&mut call, "name")?;
        let input = call.remove("input").unwrap_or(Value::Object(Map::new()));
        return Ok(ContentPart::ToolCall(hofund_ir::ToolCallPart {
            id,
            name,
            arguments: ToolArguments::Json { value: input },
            extensions: Extensions::from_map(call),
        }));
    }
    if let Some(mut result) = take_object(&mut block, "toolResult")? {
        let id = require_string(&mut result, "toolUseId")?;
        let is_error = take_string(&mut result, "status")?.as_deref() == Some("error");
        let mut content = Vec::new();
        if let Some(blocks) = result.remove("content") {
            for child in jsonutil::expect_array(blocks, "content")? {
                content.push(decode_block(child)?);
            }
        }
        return Ok(ContentPart::ToolResult(hofund_ir::ToolResultPart {
            id,
            is_error,
            content,
            extensions: Extensions::from_map(result),
        }));
    }
    if let Some(mut reasoning) = take_object(&mut block, "reasoningContent")? {
        if let Some(data) = take_string(&mut reasoning, "redactedContent")? {
            return Ok(ContentPart::Thinking(ThinkingPart {
                text: String::new(),
                signature: None,
                redacted: true,
                data: Some(data),
                extensions: Extensions::new(),
            }));
        }
        let mut text_block = take_object(&mut reasoning, "reasoningText")?.unwrap_or_default();
        return Ok(ContentPart::Thinking(ThinkingPart {
            text: take_string(&mut text_block, "text")?.unwrap_or_default(),
            signature: take_string(&mut text_block, "signature")?,
            redacted: false,
            data: None,
            extensions: Extensions::new(),
        }));
    }
    let kind = block
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "unknown".to_owned());
    Ok(ContentPart::Unknown(hofund_ir::UnknownPart {
        kind,
        extensions: Extensions::from_map(block),
    }))
}

fn decode_tool_choice(value: Value) -> Result<hofund_ir::ToolChoice, CodecError> {
    let object = jsonutil::expect_object(value, "toolChoice")?;
    if object.contains_key("auto") {
        return Ok(hofund_ir::ToolChoice::auto());
    }
    if object.contains_key("any") {
        return Ok(hofund_ir::ToolChoice::required());
    }
    if let Some(tool) = object.get("tool") {
        let mut tool = jsonutil::expect_object(tool.clone(), "tool")?;
        let name = require_string(&mut tool, "name")?;
        return Ok(hofund_ir::ToolChoice::named(name));
    }
    Err(unsupported(
        "Converse tool choice is not auto, any, or a named tool",
    ))
}

fn decode_usage(usage: &mut Map<String, Value>) -> Result<Usage, CodecError> {
    Ok(Usage {
        input_tokens: take_u64(usage, "inputTokens")?,
        output_tokens: take_u64(usage, "outputTokens")?,
        total_tokens: take_u64(usage, "totalTokens")?,
        cache_read_tokens: take_u64(usage, "cacheReadInputTokens")?,
        cache_write_tokens: take_u64(usage, "cacheWriteInputTokens")?,
        reasoning_tokens: None,
        extensions: Extensions::from_map(std::mem::take(usage)),
    })
}

fn encode_usage(usage: &Usage) -> Value {
    let mut object = Map::new();
    insert_u64(&mut object, "inputTokens", usage.input_tokens);
    insert_u64(&mut object, "outputTokens", usage.output_tokens);
    insert_u64(&mut object, "totalTokens", usage.total_tokens);
    insert_u64(&mut object, "cacheReadInputTokens", usage.cache_read_tokens);
    insert_u64(
        &mut object,
        "cacheWriteInputTokens",
        usage.cache_write_tokens,
    );
    usage.extensions.merge_into(&mut object);
    Value::Object(object)
}

fn stop_reason(token: &str) -> StopReason {
    match token {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "content_filtered" | "guardrail_intervened" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_owned()),
    }
}

fn stop_token(reason: &StopReason) -> &str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::ContentFilter => "content_filtered",
        StopReason::Refusal => "content_filtered",
        StopReason::Other(token) => token.as_str(),
    }
}

fn media_type(format: &str) -> &'static str {
    match format {
        "png" => "image/png",
        "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

fn unsupported(message: impl Into<String>) -> CodecError {
    CodecError::Unsupported {
        message: message.into(),
    }
}

struct ConverseDecoder {
    frames: FrameDecoder,
    text: BTreeSet<u32>,
    thinking: BTreeSet<u32>,
    tool: BTreeSet<u32>,
    stop: Option<Stop>,
    saw_stop: bool,
}

impl Default for ConverseDecoder {
    fn default() -> Self {
        Self {
            frames: FrameDecoder::new(),
            text: BTreeSet::new(),
            thinking: BTreeSet::new(),
            tool: BTreeSet::new(),
            stop: None,
            saw_stop: false,
        }
    }
}

impl ConverseDecoder {
    fn open_text(&mut self, events: &mut Vec<StreamEvent>, index: u32) {
        if self.text.insert(index) {
            events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
                index,
                block: BlockStart::Text,
                extensions: Extensions::new(),
            }));
        }
    }

    fn open_thinking(
        &mut self,
        events: &mut Vec<StreamEvent>,
        index: u32,
        redacted: bool,
        data: Option<String>,
    ) {
        if self.thinking.insert(index) {
            events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
                index,
                block: BlockStart::Thinking { redacted, data },
                extensions: Extensions::new(),
            }));
        }
    }

    fn flush(&mut self, events: &mut Vec<StreamEvent>, terminal: bool) {
        if self.stop.is_some() || (terminal && self.saw_stop) {
            events.push(StreamEvent::MessageDelta(MessageDelta {
                stop: self.stop.take(),
                usage: None,
                extensions: Extensions::new(),
            }));
            events.push(StreamEvent::MessageStop(MessageStop::default()));
            self.saw_stop = false;
        }
    }
}

impl StreamDecoder for ConverseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, CodecError> {
        let frames = self.frames.push(chunk)?;
        let mut events = Vec::new();
        for frame in frames {
            if frame.event_type.ends_with("Exception") {
                let text = String::from_utf8_lossy(&frame.payload);
                return Err(CodecError::Sse {
                    message: format!("Converse stream exception: {text}"),
                });
            }
            self.one(&frame.event_type, &frame.payload, &mut events)?;
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, CodecError> {
        self.frames.finish()?;
        let mut events = Vec::new();
        self.flush(&mut events, true);
        Ok(events)
    }
}

impl ConverseDecoder {
    fn one(
        &mut self,
        event_type: &str,
        payload: &[u8],
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        let mut object = if payload.is_empty() {
            Map::new()
        } else {
            parse_object(payload)?
        };
        match event_type {
            "messageStart" => {
                events.push(StreamEvent::MessageStart(MessageStart {
                    id: None,
                    model: None,
                    usage: None,
                    extensions: Extensions::new(),
                }));
            }
            "contentBlockStart" => {
                let index = require_index(&mut object, "contentBlockIndex")?;
                if let Some(mut start) = take_object(&mut object, "start")?
                    && let Some(mut tool) = take_object(&mut start, "toolUse")?
                {
                    let id = take_string(&mut tool, "toolUseId")?.unwrap_or_default();
                    let name = take_string(&mut tool, "name")?.unwrap_or_default();
                    self.tool.insert(index);
                    events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
                        index,
                        block: BlockStart::ToolCall { id, name },
                        extensions: Extensions::new(),
                    }));
                }
            }
            "contentBlockDelta" => self.delta(&mut object, events)?,
            "contentBlockStop" => {
                let index = require_index(&mut object, "contentBlockIndex")?;
                events.push(StreamEvent::ContentBlockStop(ContentBlockStop {
                    index,
                    extensions: Extensions::new(),
                }));
            }
            "messageStop" => {
                let reason = take_string(&mut object, "stopReason")?
                    .unwrap_or_else(|| "end_turn".to_owned());
                let sequence = object
                    .get("additionalModelResponseFields")
                    .and_then(|value| value.get("stop_sequence"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.stop = Some(Stop {
                    reason: stop_reason(&reason),
                    sequence,
                });
                self.saw_stop = true;
            }
            "metadata" => {
                let usage = take_object(&mut object, "usage")?
                    .map(|mut usage| decode_usage(&mut usage))
                    .transpose()?;
                events.push(StreamEvent::MessageDelta(MessageDelta {
                    stop: self.stop.take(),
                    usage,
                    extensions: Extensions::new(),
                }));
                if self.saw_stop {
                    events.push(StreamEvent::MessageStop(MessageStop::default()));
                    self.saw_stop = false;
                }
            }
            other => {
                events.push(StreamEvent::Unknown(hofund_ir::UnknownEvent {
                    name: other.to_owned(),
                    payload: Value::Object(object),
                }));
            }
        }
        Ok(())
    }

    fn delta(
        &mut self,
        object: &mut Map<String, Value>,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), CodecError> {
        let index = require_index(object, "contentBlockIndex")?;
        let Some(mut delta) = take_object(object, "delta")? else {
            return Ok(());
        };
        if let Some(text) = take_string(&mut delta, "text")? {
            self.open_text(events, index);
            events.push(StreamEvent::TextDelta(TextDelta {
                index,
                text,
                extensions: Extensions::new(),
            }));
        }
        if let Some(mut tool) = take_object(&mut delta, "toolUse")? {
            if self.tool.insert(index) {
                events.push(StreamEvent::ContentBlockStart(ContentBlockStart {
                    index,
                    block: BlockStart::ToolCall {
                        id: String::new(),
                        name: String::new(),
                    },
                    extensions: Extensions::new(),
                }));
            }
            if let Some(input) = take_string(&mut tool, "input")? {
                events.push(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                    index,
                    partial_json: input,
                    extensions: Extensions::new(),
                }));
            }
        }
        if let Some(mut reasoning) = take_object(&mut delta, "reasoningContent")? {
            if let Some(data) = take_string(&mut reasoning, "redactedContent")? {
                self.open_thinking(events, index, true, Some(data));
            }
            let mut nested = take_object(&mut reasoning, "reasoningText")?.unwrap_or_default();
            let text = take_string(&mut reasoning, "text")?
                .or(take_string(&mut nested, "text")?)
                .unwrap_or_default();
            let signature = take_string(&mut reasoning, "signature")?
                .or(take_string(&mut nested, "signature")?);
            if !text.is_empty() || signature.is_some() {
                self.open_thinking(events, index, false, None);
            }
            if !text.is_empty() {
                events.push(StreamEvent::ThinkingDelta(ThinkingDelta {
                    index,
                    text,
                    extensions: Extensions::new(),
                }));
            }
            if let Some(signature) = signature {
                events.push(StreamEvent::SignatureDelta(SignatureDelta {
                    index,
                    signature,
                    extensions: Extensions::new(),
                }));
            }
        }
        Ok(())
    }
}

struct ConverseEncoder;

impl StreamEncoder for ConverseEncoder {
    fn push(&mut self, event: &StreamEvent) -> Result<Vec<u8>, CodecError> {
        let (name, payload) = match event {
            StreamEvent::MessageStart(_) => ("messageStart", json!({ "role": "assistant" })),
            StreamEvent::ContentBlockStart(start) => match &start.block {
                BlockStart::ToolCall { id, name } => (
                    "contentBlockStart",
                    json!({
                        "contentBlockIndex": start.index,
                        "start": { "toolUse": { "toolUseId": id, "name": name } }
                    }),
                ),
                BlockStart::Text | BlockStart::Thinking { .. } | BlockStart::Unknown { .. } => {
                    return Ok(Vec::new());
                }
            },
            StreamEvent::TextDelta(delta) => (
                "contentBlockDelta",
                json!({ "contentBlockIndex": delta.index, "delta": { "text": delta.text } }),
            ),
            StreamEvent::ThinkingDelta(delta) => (
                "contentBlockDelta",
                json!({
                    "contentBlockIndex": delta.index,
                    "delta": { "reasoningContent": { "text": delta.text } }
                }),
            ),
            StreamEvent::SignatureDelta(delta) => (
                "contentBlockDelta",
                json!({
                    "contentBlockIndex": delta.index,
                    "delta": { "reasoningContent": { "signature": delta.signature } }
                }),
            ),
            StreamEvent::ToolArgumentsDelta(delta) => (
                "contentBlockDelta",
                json!({
                    "contentBlockIndex": delta.index,
                    "delta": { "toolUse": { "input": delta.partial_json } }
                }),
            ),
            StreamEvent::ContentBlockStop(stop) => (
                "contentBlockStop",
                json!({ "contentBlockIndex": stop.index }),
            ),
            StreamEvent::MessageDelta(delta) => {
                if let Some(stop) = &delta.stop {
                    return Ok(eventstream::encode_event(
                        "messageStop",
                        &serde_json::to_vec(&json!({ "stopReason": stop_token(&stop.reason) }))?,
                    ));
                }
                if let Some(usage) = &delta.usage {
                    return Ok(eventstream::encode_event(
                        "metadata",
                        &serde_json::to_vec(&json!({ "usage": encode_usage(usage) }))?,
                    ));
                }
                return Ok(Vec::new());
            }
            StreamEvent::MessageStop(_) | StreamEvent::Ping(_) => return Ok(Vec::new()),
            StreamEvent::Unknown(unknown) => {
                return Err(unsupported(format!(
                    "Converse cannot send stream event `{}`",
                    unknown.name
                )));
            }
        };
        Ok(eventstream::encode_event(
            name,
            &serde_json::to_vec(&payload)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::Converse;
    use crate::Codec;
    use hofund_ir::{ContentPart, ImageSource, Request, Role, ToolArguments};

    #[test]
    fn a_user_turn_encodes_inference_config_and_top_k() {
        let mut request = Request::new("amazon.nova-pro-v1:0");
        let mut system = hofund_ir::Message::new(Role::System);
        system.content.push(ContentPart::text("be brief"));
        let mut user = hofund_ir::Message::new(Role::User);
        user.content.push(ContentPart::text("hello"));
        request.messages = vec![system, user];
        request.params.max_tokens = Some(32);
        request.params.top_k = Some(40);
        request.params.temperature = Some(0.2);
        let bytes = Converse.encode_request(&request).expect("encode");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["system"][0]["text"], "be brief");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(value["inferenceConfig"]["maxTokens"], 32);
        assert_eq!(value["inferenceConfig"]["temperature"], 0.2);
        assert_eq!(value["additionalModelRequestFields"]["top_k"], 40);
        assert!(value.get("model").is_none());
    }

    #[test]
    fn effort_and_a_url_image_are_rejected() {
        let mut request = Request::new("model");
        let mut user = hofund_ir::Message::new(Role::User);
        user.content.push(ContentPart::text("hi"));
        request.messages = vec![user];
        request.params.effort = Some(hofund_ir::Effort::Low);
        let error = Converse.encode_request(&request).expect_err("effort");
        assert!(error.to_string().contains("reasoning effort"), "{error}");

        request.params.effort = None;
        request.messages[0].content = vec![ContentPart::Image(hofund_ir::ImagePart {
            source: ImageSource::Url {
                url: "https://example.com/a.png".to_owned(),
            },
            detail: None,
            extensions: hofund_ir::Extensions::new(),
        })];
        let error = Converse.encode_request(&request).expect_err("url");
        assert!(error.to_string().contains("bytes"), "{error}");
    }

    #[test]
    fn a_response_keeps_cache_tokens_and_the_stop_sequence() {
        let bytes = br#"{
            "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
            "stopReason": "stop_sequence",
            "additionalModelResponseFields": {"stop_sequence": "END"},
            "usage": {
                "inputTokens": 3,
                "outputTokens": 1,
                "totalTokens": 4,
                "cacheReadInputTokens": 2,
                "cacheWriteInputTokens": 1
            }
        }"#;
        let response = Converse.decode_response(bytes).expect("decode");
        assert_eq!(response.message.content, vec![ContentPart::text("ok")]);
        let stop = response.stop.expect("stop");
        assert_eq!(stop.sequence.as_deref(), Some("END"));
        let usage = response.usage.expect("usage");
        assert_eq!(usage.cache_read_tokens, Some(2));
        assert_eq!(usage.cache_write_tokens, Some(1));
    }

    #[test]
    fn a_tool_call_round_trips_as_tool_use() {
        let mut request = Request::new("model");
        let mut user = hofund_ir::Message::new(Role::User);
        user.content.push(ContentPart::text("weather"));
        let mut assistant = hofund_ir::Message::new(Role::Assistant);
        assistant
            .content
            .push(ContentPart::ToolCall(hofund_ir::ToolCallPart {
                id: "call_1".to_owned(),
                name: "weather".to_owned(),
                arguments: ToolArguments::Json {
                    value: serde_json::json!({"city": "Paris"}),
                },
                extensions: hofund_ir::Extensions::new(),
            }));
        let mut tool = hofund_ir::Message::new(Role::Tool);
        tool.content
            .push(ContentPart::ToolResult(hofund_ir::ToolResultPart {
                id: "call_1".to_owned(),
                is_error: false,
                content: vec![ContentPart::text("rain")],
                extensions: hofund_ir::Extensions::new(),
            }));
        request.messages = vec![user, assistant, tool];
        request.tools.push(hofund_ir::Tool {
            name: "weather".to_owned(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            extensions: hofund_ir::Extensions::new(),
        });
        request.tool_choice = Some(hofund_ir::ToolChoice::named("weather"));
        let bytes = Converse.encode_request(&request).expect("encode");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(
            value["messages"][1]["content"][0]["toolUse"]["name"],
            "weather"
        );
        assert_eq!(value["messages"][2]["role"], "user");
        assert_eq!(
            value["messages"][2]["content"][0]["toolResult"]["content"][0]["text"],
            "rain"
        );
        assert_eq!(value["toolConfig"]["toolChoice"]["tool"]["name"], "weather");
    }

    #[test]
    fn a_text_stream_opens_a_block_and_reports_usage_with_the_stop() {
        let mut decoder = Converse.stream_decoder();
        let text = crate::eventstream::encode_event(
            "contentBlockDelta",
            br#"{"contentBlockIndex":0,"delta":{"text":"Hi"}}"#,
        );
        let stop = crate::eventstream::encode_event("messageStop", br#"{"stopReason":"end_turn"}"#);
        let usage = crate::eventstream::encode_event(
            "metadata",
            br#"{"usage":{"inputTokens":2,"outputTokens":1,"totalTokens":3}}"#,
        );
        let mut events = decoder.push(&text).expect("delta");
        events.extend(decoder.push(&stop).expect("stop"));
        events.extend(decoder.push(&usage).expect("usage"));
        assert!(matches!(
            events[0],
            hofund_ir::StreamEvent::ContentBlockStart(_)
        ));
        assert!(matches!(events[1], hofund_ir::StreamEvent::TextDelta(_)));
        match &events[2] {
            hofund_ir::StreamEvent::MessageDelta(delta) => {
                assert!(delta.stop.is_some());
                assert_eq!(
                    delta.usage.as_ref().and_then(|usage| usage.input_tokens),
                    Some(2)
                );
            }
            other => panic!("expected message delta, got {other:?}"),
        }
        assert!(matches!(events[3], hofund_ir::StreamEvent::MessageStop(_)));
    }
}
