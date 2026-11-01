//! Pull-based decode of an upstream server-sent event body.
//!
//! The next chunk is read only when the caller has consumed the events already
//! decoded, so a slow client slows the upstream read.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use hofund_codec::{StreamDecoder, refusal_error};
use hofund_ir::StreamEvent;
use reqwest::Error as ReqwestError;

use crate::error::ProviderError;

pub(crate) struct SseEvents {
    incoming: Pin<Box<dyn Stream<Item = Result<Bytes, ReqwestError>> + Send>>,
    decoder: Box<dyn StreamDecoder>,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    finished: bool,
    committed: bool,
    /// Drop extension bags, and raw events this decoder does not model, when
    /// the client protocol and the upstream protocol differ.
    strip: bool,
}

impl SseEvents {
    pub(crate) fn new(
        incoming: impl Stream<Item = Result<Bytes, ReqwestError>> + Send + 'static,
        decoder: Box<dyn StreamDecoder>,
        strip: bool,
    ) -> Self {
        Self {
            incoming: Box::pin(incoming),
            decoder,
            pending: VecDeque::new(),
            finished: false,
            committed: false,
            strip,
        }
    }

    fn take_pending(&mut self) -> Option<Result<StreamEvent, ProviderError>> {
        loop {
            let mut item = self.pending.pop_front()?;
            if let Ok(event) = &item {
                let refusal = refusal_error(event);
                if self.committed && refusal {
                    return Some(item);
                }
                if let Some(error) = failure_of(event) {
                    self.pending.clear();
                    self.finished = true;
                    return Some(Err(error));
                }
                self.committed |= stream_commits(event);
            }
            if self.strip
                && let Ok(event) = &mut item
            {
                if matches!(event, StreamEvent::Unknown(_)) {
                    continue;
                }
                event.strip_extensions();
            }
            return Some(item);
        }
    }
}

impl Stream for SseEvents {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(item) = this.take_pending() {
                return Poll::Ready(Some(item));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            match this.incoming.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.finished = true;
                    match this.decoder.finish() {
                        Ok(events) => this.pending.extend(events.into_iter().map(Ok)),
                        Err(error) => this
                            .pending
                            .push_back(Err(ProviderError::decode(error.to_string()))),
                    }
                }
                Poll::Ready(Some(Err(error))) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(map_body(error))));
                }
                Poll::Ready(Some(Ok(chunk))) => match this.decoder.push(&chunk) {
                    Ok(events) => this.pending.extend(events.into_iter().map(Ok)),
                    Err(error) => {
                        this.finished = true;
                        return Poll::Ready(Some(Err(ProviderError::decode(error.to_string()))));
                    }
                },
            }
        }
    }
}

fn stream_commits(event: &StreamEvent) -> bool {
    match event {
        StreamEvent::MessageStart(_)
        | StreamEvent::ContentBlockStart(_)
        | StreamEvent::ContentBlockStop(_)
        | StreamEvent::Ping(_) => false,
        StreamEvent::MessageDelta(delta) => delta.stop.is_some(),
        _ => true,
    }
}

/// A stream that reports an upstream failure before any content event is a
/// status the router can fail over, when that status is configured.
pub(crate) fn failure_of(event: &StreamEvent) -> Option<ProviderError> {
    if let StreamEvent::MessageDelta(delta) = event
        && delta.stop.as_ref().is_some_and(|stop| {
            matches!(&stop.reason, hofund_ir::StopReason::Other(reason) if reason == "error")
        })
    {
        return Some(
            ProviderError::status(502, "the reply ended with finish_reason \"error\"")
                .without_cooldown(),
        );
    }
    let StreamEvent::Unknown(unknown) = event else {
        return None;
    };
    let embedded = unknown.name == "data" && unknown.payload.get("error").is_some();
    if unknown.name != "error" && unknown.name != "response.failed" && !embedded {
        return None;
    }
    let message = ["error.message", "message", "response.error.message"]
        .into_iter()
        .find_map(|path| pointer(&unknown.payload, path))
        .unwrap_or_else(|| "upstream stream failed".to_owned());
    let body = serde_json::to_vec(&unknown.payload).unwrap_or_default();
    let status = hofund_plugin_sdk::transport::status_for_error(&body);
    let error = if hofund_plugin_sdk::transport::is_credit_error(status, &body) {
        ProviderError::credit_status(status, message)
    } else if hofund_plugin_sdk::transport::is_quota_error(status, &body) {
        let mut error = ProviderError::quota(message);
        error.status = Some(status);
        error
    } else {
        ProviderError::status(status, message)
    };
    Some(if refusal_error(event) {
        error.without_cooldown()
    } else {
        error
    })
}

fn pointer(value: &serde_json::Value, path: &str) -> Option<String> {
    let mut current = value;
    for key in path.split('.') {
        current = current.get(key)?;
    }
    let text = current.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

pub(crate) fn map_transport(error: ReqwestError) -> ProviderError {
    if error.is_timeout() {
        ProviderError::timeout(error.to_string())
    } else if error.is_connect() || error.status().is_none() {
        ProviderError::connect(error.to_string())
    } else {
        ProviderError::status(
            error.status().map_or(502, |status| status.as_u16()),
            error.to_string(),
        )
    }
}

pub(crate) fn map_body(error: ReqwestError) -> ProviderError {
    let mut message = error.to_string();
    if error.is_timeout() && !message.contains("timed out") {
        message.push_str(": timed out");
    }
    ProviderError::interrupted(message)
}

pub(crate) fn message_from(bytes: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        for path in ["error.message", "message"] {
            if let Some(text) = pointer(&value, path) {
                return text;
            }
        }
    }
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "upstream request failed".to_owned()
    } else {
        trimmed.chars().take(512).collect()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use hofund_codec::{AnthropicMessages, Codec};
    use hofund_ir::StreamEvent;

    use super::SseEvents;
    use reqwest::Error as ReqwestError;

    const THOUGHT: &str = concat!(
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"thought\"}}\n\n"
    );
    const REFUSAL: &str = concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"code\":\"content_filter\",\"message\":\"refused\"}}\n\n"
    );

    fn events(bytes: Vec<u8>) -> SseEvents {
        let incoming = stream::iter(vec![Ok::<Bytes, ReqwestError>(Bytes::from(bytes))]);
        SseEvents::new(incoming, AnthropicMessages.stream_decoder(), true)
    }

    #[tokio::test]
    async fn b9_committed_refusal_error_keeps_its_event() {
        let events = events([THOUGHT.as_bytes(), REFUSAL.as_bytes()].concat());
        let events: Vec<_> = events.collect().await;
        assert!(matches!(events[0], Ok(StreamEvent::ThinkingDelta(_))));
        assert!(matches!(events[1], Ok(StreamEvent::Unknown(_))));
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn b9_pre_commit_refusal_error_is_not_cooling() {
        let events = events(REFUSAL.as_bytes().to_vec());
        let events: Vec<_> = events.collect().await;
        let error = events[0].as_ref().expect_err("refusal");
        assert!(!error.cools());
        assert_eq!(events.len(), 1);
    }
}
