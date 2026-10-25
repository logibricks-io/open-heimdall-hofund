//! Same-protocol passthrough helpers.
//!
//! The gateway forwards raw bytes. It asks the codec to read token usage and
//! to patch the model name, so protocol field names stay in this crate.
//!
//! Model names are rewritten per complete SSE event or JSON value. A quote on
//! a comment line cannot change the next event, and only the codec's model
//! path is replaced.

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
    paths: &'static [&'static [&'static str]],
    model: String,
    pending: Vec<u8>,
    input: Option<i64>,
    output: Option<i64>,
}

impl DecoderTap {
    pub(crate) fn new(
        decoder: Box<dyn StreamDecoder>,
        paths: &'static [&'static [&'static str]],
        model: &str,
    ) -> Self {
        Self {
            decoder,
            paths,
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
        if self.paths.is_empty() {
            return chunk.to_vec();
        }
        self.pending.extend_from_slice(chunk);
        let (ready, consumed) = rewrite_ready(&self.pending, self.paths, &self.model);
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
        if tail.is_empty() || self.paths.is_empty() {
            return tail;
        }
        let mut out = rewrite_all(&tail, self.paths, &self.model);
        terminate_event(&mut out);
        out
    }

    fn tokens(&self) -> (Option<i64>, Option<i64>) {
        (self.input, self.output)
    }
}

pub(crate) fn rewrite_all(bytes: &[u8], paths: &[&[&str]], model: &str) -> Vec<u8> {
    if paths.is_empty() {
        return bytes.to_vec();
    }
    if starts_like_sse(bytes) {
        let (mut out, consumed) = rewrite_ready(bytes, paths, model);
        if consumed < bytes.len() {
            out.extend(rewrite_event(&bytes[consumed..], paths, model));
        }
        return out;
    }
    rewrite_json_document(bytes, paths, model)
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

/// Rewrites complete SSE events and JSON values. The `usize` is how much of
/// `input` was consumed. An unfinished event stays unconsumed.
pub(crate) fn rewrite_ready(input: &[u8], paths: &[&[&str]], model: &str) -> (Vec<u8>, usize) {
    if paths.is_empty() || input.is_empty() {
        return (input.to_vec(), input.len());
    }
    if !starts_like_sse(input) {
        return match json_value_len(input) {
            Some(len) => (rewrite_one_json(&input[..len], paths, model), len),
            None => (Vec::new(), 0),
        };
    }
    let mut out = Vec::new();
    let mut offset = 0;
    while let Some(end) = event_end(&input[offset..]) {
        out.extend(rewrite_event(&input[offset..offset + end], paths, model));
        offset += end;
    }
    (out, offset)
}

fn rewrite_json_document(bytes: &[u8], paths: &[&[&str]], model: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset..].iter().all(|byte| is_ws(*byte)) {
            out.extend_from_slice(&bytes[offset..]);
            break;
        }
        match json_value_len(&bytes[offset..]) {
            Some(len) => {
                out.extend(rewrite_one_json(&bytes[offset..offset + len], paths, model));
                offset += len;
            }
            None => {
                out.extend_from_slice(&bytes[offset..]);
                break;
            }
        }
    }
    out
}

fn rewrite_one_json(bytes: &[u8], paths: &[&[&str]], model: &str) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    if !value.is_object() {
        return bytes.to_vec();
    }
    rewrite_paths(&mut value, paths, model);
    serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
}

fn rewrite_paths(value: &mut serde_json::Value, paths: &[&[&str]], model: &str) {
    for path in paths {
        rewrite_path(value, path, model);
    }
}

fn rewrite_path(value: &mut serde_json::Value, path: &[&str], model: &str) {
    let Some((head, tail)) = path.split_first() else {
        return;
    };
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(child) = object.get_mut(*head) else {
        return;
    };
    if tail.is_empty() {
        if child.is_string() {
            *child = serde_json::Value::String(model.to_owned());
        }
        return;
    }
    rewrite_path(child, tail, model);
}

fn rewrite_event(event: &[u8], paths: &[&[&str]], model: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(event.len());
    let mut start = 0;
    let mut index = 0;
    while index < event.len() {
        if event[index] == b'\n' {
            out.extend(rewrite_line(&event[start..=index], paths, model));
            start = index + 1;
        }
        index += 1;
    }
    if start < event.len() {
        out.extend(rewrite_line(&event[start..], paths, model));
    }
    out
}

fn rewrite_line(line: &[u8], paths: &[&[&str]], model: &str) -> Vec<u8> {
    let (content, ending) = split_ending(line);
    let Some(payload) = data_payload(content) else {
        return line.to_vec();
    };
    let Some(rewritten) = rewrite_payload(payload, paths, model) else {
        return line.to_vec();
    };
    let mut out = Vec::with_capacity(line.len());
    let payload_at = content.len() - payload.len();
    out.extend_from_slice(&content[..payload_at]);
    out.extend(rewritten);
    out.extend_from_slice(ending);
    out
}

fn rewrite_payload(payload: &[u8], paths: &[&[&str]], model: &str) -> Option<Vec<u8>> {
    let trimmed = payload.iter().position(|byte| !is_ws(*byte))?;
    let body = &payload[trimmed..];
    let len = json_value_len(body)?;
    if body[len..].iter().any(|byte| !is_ws(*byte)) {
        return None;
    }
    let mut out = payload[..trimmed].to_vec();
    out.extend(rewrite_one_json(&body[..len], paths, model));
    out.extend_from_slice(&body[len..]);
    Some(out)
}

fn data_payload(content: &[u8]) -> Option<&[u8]> {
    let rest = content.strip_prefix(b"data:")?;
    Some(rest.strip_prefix(b" ").unwrap_or(rest))
}

fn split_ending(line: &[u8]) -> (&[u8], &[u8]) {
    if line.ends_with(b"\r\n") {
        (&line[..line.len() - 2], &line[line.len() - 2..])
    } else if let Some(stripped) = line.strip_suffix(b"\n") {
        (stripped, &line[line.len() - 1..])
    } else {
        (line, &[])
    }
}

fn event_end(input: &[u8]) -> Option<usize> {
    let mut index = 0;
    let mut line_start = 0;
    let mut saw_line = false;
    while index < input.len() {
        if input[index] == b'\n' {
            let mut content_end = index;
            if content_end > line_start && input[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            let end = index + 1;
            if content_end == line_start && saw_line {
                return Some(end);
            }
            saw_line = true;
            line_start = end;
            index = end;
            continue;
        }
        index += 1;
    }
    None
}

fn starts_like_sse(input: &[u8]) -> bool {
    let trimmed = trim_ws(input);
    trimmed.starts_with(b"data:")
        || trimmed.starts_with(b"event:")
        || trimmed.starts_with(b"id:")
        || trimmed.starts_with(b"retry:")
        || trimmed.starts_with(b":")
}

fn trim_ws(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|byte| !is_ws(*byte))
        .unwrap_or(input.len());
    &input[start..]
}

fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn json_value_len(input: &[u8]) -> Option<usize> {
    let mut stream = serde_json::Deserializer::from_slice(input).into_iter::<serde_json::Value>();
    match stream.next() {
        Some(Ok(_)) => Some(stream.byte_offset()),
        _ => None,
    }
}

/// Ends a partial event so the next frame is not glued to it.
fn terminate_event(bytes: &mut Vec<u8>) {
    if bytes.is_empty() || frame_closed(bytes) {
        return;
    }
    let crlf = bytes.windows(2).any(|pair| pair == b"\r\n");
    if crlf {
        if bytes.ends_with(b"\r\n") {
            bytes.extend_from_slice(b"\r\n");
        } else {
            bytes.extend_from_slice(b"\r\n\r\n");
        }
    } else if bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    } else {
        bytes.extend_from_slice(b"\n\n");
    }
}

pub(crate) fn frame_closed(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\n\n") || bytes.ends_with(b"\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::rewrite_all;
    use crate::{
        AnthropicMessages, Codec, Converse, GenerativeLanguage, OpenAiChat, OpenAiResponses,
    };

    #[test]
    fn a_split_json_object_is_rewritten_when_it_completes() {
        let paths: &[&[&str]] = &[&["model"]];
        let head = br#"{"model":"up"#;
        let (first, consumed) = super::rewrite_ready(head, paths, "demo");
        assert!(first.is_empty(), "{first:?}");
        assert_eq!(consumed, 0);
        let mut pending = head.to_vec();
        pending.extend_from_slice(br#"stream","n":1}"#);
        let text = String::from_utf8(rewrite_all(&pending, paths, "demo")).expect("utf-8");
        assert_eq!(text, r#"{"model":"demo","n":1}"#);
    }

    #[test]
    fn a_stray_quote_on_a_comment_does_not_leak_the_upstream_model() {
        let paths: &[&[&str]] = &[&["model"]];
        for prefix in [": keep-alive \"\n\n", "data: upstream said \"hello\n\n"] {
            let body = format!(
                "{prefix}data: {{\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"model\":\"upstream-x\",\"choices\":[]}}\n\n"
            );
            let text =
                String::from_utf8(rewrite_all(body.as_bytes(), paths, "demo")).expect("utf-8");
            assert!(text.contains("\"model\":\"demo\""), "{prefix}: {text}");
            assert!(!text.contains("upstream-x"), "{prefix}: {text}");
            assert!(text.contains(prefix.trim_end()), "{prefix}");
        }
    }

    #[test]
    fn tool_arguments_keep_their_own_model_field() {
        let anthropic = br#"{"id":"m","type":"message","role":"assistant","model":"upstream-x","content":[{"type":"tool_use","id":"t","name":"car","input":{"model":"tesla-3"}}],"stop_reason":"end_turn"}"#;
        let anthropic_text =
            String::from_utf8(AnthropicMessages.rewrite_model(anthropic, "demo")).expect("utf-8");
        assert!(
            anthropic_text.contains("\"model\":\"demo\""),
            "{anthropic_text}"
        );
        assert!(
            anthropic_text.contains("\"model\":\"tesla-3\""),
            "{anthropic_text}"
        );
        assert!(!anthropic_text.contains("upstream-x"), "{anthropic_text}");

        let nested = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"upstream-x\",",
            "\"content\":[{\"type\":\"tool_use\",\"input\":{\"model\":\"tesla-3\"}}]}}\n\n",
        );
        let mut tap = AnthropicMessages.raw_tap("demo");
        let mut forwarded = tap.push(nested.as_bytes());
        forwarded.extend(tap.finish());
        let text = String::from_utf8(forwarded).expect("utf-8");
        assert!(text.contains("\"model\":\"demo\""), "{text}");
        assert!(text.contains("\"model\":\"tesla-3\""), "{text}");
        assert!(!text.contains("upstream-x"), "{text}");

        let gemini = br#"{"responseId":"g","modelVersion":"upstream-x","candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"car","args":{"model":"tesla-3"}}}]}}]}"#;
        let gemini_text =
            String::from_utf8(GenerativeLanguage.rewrite_model(gemini, "demo")).expect("utf-8");
        assert!(
            gemini_text.contains("\"modelVersion\":\"demo\""),
            "{gemini_text}"
        );
        assert!(
            gemini_text.contains("\"model\":\"tesla-3\""),
            "{gemini_text}"
        );
        assert!(!gemini_text.contains("upstream-x"), "{gemini_text}");
    }

    #[test]
    fn a_partial_event_is_terminated_when_the_tap_finishes() {
        let mut tap = OpenAiChat.raw_tap("demo");
        assert!(tap.push(br#"data: {"model":"upstream"#).is_empty());
        let tail = String::from_utf8(tap.finish()).expect("utf-8");
        assert_eq!(tail, "data: {\"model\":\"upstream\n\n");
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
