//! GetChatMessage protobuf and Connect response decoding.

use std::collections::{HashSet, VecDeque};

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::inflate::compress;
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::protobuf::{FLAG_GZIP, MAX_FRAME_PAYLOAD, ProtoBuf, fields, frame, frames};

use super::metadata;

pub(crate) const GZIP_AT: usize = 8192;
const DEFAULT_MAX_TOKENS: u64 = 128_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PromptRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptCall<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) arguments: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptImage<'a> {
    pub(crate) data: &'a str,
    pub(crate) media_type: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PromptThinking<'a> {
    pub(crate) text: &'a str,
    pub(crate) signature: Option<&'a str>,
    pub(crate) signature_type: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Prompt<'a> {
    pub(crate) id: &'a str,
    pub(crate) role: PromptRole,
    pub(crate) text: &'a str,
    pub(crate) calls: Vec<PromptCall<'a>>,
    pub(crate) answer: Option<&'a str>,
    pub(crate) images: Vec<PromptImage<'a>>,
    pub(crate) thinking: Option<PromptThinking<'a>>,
}

impl<'a> Prompt<'a> {
    pub(crate) fn text(id: &'a str, role: PromptRole, text: &'a str) -> Self {
        Self {
            id,
            role,
            text,
            calls: Vec::new(),
            answer: None,
            images: Vec::new(),
            thinking: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolDefinition<'a> {
    pub(crate) name: &'a str,
    pub(crate) description: Option<&'a str>,
    pub(crate) schema: &'a Value,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChatRequest<'a> {
    pub(crate) session_token: &'a str,
    pub(crate) os: &'a str,
    pub(crate) model_uid: &'a str,
    pub(crate) prompts: &'a [Prompt<'a>],
    pub(crate) tools: &'a [ToolDefinition<'a>],
    pub(crate) max_tokens: Option<u64>,
    pub(crate) temperature: Option<f64>,
    pub(crate) top_p: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EncodedChatRequest {
    pub(crate) body: Vec<u8>,
    pub(crate) gzipped: bool,
}

pub(crate) fn encode_request(request: &ChatRequest<'_>) -> Result<EncodedChatRequest, PluginError> {
    let mut message = ProtoBuf::new().bytes(1, &metadata(request.session_token, request.os));
    for prompt in request.prompts {
        message = message.bytes(3, &encode_prompt(prompt));
    }
    let sampling = sampling(request);
    message = message.varint(7, 5).bytes(8, &sampling);
    let mut offered = HashSet::new();
    for tool in request.tools {
        let description = tool.description.filter(|text| !text.is_empty()).map(|_| {
            format!(
                "Described under <tool name=\"{}\"> in <tool_descriptions>, in the instructions.",
                tool.name
            )
        });
        message = message.bytes(
            10,
            &encode_tool(
                tool.name,
                description.as_deref().unwrap_or_default(),
                Some(tool.schema),
            ),
        );
        offered.insert(tool.name);
    }
    for call in request.prompts.iter().flat_map(|prompt| &prompt.calls) {
        if offered.insert(call.name) {
            message = message.bytes(10, &encode_tool(call.name, "Tool", None));
        }
    }
    frame_request(&message.string(21, request.model_uid).finish())
}

fn encode_tool(name: &str, description: &str, schema: Option<&Value>) -> Vec<u8> {
    let default = json!({"type": "object", "properties": {}});
    let schema = schema
        .filter(|value| value.is_object() || value.is_array())
        .unwrap_or(&default);
    let schema = hofund_plugin_sdk::json::to_string(schema)
        .expect("a JSON tool schema always serializes to a string");
    ProtoBuf::new()
        .string(1, name)
        .string(2, description)
        .bytes(3, schema.as_bytes())
        .finish()
}

fn encode_prompt(prompt: &Prompt<'_>) -> Vec<u8> {
    let role = match prompt.role {
        PromptRole::User => 1,
        PromptRole::Assistant => 2,
        PromptRole::Tool => 4,
    };
    let mut encoded = ProtoBuf::new().string(1, prompt.id).varint(2, role);
    if !prompt.text.is_empty() {
        encoded = encoded.string(3, prompt.text);
    }
    for call in &prompt.calls {
        let call = ProtoBuf::new()
            .string(1, call.id)
            .string(2, call.name)
            .bytes(3, call.arguments.as_bytes())
            .finish();
        encoded = encoded.bytes(6, &call);
    }
    if let Some(answer) = prompt.answer {
        encoded = encoded.string(7, answer);
    }
    for image in &prompt.images {
        let image = ProtoBuf::new()
            .string(1, image.data)
            .string(2, image.media_type)
            .finish();
        encoded = encoded.bytes(10, &image);
    }
    if let Some(thinking) = &prompt.thinking {
        if !thinking.text.is_empty() {
            encoded = encoded.string(11, thinking.text);
        }
        if let Some(signature) = thinking.signature.filter(|value| !value.is_empty()) {
            encoded = encoded.string(12, signature);
            if let Some(kind) = thinking.signature_type.filter(|value| !value.is_empty()) {
                encoded = encoded.string(18, kind);
            }
        }
    }
    encoded.finish()
}

fn sampling(request: &ChatRequest<'_>) -> Vec<u8> {
    ProtoBuf::new()
        .varint(1, 1)
        .varint(2, request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS))
        .varint(3, 400)
        .double(5, above_zero(request.temperature.unwrap_or(1.0)))
        .varint(7, 40)
        .double(8, above_zero(request.top_p.unwrap_or(0.95)))
        .finish()
}

fn above_zero(value: f64) -> f64 {
    if value == 0.0 { 1e-6 } else { value }
}

fn frame_request(message: &[u8]) -> Result<EncodedChatRequest, PluginError> {
    if message.len() > GZIP_AT {
        return Ok(EncodedChatRequest {
            body: frame(FLAG_GZIP, &compress(message)?)?,
            gzipped: true,
        });
    }
    Ok(EncodedChatRequest {
        body: frame(0, message)?,
        gzipped: false,
    })
}

pub(crate) fn out_id(id: &str) -> String {
    if safe_id(id) && !id.starts_with("dv_") {
        id.to_owned()
    } else {
        format!("dv_{}", base64url_encode(id.as_bytes()))
    }
}

pub(crate) fn in_id(id: &str) -> String {
    if !id.starts_with("dv_") || !safe_id(id) {
        return id.to_owned();
    }
    let Some(raw) = base64url_decode(&id[3..]).and_then(|bytes| String::from_utf8(bytes).ok())
    else {
        return id.to_owned();
    };
    if !raw.is_empty() && out_id(&raw) == id {
        raw
    } else {
        id.to_owned()
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (chunk.get(1).copied().map_or(0, u32::from) << 8)
            | chunk.get(2).copied().map_or(0, u32::from);
        for index in 0..chunk.len() + 1 {
            out.push(char::from(
                ALPHABET[((triple >> (18 - 6 * index)) & 0x3f) as usize],
            ));
        }
    }
    out
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.as_bytes().chunks(4) {
        let mut triple = 0_u32;
        for (index, byte) in chunk.iter().enumerate() {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'-' => 62,
                b'_' => 63,
                _ => return None,
            };
            triple |= u32::from(value) << (18 - 6 * index);
        }
        out.push((triple >> 16) as u8);
        if chunk.len() > 2 {
            out.push((triple >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatResponse {
    pub(crate) text: Vec<String>,
    pub(crate) reasoning: Vec<String>,
    pub(crate) error: Option<ConnectError>,
    pub(crate) tool_calls: Vec<ChatToolCall>,
    pub(crate) tool_arguments: Vec<ChatToolArguments>,
    pub(crate) signatures: Vec<String>,
    pub(crate) signature_types: Vec<String>,
    pub(crate) saw_usage: bool,
    pub(crate) usage: ChatUsage,
    pub(crate) stop: Option<u64>,
}

#[cfg(test)]
impl ChatResponse {
    pub(crate) fn has_text_stream(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty()
    }

    pub(crate) fn is_empty_reply(&self) -> bool {
        self.error.is_none() && !self.has_text_stream() && self.tool_calls.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConnectError {
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChatUsage {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_write: u64,
    pub(crate) cache_read: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatToolCall {
    pub(crate) index: u32,
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatToolArguments {
    pub(crate) index: u32,
    pub(crate) arguments: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChatEvent {
    Text(String),
    Reasoning(String),
    ToolCall(ChatToolCall),
    ToolArguments(ChatToolArguments),
    Signature(String),
    SignatureType(String),
    Usage(ChatUsage),
    Stop(u64),
    End(Option<ConnectError>),
}

#[derive(Default)]
pub(crate) struct ChatDecoder {
    buffered: Vec<u8>,
    pending: VecDeque<ChatEvent>,
    saw_frame: bool,
    active_tool: Option<u32>,
}

impl ChatDecoder {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), PluginError> {
        self.buffered.extend_from_slice(chunk);
        loop {
            if self.buffered.len() < 5 {
                return Ok(());
            }
            let length = u32::from_be_bytes([
                self.buffered[1],
                self.buffered[2],
                self.buffered[3],
                self.buffered[4],
            ]) as usize;
            if length > MAX_FRAME_PAYLOAD {
                return Err(PluginError::decode(format!(
                    "frame payload exceeds {MAX_FRAME_PAYLOAD} bytes (got {length})"
                )));
            }
            let end = 5_usize
                .checked_add(length)
                .ok_or_else(|| PluginError::decode("frame payload length overflow"))?;
            if self.buffered.len() < end {
                return Ok(());
            }
            let encoded: Vec<_> = self.buffered.drain(..end).collect();
            let frame = frames(&encoded)
                .next()
                .expect("a complete frame yields one item")?;
            self.saw_frame = true;
            if frame.is_end() {
                self.pending
                    .push_back(ChatEvent::End(end_error(&frame.payload)?));
                continue;
            }
            for field in fields(&frame.payload) {
                if field.number == 6
                    && let Some(bytes) = field.as_bytes()
                {
                    self.decode_tool_call(bytes)?;
                    continue;
                }
                let event = match field.number {
                    3 => field
                        .as_string()
                        .filter(|text| !text.is_empty())
                        .map(ChatEvent::Text),
                    9 => field
                        .as_string()
                        .filter(|text| !text.is_empty())
                        .map(ChatEvent::Reasoning),
                    10 => field
                        .as_string()
                        .filter(|text| !text.is_empty())
                        .map(ChatEvent::Signature),
                    21 => field
                        .as_string()
                        .filter(|text| !text.is_empty())
                        .map(ChatEvent::SignatureType),
                    7 => field
                        .as_bytes()
                        .map(|bytes| ChatEvent::Usage(decode_usage(bytes))),
                    5 => field.as_varint().map(ChatEvent::Stop),
                    _ => None,
                };
                if let Some(event) = event {
                    self.pending.push_back(event);
                }
            }
        }
    }

    fn decode_tool_call(&mut self, bytes: &[u8]) -> Result<(), PluginError> {
        let mut id = String::new();
        let mut name = String::new();
        let mut arguments = String::new();
        for field in fields(bytes) {
            let text = field.as_string().unwrap_or_default();
            match field.number {
                1 => id = text,
                2 => name = text,
                3 => arguments.push_str(&text),
                _ => {}
            }
        }
        if !name.is_empty() {
            let index = match self.active_tool {
                Some(index) => index
                    .checked_add(1)
                    .ok_or_else(|| PluginError::decode("too many tool calls"))?,
                None => 0,
            };
            self.active_tool = Some(index);
            if id.is_empty() {
                id = random_call_id()?;
            }
            self.pending.push_back(ChatEvent::ToolCall(ChatToolCall {
                index,
                id: out_id(&id),
                name,
            }));
        }
        if !arguments.is_empty()
            && let Some(index) = self.active_tool
        {
            self.pending
                .push_back(ChatEvent::ToolArguments(ChatToolArguments {
                    index,
                    arguments,
                }));
        }
        Ok(())
    }

    pub(crate) fn next_event(&mut self) -> Option<ChatEvent> {
        self.pending.pop_front()
    }

    pub(crate) fn finish(&self) -> Result<(), PluginError> {
        if self.buffered.is_empty() {
            Ok(())
        } else {
            Err(PluginError::decode("unexpected EOF"))
        }
    }

    pub(crate) fn saw_frame(&self) -> bool {
        self.saw_frame
    }
}

fn random_call_id() -> Result<String, PluginError> {
    let uuid = hofund_plugin_sdk::uuid::v4().map_err(|error| PluginError::io(error.to_string()))?;
    let hex: String = uuid
        .chars()
        .filter(|ch| *ch != '-')
        .enumerate()
        .filter_map(|(index, ch)| (index != 12 && index != 16).then_some(ch))
        .take(24)
        .collect();
    Ok(format!("call_{hex}"))
}

fn decode_usage(bytes: &[u8]) -> ChatUsage {
    let mut usage = ChatUsage::default();
    for field in fields(bytes) {
        let Some(value) = field.as_varint().filter(|value| *value != 0) else {
            continue;
        };
        match field.number {
            2 => usage.input = value,
            3 => usage.output = value,
            4 => usage.cache_write = value,
            5 => usage.cache_read = value,
            _ => {}
        }
    }
    usage
}

#[cfg(test)]
pub(crate) fn decode_response(body: &[u8]) -> Result<ChatResponse, PluginError> {
    let mut decoder = ChatDecoder::default();
    decoder.push(body)?;
    decoder.finish()?;
    let mut response = ChatResponse::default();
    while let Some(event) = decoder.next_event() {
        match event {
            ChatEvent::Text(text) => response.text.push(text),
            ChatEvent::Reasoning(text) => response.reasoning.push(text),
            ChatEvent::ToolCall(call) => response.tool_calls.push(call),
            ChatEvent::ToolArguments(arguments) => response.tool_arguments.push(arguments),
            ChatEvent::Signature(signature) => response.signatures.push(signature),
            ChatEvent::SignatureType(kind) => response.signature_types.push(kind),
            ChatEvent::Usage(usage) => {
                response.saw_usage = true;
                response.usage = usage;
            }
            ChatEvent::Stop(stop) => response.stop = Some(stop),
            ChatEvent::End(error) => {
                response.error = error;
                break;
            }
        }
    }
    Ok(response)
}

pub(crate) fn end_error(payload: &[u8]) -> Result<Option<ConnectError>, PluginError> {
    let value: Value = hofund_plugin_sdk::json::from_slice(payload)
        .map_err(|error| PluginError::decode(format!("invalid Connect end JSON: {error}")))?;
    let object = if value.get("error").is_some_and(Value::is_object) {
        value.get("error").unwrap_or(&Value::Null)
    } else {
        &value
    };
    let Some(code) = object.get("code").and_then(Value::as_str) else {
        return Ok(None);
    };
    if code.is_empty() {
        return Ok(None);
    }
    Ok(Some(ConnectError {
        code: code.to_owned(),
        message: object
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::inflate::compress;
    use hofund_plugin_sdk::protobuf::{FLAG_END, FLAG_GZIP, WireValue, deframe, fields, frame};

    use super::{
        ChatRequest, ConnectError, GZIP_AT, Prompt, PromptRole, decode_response, encode_request,
        frame_request,
    };

    const EMPTY_REPLY: &[u8] = crate::connect::testdata::EMPTY_REPLY;

    fn request<'a>(prompts: &'a [Prompt<'a>]) -> ChatRequest<'a> {
        ChatRequest {
            session_token: "session-token",
            os: "linux",
            model_uid: "swe-2-high",
            prompts,
            tools: &[],
            max_tokens: None,
            temperature: None,
            top_p: None,
        }
    }

    #[test]
    fn text_only_request_uses_the_chat_field_table() {
        let prompts = [
            Prompt::text("user-id", PromptRole::User, "hello"),
            Prompt::text("assistant-id", PromptRole::Assistant, "answer"),
        ];
        let encoded = encode_request(&request(&prompts)).expect("encode");
        assert!(!encoded.gzipped);
        let frame = deframe(&encoded.body).expect("request frame");
        assert_eq!(frame.flags, 0);
        let outer = fields(&frame.payload);
        assert_eq!(outer[0].number, 1);
        let prompts: Vec<_> = outer.iter().filter(|field| field.number == 3).collect();
        assert_eq!(prompts.len(), 2);
        let first = fields(prompts[0].as_bytes().expect("prompt"));
        assert_eq!(first[0].as_string().as_deref(), Some("user-id"));
        assert_eq!(first[1].as_varint(), Some(1));
        assert_eq!(first[2].as_string().as_deref(), Some("hello"));
        assert_eq!(first.len(), 3);
        let second = fields(prompts[1].as_bytes().expect("prompt"));
        assert_eq!(second[1].as_varint(), Some(2));
        assert_eq!(
            outer
                .iter()
                .find(|field| field.number == 7)
                .and_then(|field| field.as_varint()),
            Some(5)
        );
        assert_eq!(
            outer
                .iter()
                .find(|field| field.number == 21)
                .and_then(|field| field.as_string())
                .as_deref(),
            Some("swe-2-high")
        );
        let sampling = fields(
            outer
                .iter()
                .find(|field| field.number == 8)
                .and_then(|field| field.as_bytes())
                .expect("sampling"),
        );
        assert_eq!(sampling[1].as_varint(), Some(128_000));
        assert_eq!(sampling[3].value, WireValue::Fixed64(1.0_f64.to_bits()));
        assert_eq!(sampling[5].value, WireValue::Fixed64(0.95_f64.to_bits()));
        assert!(!outer.iter().any(|field| field.number == 10));
    }

    #[test]
    fn zero_sampling_values_become_one_millionth() {
        let prompts = [Prompt::text("id", PromptRole::User, "hello")];
        let mut request = request(&prompts);
        request.temperature = Some(0.0);
        request.top_p = Some(0.0);
        request.max_tokens = Some(42);
        let encoded = encode_request(&request).expect("encode");
        let frame = deframe(&encoded.body).expect("frame");
        let outer = fields(&frame.payload);
        let sampling = fields(
            outer
                .iter()
                .find(|field| field.number == 8)
                .and_then(|field| field.as_bytes())
                .expect("sampling"),
        );
        assert_eq!(sampling[1].as_varint(), Some(42));
        assert_eq!(sampling[3].value, WireValue::Fixed64(1e-6_f64.to_bits()));
        assert_eq!(sampling[5].value, WireValue::Fixed64(1e-6_f64.to_bits()));
    }

    #[test]
    fn image_prompts_encode_data_and_media_type() {
        let mut prompt = Prompt::text("id", PromptRole::User, "look");
        prompt.images.push(super::PromptImage {
            data: "AAAA",
            media_type: "image/png",
        });
        let encoded = encode_request(&request(&[prompt])).expect("encode");
        let outer = fields(&deframe(&encoded.body).expect("frame").payload);
        let prompt = outer
            .iter()
            .find(|field| field.number == 3)
            .and_then(|field| field.as_bytes())
            .expect("prompt");
        let image = fields(prompt)
            .into_iter()
            .find(|field| field.number == 10)
            .expect("image");
        let image = fields(image.as_bytes().expect("image bytes"));
        assert_eq!(image[0].as_string().as_deref(), Some("AAAA"));
        assert_eq!(image[1].as_string().as_deref(), Some("image/png"));
    }

    #[test]
    fn gzip_threshold_matches_the_captured_cli_sizes() {
        let plain = frame_request(&vec![0; 8083]).expect("plain");
        assert!(!plain.gzipped);
        assert_eq!(deframe(&plain.body).expect("plain frame").flags, 0);
        assert_eq!(GZIP_AT, 8192);

        let gzipped = frame_request(&vec![0; 8333]).expect("gzip");
        assert!(gzipped.gzipped);
        let decoded = deframe(&gzipped.body).expect("gzip frame");
        assert_eq!(decoded.flags, FLAG_GZIP);
        assert_eq!(decoded.payload, vec![0; 8333]);
    }

    #[test]
    fn outbound_chat_bytes_do_not_contain_product_names() {
        let prompts = [Prompt::text("id", PromptRole::User, "hello")];
        let encoded = encode_request(&request(&prompts)).expect("encode");
        let lower = String::from_utf8_lossy(&encoded.body).to_ascii_lowercase();
        assert!(!lower.contains("hofund"));
        let reference = String::from("mag") + "pie";
        assert!(!lower.contains(&reference));
    }

    #[test]
    fn response_surfaces_nonempty_text_deltas_in_order() {
        let first = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(3, "hello")
            .string(3, "")
            .string(9, "not text in this slice")
            .finish();
        let second = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(3, " world")
            .finish();
        let mut body = frame(0, &first).expect("first");
        body.extend(frame(0, &second).expect("second"));
        body.extend(frame(FLAG_END, b"{}").expect("end"));
        let decoded = decode_response(&body).expect("decode");
        assert_eq!(decoded.text, ["hello", " world"]);
        assert_eq!(decoded.reasoning, ["not text in this slice"]);
        assert!(decoded.has_text_stream());
        assert!(!decoded.is_empty_reply());
        assert_eq!(decoded.error, None);
    }

    #[test]
    fn incremental_decoder_waits_for_split_frames_and_surfaces_reasoning_and_stop() {
        use super::{ChatDecoder, ChatEvent};

        let message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(9, "thinking")
            .varint(5, 3)
            .finish();
        let body = frame(0, &message).expect("frame");
        let mut decoder = ChatDecoder::default();
        decoder.push(&body[..2]).expect("partial header");
        assert!(decoder.next_event().is_none());
        decoder.push(&body[2..7]).expect("partial payload");
        assert!(decoder.next_event().is_none());
        decoder.push(&body[7..]).expect("rest");
        assert_eq!(
            decoder.next_event(),
            Some(ChatEvent::Reasoning("thinking".to_owned()))
        );
        assert_eq!(decoder.next_event(), Some(ChatEvent::Stop(3)));
        assert!(decoder.saw_frame());
        decoder.finish().expect("complete");
    }

    #[test]
    fn empty_reply_golden_is_usage_only_not_a_text_stream() {
        let decoded = decode_response(EMPTY_REPLY).expect("golden");
        assert!(decoded.text.is_empty());
        assert!(!decoded.has_text_stream());
        assert!(decoded.saw_usage);
        assert!(decoded.tool_calls.is_empty());
        assert!(decoded.is_empty_reply());
        assert_eq!(decoded.error, None);
    }

    #[test]
    fn usage_decodes_all_counters_as_u64() {
        let usage = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .varint(2, 27)
            .varint(3, u64::MAX)
            .varint(4, 560)
            .varint(5, 7318)
            .finish();
        let message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .bytes(7, &usage)
            .finish();
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("decode");
        assert!(decoded.saw_usage);
        assert_eq!(decoded.usage.input, 27);
        assert_eq!(decoded.usage.output, u64::MAX);
        assert_eq!(decoded.usage.cache_write, 560);
        assert_eq!(decoded.usage.cache_read, 7318);
    }

    #[test]
    fn direct_and_nested_end_json_errors_decode() {
        for (json, expected) in [
            (
                br#"{"code":"unauthenticated","message":"bad key"}"#.as_slice(),
                ConnectError {
                    code: "unauthenticated".into(),
                    message: "bad key".into(),
                },
            ),
            (
                br#"{"error":{"code":"resource_exhausted","message":"daily quota used up"}}"#,
                ConnectError {
                    code: "resource_exhausted".into(),
                    message: "daily quota used up".into(),
                },
            ),
        ] {
            let decoded = decode_response(&frame(FLAG_END, json).expect("end")).expect("decode");
            assert_eq!(decoded.error, Some(expected));
            assert!(!decoded.is_empty_reply());
        }
    }

    #[test]
    fn gzipped_end_json_error_decodes_after_inflation() {
        let json = br#"{"error":{"code":"unavailable","message":"try later"}}"#;
        let body = frame(FLAG_GZIP | FLAG_END, &compress(json).expect("gzip")).expect("frame");
        let decoded = decode_response(&body).expect("decode");
        assert_eq!(
            decoded.error,
            Some(ConnectError {
                code: "unavailable".into(),
                message: "try later".into(),
            })
        );
    }

    #[test]
    fn malformed_protobuf_tail_keeps_an_already_decoded_text_delta() {
        let mut message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(3, "prefix")
            .finish();
        message.extend_from_slice(&[(4 << 3) | 2, 20, 0]);
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("partial");
        assert_eq!(decoded.text, ["prefix"]);
    }

    #[test]
    fn truncated_connect_frame_is_an_error() {
        let body = frame(0, b"payload").expect("frame");
        let error = decode_response(&body[..body.len() - 1]).expect_err("truncated");
        assert_eq!(error.message(), "unexpected EOF");
    }

    #[test]
    fn invalid_end_json_is_a_decode_error() {
        let error = decode_response(&frame(FLAG_END, b"not-json").expect("frame"))
            .expect_err("invalid JSON");
        assert!(error.message().contains("invalid Connect end JSON"));
    }

    #[test]
    fn tool_only_response_is_not_misclassified_as_empty() {
        let call = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(1, "call:1")
            .string(2, "lookup")
            .string(3, "{\"x\":")
            .finish();
        let message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .bytes(6, &call)
            .finish();
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("decode");
        assert_eq!(decoded.tool_calls.len(), 1);
        assert_eq!(decoded.tool_calls[0].index, 0);
        assert_eq!(decoded.tool_calls[0].id, "dv_Y2FsbDox");
        assert_eq!(decoded.tool_calls[0].name, "lookup");
        assert_eq!(decoded.tool_arguments[0].arguments, "{\"x\":");
        assert!(!decoded.has_text_stream());
        assert!(!decoded.is_empty_reply());
    }

    #[test]
    fn tool_and_usage_markers_require_length_delimited_fields() {
        let message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .varint(6, 1)
            .varint(7, 1)
            .finish();
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("decode");
        assert!(decoded.tool_calls.is_empty());
        assert!(!decoded.saw_usage);
        assert!(decoded.is_empty_reply());
    }

    #[test]
    fn empty_end_json_is_clean_and_still_empty() {
        let decoded = decode_response(&frame(FLAG_END, b"{}").expect("frame")).expect("decode");
        assert_eq!(decoded.error, None);
        assert!(decoded.is_empty_reply());
    }

    #[test]
    fn tool_ids_round_trip_only_through_the_own_encoding() {
        for (raw, encoded) in [
            ("call:1", "dv_Y2FsbDox"),
            ("#foo", "dv_I2Zvbw"),
            ("dv_", "dv_ZHZf"),
            ("dv_abc", "dv_ZHZfYWJj"),
            ("~~~", "dv_fn5-"),
            ("~~?", "dv_fn4_"),
        ] {
            assert_eq!(super::out_id(raw), encoded);
            assert_eq!(super::in_id(encoded), raw);
        }
        for safe in ["toolu_01AbC", "call_abc-123"] {
            assert_eq!(super::out_id(safe), safe);
            assert_eq!(super::in_id(safe), safe);
        }
        assert_eq!(super::in_id("dv_YQ"), "dv_YQ");
        assert_eq!(super::in_id("dv_!"), "dv_!");
        assert_eq!(super::base64url_decode("A"), None);
    }

    #[test]
    fn an_empty_upstream_tool_id_is_synthesized_as_random_hex() {
        let call = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .string(2, "lookup")
            .finish();
        let message = hofund_plugin_sdk::protobuf::ProtoBuf::new()
            .bytes(6, &call)
            .finish();
        let decoded = decode_response(&frame(0, &message).expect("frame")).expect("decode");
        let id = &decoded.tool_calls[0].id;
        assert_eq!(id.len(), 29);
        assert!(id.starts_with("call_"));
        assert!(id[5..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
