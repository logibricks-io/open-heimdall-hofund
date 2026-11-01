use std::collections::{BTreeSet, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use futures_util::{Stream, StreamExt, future::BoxFuture};
use hofund_codec::{Codec, StreamEncoder, refusal_error};
use hofund_ir::{BlockStart, Request, StopReason, StreamEvent};
use hofund_plugin_api::{BodyStream, Completion, PluginError, ProtocolId};
use hofund_router::{RouteContinuation, Routed, RouterError};

use super::{ErrorFrame, RecordOnEnd, finish_record, frame_closed, note_usage};

pub(super) type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, PluginError>> + Send>>;

pub(super) fn events(
    completion: Completion,
    codec: &Arc<dyn Codec>,
    model: &str,
) -> Result<EventStream, PluginError> {
    match completion {
        Completion::Stream(events) => Ok(events),
        Completion::Raw(body) => Ok(decode_raw(body, codec, model)),
        _ => Err(PluginError::status_error(
            502,
            "the member answered a streamed request with a whole reply",
        )
        .without_cooldown()),
    }
}

fn decode_raw(body: BodyStream, codec: &Arc<dyn Codec>, model: &str) -> EventStream {
    let decoder = codec.stream_decoder();
    let model = model.to_owned();
    Box::pin(futures_util::stream::unfold(
        (body, decoder, VecDeque::new(), false),
        move |(mut body, mut decoder, mut pending, mut finished)| {
            let model = model.clone();
            async move {
                loop {
                    if let Some(event) = pending.pop_front() {
                        return Some((event, (body, decoder, pending, finished)));
                    }
                    if finished {
                        return None;
                    }
                    match body.next_chunk().await {
                        None => {
                            finished = true;
                            match decoder.finish() {
                                Ok(events) => pending.extend(
                                    events.into_iter().map(|event| Ok(event.with_model(&model))),
                                ),
                                Err(error) => {
                                    pending.push_back(Err(PluginError::decode(error.to_string())))
                                }
                            }
                        }
                        Some(Err(error)) => {
                            finished = true;
                            pending.push_back(Err(error));
                        }
                        Some(Ok(chunk)) => match decoder.push(&chunk) {
                            Ok(events) => pending.extend(
                                events.into_iter().map(|event| Ok(event.with_model(&model))),
                            ),
                            Err(error) => {
                                finished = true;
                                pending.push_back(Err(PluginError::decode(error.to_string())));
                            }
                        },
                    }
                }
            }
        },
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Thinking,
    Lead,
    Content,
}

struct Tracker {
    indexed: bool,
    responses: bool,
    passing: bool,
    thought: bool,
    spoke: bool,
    spliced: bool,
    offset: u32,
    blocks: u32,
    open: BTreeSet<u32>,
}

impl Tracker {
    fn new(codec: &dyn Codec) -> Self {
        let responses = codec.id() == ProtocolId::OPENAI_RESPONSES.as_str();
        Self {
            indexed: responses || codec.id() == ProtocolId::ANTHROPIC_MESSAGES.as_str(),
            responses,
            passing: false,
            thought: false,
            spoke: false,
            spliced: false,
            offset: 0,
            blocks: 0,
            open: BTreeSet::new(),
        }
    }

    fn kind(&self, event: &StreamEvent) -> Kind {
        match event {
            StreamEvent::ContentBlockStart(start) => match &start.block {
                BlockStart::Thinking { .. } => Kind::Thinking,
                BlockStart::Text => Kind::Lead,
                BlockStart::ToolCall { .. } | BlockStart::Unknown { .. } => Kind::Content,
            },
            StreamEvent::ThinkingDelta(delta) if !delta.text.is_empty() => Kind::Thinking,
            StreamEvent::SignatureDelta(delta) if !delta.signature.is_empty() => Kind::Thinking,
            StreamEvent::TextDelta(delta) if !delta.text.is_empty() => Kind::Content,
            StreamEvent::ToolArgumentsDelta(_) => Kind::Content,
            StreamEvent::MessageDelta(delta) if delta.stop.is_some() => Kind::Content,
            StreamEvent::MessageStop(_) => Kind::Content,
            StreamEvent::Unknown(event) => unknown_kind(event),
            _ => Kind::Lead,
        }
    }

    fn prepare(&self, event: StreamEvent) -> Option<StreamEvent> {
        if self.spliced && matches!(event, StreamEvent::MessageStart(_)) && self.indexed {
            return None;
        }
        if self.spliced
            && self.responses
            && let StreamEvent::Unknown(unknown) = &event
            && matches!(
                unknown.name.as_str(),
                "response.created" | "response.in_progress" | "response.queued"
            )
        {
            return None;
        }
        if !self.indexed || self.offset == 0 {
            return Some(event);
        }
        Some(shift(event, self.offset))
    }

    fn track(&mut self, event: &StreamEvent) {
        if !self.indexed {
            return;
        }
        match event {
            StreamEvent::ContentBlockStart(start) => {
                self.blocks = self.blocks.max(start.index.saturating_add(1));
                self.open.insert(start.index);
            }
            StreamEvent::ContentBlockStop(stop) => {
                self.open.remove(&stop.index);
            }
            _ => {}
        }
    }

    fn close_open(&mut self) -> Vec<StreamEvent> {
        self.open
            .iter()
            .map(|index| {
                StreamEvent::ContentBlockStop(hofund_ir::ContentBlockStop {
                    index: *index,
                    extensions: hofund_ir::Extensions::new(),
                })
            })
            .collect()
    }
}

fn unknown_kind(event: &hofund_ir::UnknownEvent) -> Kind {
    let name = event.name.as_str();
    if matches!(
        name,
        "ping"
            | "message_start"
            | "response.created"
            | "response.in_progress"
            | "response.queued"
            | "response.metadata"
            | "content_block_stop"
            | "response.content_part.done"
    ) || extension_event(name)
    {
        Kind::Lead
    } else {
        Kind::Content
    }
}

/// A client-extension event (the namespace of
/// [`hofund_plugin_api::responses_ws::RATE_LIMITS_EVENT`]) carries no reply
/// content, so it never counts as having spoken.
fn extension_event(name: &str) -> bool {
    hofund_plugin_api::responses_ws::RATE_LIMITS_EVENT
        .split_once('.')
        .is_some_and(|(namespace, _)| {
            name.strip_prefix(namespace)
                .is_some_and(|rest| rest.starts_with('.'))
        })
}

fn shift(event: StreamEvent, offset: u32) -> StreamEvent {
    match event {
        StreamEvent::ContentBlockStart(mut event) => {
            event.index = event.index.saturating_add(offset);
            StreamEvent::ContentBlockStart(event)
        }
        StreamEvent::TextDelta(mut event) => {
            event.index = event.index.saturating_add(offset);
            StreamEvent::TextDelta(event)
        }
        StreamEvent::ThinkingDelta(mut event) => {
            event.index = event.index.saturating_add(offset);
            StreamEvent::ThinkingDelta(event)
        }
        StreamEvent::SignatureDelta(mut event) => {
            event.index = event.index.saturating_add(offset);
            StreamEvent::SignatureDelta(event)
        }
        StreamEvent::ToolArgumentsDelta(mut event) => {
            event.index = event.index.saturating_add(offset);
            StreamEvent::ToolArgumentsDelta(event)
        }
        StreamEvent::ContentBlockStop(mut event) => {
            event.index = event.index.saturating_add(offset);
            StreamEvent::ContentBlockStop(event)
        }
        other => other,
    }
}

/// The error a committed refusal event is when it is not handed over.
///
/// A provider passes a refusal error that follows shown thinking through as
/// an event so a gateway with another member can hand it on. Without one, or
/// once the answer began, the client gets the protocol's own error frame, as
/// before, never the upstream's raw payload. The status follows the upstream
/// error-word rule: invalid or too-long words are 400, the rest 502.
pub(super) fn refusal_failure(event: &StreamEvent) -> Option<PluginError> {
    if !refusal_error(event) {
        return None;
    }
    let StreamEvent::Unknown(unknown) = event else {
        return None;
    };
    let message = ["/error/message", "/message", "/response/error/message"]
        .into_iter()
        .find_map(|path| {
            unknown
                .payload
                .pointer(path)
                .and_then(|value| value.as_str())
        })
        .unwrap_or("upstream stream failed")
        .to_owned();
    let text = unknown.payload.to_string().to_lowercase();
    let status =
        if text.contains("invalid") || text.contains("context_length") || text.contains("too long")
        {
            400
        } else {
            502
        };
    Some(PluginError::status_error(status, message).without_cooldown())
}

/// Turns committed refusal events into errors for a stream nobody takes over.
pub(super) fn refusals_fail(incoming: EventStream) -> EventStream {
    Box::pin(incoming.map(|item| match item {
        Ok(event) => match refusal_failure(&event) {
            Some(error) => Err(error),
            None => Ok(event),
        },
        Err(error) => Err(error),
    }))
}

fn refused(event: &StreamEvent) -> bool {
    refusal_error(event)
        || matches!(
            event,
            StreamEvent::MessageDelta(delta)
                if delta.stop.as_ref().is_some_and(|stop| matches!(
                    &stop.reason,
                    StopReason::Refusal | StopReason::ContentFilter
                ))
        )
}

struct State {
    incoming: EventStream,
    continuation: Option<RouteContinuation>,
    codec: Arc<dyn Codec>,
    request: Request,
    encoder: Box<dyn StreamEncoder>,
    record: RecordOnEnd,
    on_error: ErrorFrame,
    tracker: Tracker,
    held: Vec<StreamEvent>,
    ready: Vec<u8>,
    boundary: bool,
    phase: u8,
    ready_future: Option<BoxFuture<'static, Result<Routed, RouterError>>>,
}

impl State {
    fn encode(&mut self, event: &StreamEvent) {
        note_usage(event, &mut self.record.input, &mut self.record.output);
        match self.encoder.push(event) {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    self.boundary = frame_closed(&bytes);
                    self.ready.extend(bytes);
                }
            }
            Err(_) => {
                self.phase = 3;
            }
        }
    }

    fn flush_held(&mut self) {
        let held = std::mem::take(&mut self.held);
        for event in held {
            self.tracker.track(&event);
            self.encode(&event);
        }
    }

    fn fail(&mut self, error: &PluginError) {
        let mut bytes = (self.on_error)(error);
        if !self.boundary {
            let mut prefixed = b"\n\n".to_vec();
            prefixed.extend(bytes);
            bytes = prefixed;
        }
        self.ready.extend(bytes);
        self.phase = 3;
    }

    fn resume(&mut self, continuation: RouteContinuation) {
        let future = continuation.dispatch();
        self.phase = 4;
        self.ready_future = Some(Box::pin(future));
    }

    async fn next(mut self) -> Option<(Result<Bytes, std::io::Error>, Self)> {
        loop {
            if !self.ready.is_empty() {
                let bytes = std::mem::take(&mut self.ready);
                return Some((Ok(Bytes::from(bytes)), self));
            }
            match self.phase {
                1 => {
                    finish_record(&mut self.record, super::StatusCode::OK);
                    self.phase = 2;
                    return None;
                }
                2 => return None,
                3 => {
                    finish_record(&mut self.record, super::StatusCode::BAD_GATEWAY);
                    self.phase = 2;
                    return None;
                }
                4 => {
                    let result = self.ready_future.take().expect("dispatch future").await;
                    self.phase = 0;
                    match result {
                        Ok(mut routed) => {
                            self.record.meta.upstream = routed.upstream_id.clone();
                            self.record.member = Some(routed.upstream_id.clone());
                            self.continuation = routed.take_continuation();
                            match events(routed.completion, &self.codec, &self.request.model) {
                                Ok(events) => self.incoming = events,
                                Err(error) => self.fail(&error),
                            }
                        }
                        Err(error) => self.fail(&continuation_error(error)),
                    }
                    continue;
                }
                _ => {}
            }
            match self.incoming.next().await {
                None => match self.encoder.finish() {
                    Ok(bytes) if bytes.is_empty() => {
                        finish_record(&mut self.record, super::StatusCode::OK);
                        return None;
                    }
                    Ok(bytes) => {
                        self.boundary = frame_closed(&bytes);
                        self.phase = 1;
                        return Some((Ok(Bytes::from(bytes)), self));
                    }
                    Err(_) => {
                        finish_record(&mut self.record, super::StatusCode::BAD_GATEWAY);
                        return None;
                    }
                },
                Some(Err(error)) => {
                    self.fail(&error);
                }
                Some(Ok(event)) => {
                    if (self.tracker.spoke || self.continuation.is_none())
                        && let Some(error) = refusal_failure(&event)
                    {
                        self.flush_held();
                        self.fail(&error);
                        continue;
                    }
                    if !self.tracker.spoke && refused(&event) {
                        let Some(continuation) = self.continuation.take() else {
                            self.flush_held();
                            self.encode(&event);
                            continue;
                        };
                        self.held.clear();
                        let open = self.tracker.close_open();
                        self.tracker.open.clear();
                        for event in open {
                            self.encode(&event);
                        }
                        self.tracker.passing = false;
                        self.tracker.spliced = self.tracker.thought;
                        self.tracker.offset = if self.tracker.thought {
                            self.tracker.blocks
                        } else {
                            0
                        };
                        self.resume(continuation);
                        continue;
                    }
                    let kind = self.tracker.kind(&event);
                    if kind == Kind::Content {
                        self.flush_held();
                        self.tracker.spoke = true;
                    } else if kind == Kind::Thinking && !self.tracker.passing {
                        self.flush_held();
                        self.tracker.passing = true;
                        self.tracker.thought = true;
                    } else if !self.tracker.passing {
                        if let Some(event) = self.tracker.prepare(event) {
                            self.held.push(event);
                        }
                        continue;
                    }
                    let Some(event) = self.tracker.prepare(event) else {
                        continue;
                    };
                    self.tracker.track(&event);
                    self.encode(&event);
                }
            }
            if self.phase == 3 && self.ready.is_empty() {
                continue;
            }
        }
    }
}

fn continuation_error(error: RouterError) -> PluginError {
    match error {
        RouterError::NoRoute { .. } => PluginError::status_error(404, error.to_string()),
        RouterError::Ambiguous { .. } | RouterError::PreviousResponseNotFound { .. } => {
            PluginError::status_error(400, error.to_string())
        }
        RouterError::ModelScope { .. } | RouterError::AccountScope { .. } => {
            PluginError::status_error(403, error.to_string())
        }
        RouterError::Upstream { error, .. } => error,
    }
}

pub(super) fn body(
    incoming: EventStream,
    continuation: RouteContinuation,
    codec: Arc<dyn Codec>,
    request: Request,
    encoder: Box<dyn StreamEncoder>,
    record: RecordOnEnd,
    on_error: ErrorFrame,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    let tracker = Tracker::new(codec.as_ref());
    futures_util::stream::unfold(
        State {
            incoming,
            continuation: Some(continuation),
            codec,
            request,
            encoder,
            record,
            on_error,
            tracker,
            held: Vec::new(),
            ready: Vec::new(),
            boundary: true,
            phase: 0,
            ready_future: None,
        },
        State::next,
    )
}

#[cfg(test)]
mod tests {
    use super::{extension_event, refusal_failure};
    use hofund_ir::{StreamEvent, UnknownEvent};
    use hofund_plugin_api::responses_ws::RATE_LIMITS_EVENT;

    fn refusal(message: &str) -> StreamEvent {
        StreamEvent::Unknown(UnknownEvent {
            name: "error".to_owned(),
            payload: serde_json::json!({"error":{"code":"content_filter","message":message}}),
        })
    }

    #[test]
    fn b9v_refusal_failure_status_follows_the_error_words() {
        let status = |message: &str| refusal_failure(&refusal(message)).and_then(|e| e.status());
        assert_eq!(status("refused"), Some(502));
        assert_eq!(status("invalid content"), Some(400));
        assert_eq!(status("prompt is too long"), Some(400));
        assert_eq!(status("context_length exceeded"), Some(400));
        assert!(
            refusal_failure(&StreamEvent::Unknown(UnknownEvent {
                name: "error".to_owned(),
                payload: serde_json::json!({"error":{"code":"overloaded","message":"busy"}}),
            }))
            .is_none()
        );
    }

    #[test]
    fn b9p_client_extension_events_are_not_reply_content() {
        let namespace = RATE_LIMITS_EVENT.split_once('.').map(|(head, _)| head);
        let namespace = namespace.unwrap_or_default();
        assert!(extension_event(RATE_LIMITS_EVENT));
        assert!(extension_event(&format!("{namespace}.other")));
        assert!(!extension_event(namespace));
        assert!(!extension_event(&format!("{namespace}x.other")));
        assert!(!extension_event("response.output_text.delta"));
    }
}
