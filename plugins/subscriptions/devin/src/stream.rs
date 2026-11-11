//! Incremental GetChatMessage stream conversion.

use hofund_plugin_api::{Body, BodyStream, Completion, PluginError, ProtocolId};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::transport::HttpResponse;
use hofund_plugin_sdk::{uuid, wire};

use crate::connect::chat::{ChatDecoder, ChatEvent};
use crate::errors;

pub(crate) const TOOLS_MESSAGE: &str = "tool calls and results are not served on Devin until R-13";
pub(crate) const EMPTY_REPLY_MESSAGE: &str =
    "an empty reply (the model answered nothing; try again)";

pub(crate) async fn open(
    body: BodyStream,
    requested_model: String,
    inbound: ProtocolId,
    client_stream: bool,
) -> Result<Completion, PluginError> {
    let mut reader = Reader::new(body);
    let mut saw_tool_call = false;
    let mut stop = None;
    let first = loop {
        match reader.next_event().await? {
            Some(event @ (ChatEvent::Text(_) | ChatEvent::Reasoning(_))) => break event,
            Some(ChatEvent::ToolCall) => saw_tool_call = true,
            Some(ChatEvent::Stop(reason)) => stop = Some(reason),
            Some(ChatEvent::Usage) => {}
            Some(ChatEvent::End(Some(error))) => return Err(errors::connect(&error)),
            Some(ChatEvent::End(None)) => return Err(empty_or_tools(saw_tool_call)),
            None if saw_tool_call => return Err(tools_refusal()),
            None if reader.decoder.saw_frame() => return Err(empty_reply()),
            None => return Err(broken("EOF")),
        }
    };
    let id = uuid::v4().map_err(|error| PluginError::io(error.to_string()))?;
    let mut initial = start_chunk(&id, &requested_model);
    initial.extend(delta_chunk(&id, &requested_model, &first));
    let state = StreamState {
        reader,
        id,
        model: requested_model,
        stop,
        finished: false,
    };
    let rest = BodyStream::unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        loop {
            let event = match state.reader.next_event().await {
                Ok(event) => event,
                Err(error) => {
                    state.finished = true;
                    return Some((Err(error), state));
                }
            };
            match event {
                Some(ChatEvent::Text(_) | ChatEvent::Reasoning(_)) => {
                    let event = event.expect("content was just matched");
                    let chunk = delta_chunk(&state.id, &state.model, &event);
                    return Some((Ok(chunk), state));
                }
                Some(ChatEvent::Stop(reason)) => state.stop = Some(reason),
                Some(ChatEvent::End(Some(error))) => {
                    state.finished = true;
                    return Some((Err(errors::connect(&error)), state));
                }
                Some(ChatEvent::End(None)) | None => {
                    let chunk = finish_chunk(&state.id, &state.model, state.stop);
                    state.finished = true;
                    return Some((Ok(chunk), state));
                }
                Some(ChatEvent::ToolCall | ChatEvent::Usage) => {}
            }
        }
    });
    let body = BodyStream::resume(Body::from_bytes(initial).bytes().clone(), rest);
    let decode_inbound = if inbound == ProtocolId::OPENAI_CHAT {
        ProtocolId::OPENAI_RESPONSES
    } else {
        inbound
    };
    wire::decode_upstream_response(
        HttpResponse::new(body),
        ProtocolId::OPENAI_CHAT,
        decode_inbound,
        client_stream,
    )
    .await
}

struct Reader {
    body: BodyStream,
    decoder: ChatDecoder,
    finished: bool,
}

impl Reader {
    fn new(body: BodyStream) -> Self {
        Self {
            body,
            decoder: ChatDecoder::default(),
            finished: false,
        }
    }

    async fn next_event(&mut self) -> Result<Option<ChatEvent>, PluginError> {
        loop {
            if let Some(event) = self.decoder.next_event() {
                return Ok(Some(event));
            }
            if self.finished {
                return Ok(None);
            }
            match self.body.next_chunk().await {
                Some(Ok(chunk)) => self.decoder.push(&chunk).map_err(broken)?,
                Some(Err(error)) => return Err(broken(error.message())),
                None => {
                    self.decoder.finish().map_err(broken)?;
                    self.finished = true;
                }
            }
        }
    }
}

struct StreamState {
    reader: Reader,
    id: String,
    model: String,
    stop: Option<u64>,
    finished: bool,
}

fn empty_or_tools(saw_tool_call: bool) -> PluginError {
    if saw_tool_call {
        tools_refusal()
    } else {
        empty_reply()
    }
}

pub(crate) fn tools_refusal() -> PluginError {
    PluginError::status_error(404, TOOLS_MESSAGE).without_cooldown()
}

fn empty_reply() -> PluginError {
    PluginError::status_error(502, EMPTY_REPLY_MESSAGE)
}

fn broken(detail: impl std::fmt::Display) -> PluginError {
    PluginError::status_error(502, format!("the reply broke off: {detail}"))
}

fn start_chunk(id: &str, model: &str) -> Vec<u8> {
    sse(json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant"},
            "finish_reason": Value::Null,
        }],
    }))
}

fn delta_chunk(id: &str, model: &str, event: &ChatEvent) -> Vec<u8> {
    let delta = match event {
        ChatEvent::Text(text) => json!({"content": text}),
        ChatEvent::Reasoning(text) => json!({"reasoning_content": text}),
        _ => unreachable!("only content deltas are encoded"),
    };
    sse(json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": Value::Null,
        }],
    }))
}

fn finish_chunk(id: &str, model: &str, stop: Option<u64>) -> Vec<u8> {
    let reason = if stop == Some(3) { "length" } else { "stop" };
    let mut out = sse(json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": reason,
        }],
    }));
    out.extend_from_slice(b"data: [DONE]\n\n");
    out
}

fn sse(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}
