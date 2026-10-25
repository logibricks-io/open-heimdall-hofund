//! Same-protocol passthrough helpers.
//!
//! The gateway forwards raw bytes. It asks the codec to read token usage and
//! to patch the model name, so protocol field names stay in this crate.

use hofund_ir::{StreamEvent, Usage};

use crate::StreamDecoder;

/// Incremental read of a raw response body.
///
/// `push` returns the bytes the client should see. A chunk may end inside a
/// frame or a model string; the unfinished tail is held until a later `push`
/// or `finish`.
pub trait RawTap: Send {
    /// Observes `chunk` and returns the bytes to forward.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8>;

    /// Flushes a trailing partial frame and any unfinished model field.
    fn finish(&mut self) -> Vec<u8>;

    /// Input and output tokens seen so far.
    fn tokens(&self) -> (Option<i64>, Option<i64>);
}

pub(crate) struct DecoderTap {
    decoder: Box<dyn StreamDecoder>,
    fields: &'static [&'static str],
    model: String,
    pending: Vec<u8>,
    input: Option<i64>,
    output: Option<i64>,
}

impl DecoderTap {
    pub(crate) fn new(
        decoder: Box<dyn StreamDecoder>,
        fields: &'static [&'static str],
        model: &str,
    ) -> Self {
        Self {
            decoder,
            fields,
            model: model.to_owned(),
            pending: Vec::new(),
            input: None,
            output: None,
        }
    }

    fn note(&mut self, events: &[StreamEvent]) {
        for event in events {
            let usage = match event {
                StreamEvent::MessageStart(start) => start.usage.as_ref(),
                StreamEvent::MessageDelta(delta) => delta.usage.as_ref(),
                _ => None,
            };
            note_usage(usage, &mut self.input, &mut self.output);
        }
    }

    fn emit(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let (ready, consumed) = rewrite_ready(&self.pending, self.fields, &self.model);
        self.pending.drain(..consumed);
        ready
    }
}

impl RawTap for DecoderTap {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if let Ok(events) = self.decoder.push(chunk) {
            self.note(&events);
        }
        self.emit(chunk)
    }

    fn finish(&mut self) -> Vec<u8> {
        if let Ok(events) = self.decoder.finish() {
            self.note(&events);
        }
        let tail = std::mem::take(&mut self.pending);
        rewrite_all(&tail, self.fields, &self.model)
    }

    fn tokens(&self) -> (Option<i64>, Option<i64>) {
        (self.input, self.output)
    }
}

pub(crate) fn rewrite_all(bytes: &[u8], fields: &[&str], model: &str) -> Vec<u8> {
    let (mut out, consumed) = rewrite_ready(bytes, fields, model);
    if consumed < bytes.len() {
        out.extend_from_slice(&bytes[consumed..]);
    }
    out
}

fn note_usage(usage: Option<&Usage>, input: &mut Option<i64>, output: &mut Option<i64>) {
    let Some(usage) = usage else {
        return;
    };
    if let Some(value) = as_i64(usage.input_tokens) {
        *input = Some(value);
    }
    if let Some(value) = as_i64(usage.output_tokens) {
        *output = Some(value);
    }
}

fn as_i64(value: Option<u64>) -> Option<i64> {
    value.and_then(|value| i64::try_from(value).ok())
}

/// Rewrites complete `"field":"..."` values. The `usize` is how much of
/// `input` was consumed; the rest may be the start of a model field.
pub(crate) fn rewrite_ready(input: &[u8], fields: &[&str], model: &str) -> (Vec<u8>, usize) {
    let replacement = serde_json::to_string(model).unwrap_or_else(|_| "\"\"".to_owned());
    let mut out = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'"' {
            out.push(input[index]);
            index += 1;
            continue;
        }
        let Some(key) = json_string(input, index) else {
            return (out, index);
        };
        match value_string(input, key.end) {
            Look::Incomplete => return (out, index),
            Look::NotValue => {
                out.extend_from_slice(&input[index..key.end]);
                index = key.end;
            }
            Look::Value { start, end } => {
                if fields.iter().any(|field| *field == key.text) {
                    out.extend_from_slice(&input[index..start]);
                    out.extend_from_slice(replacement.as_bytes());
                } else {
                    out.extend_from_slice(&input[index..end]);
                }
                index = end;
            }
        }
    }
    (out, input.len())
}

struct JsonString {
    text: String,
    end: usize,
}

fn json_string(input: &[u8], start: usize) -> Option<JsonString> {
    if input.get(start) != Some(&b'"') {
        return None;
    }
    let mut text = String::new();
    let mut index = start + 1;
    let mut escape = false;
    while index < input.len() {
        let byte = input[index];
        if escape {
            text.push(char::from(byte));
            escape = false;
            index += 1;
            continue;
        }
        if byte == b'\\' {
            escape = true;
            index += 1;
            continue;
        }
        if byte == b'"' {
            return Some(JsonString {
                text,
                end: index + 1,
            });
        }
        text.push(char::from(byte));
        index += 1;
    }
    None
}

enum Look {
    Incomplete,
    NotValue,
    Value { start: usize, end: usize },
}

fn value_string(input: &[u8], after_key: usize) -> Look {
    let index = skip_ws(input, after_key);
    if index >= input.len() {
        return Look::Incomplete;
    }
    if input[index] != b':' {
        return Look::NotValue;
    }
    let index = skip_ws(input, index + 1);
    if index >= input.len() {
        return Look::Incomplete;
    }
    if input[index] != b'"' {
        return Look::NotValue;
    }
    match json_string(input, index) {
        Some(value) => Look::Value {
            start: index,
            end: value.end,
        },
        None => Look::Incomplete,
    }
}

fn skip_ws(input: &[u8], mut index: usize) -> usize {
    while index < input.len() && matches!(input[index], b' ' | b'\t' | b'\n' | b'\r') {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::rewrite_all;
    use crate::{
        AnthropicMessages, Codec, Converse, GenerativeLanguage, OpenAiChat, OpenAiResponses,
    };

    #[test]
    fn a_split_model_field_is_rewritten_without_the_rest_of_the_body() {
        let fields = ["model"];
        let head = br#"{"model":"up"#;
        let (first, consumed) = super::rewrite_ready(head, &fields, "demo");
        assert_eq!(first, b"{");
        assert_eq!(consumed, 1);
        let mut pending = head[consumed..].to_vec();
        pending.extend_from_slice(br#"stream","n":1}"#);
        let mut rewritten = first;
        rewritten.extend(rewrite_all(&pending, &fields, "demo"));
        let text = String::from_utf8(rewritten).expect("utf-8");
        assert_eq!(text, r#"{"model":"demo","n":1}"#);
    }

    #[test]
    fn each_protocol_reports_stream_and_unary_usage_across_a_split_frame() {
        let chat = concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
            "\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
        );
        assert_tokens(&OpenAiChat, chat.as_bytes(), chat_unary());
        let anthropic = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",",
            "\"role\":\"assistant\",\"model\":\"upstream\",\"content\":[],",
            "\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},",
            "\"usage\":{\"output_tokens\":7}}\n\n",
        );
        assert_tokens(
            &AnthropicMessages,
            anthropic.as_bytes(),
            br#"{"id":"m","type":"message","role":"assistant","model":"upstream","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":11,"output_tokens":7}}"#,
        );
        let responses = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"upstream\",",
            "\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n\n",
        );
        assert_tokens(
            &OpenAiResponses,
            responses.as_bytes(),
            br#"{"id":"r","model":"upstream","status":"completed","output":[],"usage":{"input_tokens":11,"output_tokens":7}}"#,
        );
        let gemini = concat!(
            "data: {\"responseId\":\"g\",\"modelVersion\":\"upstream\",",
            "\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":7}}\n\n",
        );
        assert_tokens(
            &GenerativeLanguage,
            gemini.as_bytes(),
            br#"{"responseId":"g","modelVersion":"upstream","candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7}}"#,
        );
    }

    fn assert_tokens(codec: &dyn Codec, stream: &[u8], unary: &[u8]) {
        let mut tap = codec.raw_tap("demo");
        let mut forwarded = Vec::new();
        for byte in stream {
            forwarded.extend(tap.push(std::slice::from_ref(byte)));
        }
        forwarded.extend(tap.finish());
        assert_eq!(tap.tokens(), (Some(11), Some(7)), "{}", codec.id());
        let text = String::from_utf8_lossy(&forwarded);
        assert!(
            text.contains("demo") && !text.contains("upstream"),
            "{} model rewrite: {text}",
            codec.id()
        );
        assert_eq!(
            codec.response_tokens(unary),
            (Some(11), Some(7)),
            "{}",
            codec.id()
        );
        let rewritten = codec.rewrite_model(unary, "demo");
        let unary_text = String::from_utf8(rewritten).expect("utf-8");
        assert!(
            unary_text.contains("demo") && !unary_text.contains("upstream"),
            "{} unary model: {unary_text}",
            codec.id()
        );
    }

    #[test]
    fn a_usage_frame_split_across_two_chunks_is_counted_once_it_completes() {
        let frame = concat!(
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream\",",
            "\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":7}}\n\n",
        );
        let mut tap = OpenAiChat.raw_tap("demo");
        let split = frame.find("prompt_tokens").expect("field");
        let bytes = frame.as_bytes();
        let _ = tap.push(&bytes[..split]);
        assert_eq!(tap.tokens(), (None, None));
        let _ = tap.push(&bytes[split..]);
        let _ = tap.finish();
        assert_eq!(tap.tokens(), (Some(11), Some(7)));
    }

    fn chat_unary() -> &'static [u8] {
        br#"{"id":"c","object":"chat.completion","model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#
    }

    #[test]
    fn stream_error_frames_match_the_protocol() {
        let responses = String::from_utf8(OpenAiResponses.encode_stream_error(502, "overloaded"))
            .expect("utf-8");
        assert!(responses.starts_with("event: error\ndata: "), "{responses}");
        assert!(responses.contains("\"type\":\"error\""), "{responses}");
        assert!(responses.contains("\"code\":502"), "{responses}");
        assert!(
            responses.contains("\"message\":\"overloaded\""),
            "{responses}"
        );
        assert!(responses.ends_with("\n\n"), "{responses}");

        let chat = String::from_utf8(OpenAiChat.encode_stream_error(500, "nope")).expect("utf-8");
        assert!(chat.starts_with("data: {\"error\":"), "{chat}");
        assert!(!chat.contains("event:"), "{chat}");

        let gemini =
            String::from_utf8(GenerativeLanguage.encode_stream_error(503, "later")).expect("utf-8");
        assert!(gemini.starts_with("data: {\"error\":"), "{gemini}");
        assert!(!gemini.contains("event:"), "{gemini}");

        let anthropic =
            String::from_utf8(AnthropicMessages.encode_stream_error(429, "slow")).expect("utf-8");
        assert!(anthropic.starts_with("event: error\ndata: "), "{anthropic}");
    }

    #[test]
    fn stream_content_type_follows_the_protocol() {
        assert_eq!(OpenAiChat.stream_content_type(), "text/event-stream");
        assert_eq!(
            GenerativeLanguage.stream_content_type(),
            "text/event-stream"
        );
        assert_eq!(OpenAiResponses.stream_content_type(), "text/event-stream");
        assert_eq!(AnthropicMessages.stream_content_type(), "text/event-stream");
        assert_eq!(
            Converse.stream_content_type(),
            "application/vnd.amazon.eventstream"
        );
        assert_eq!(Converse.response_content_type(false), "application/json");
    }
}
