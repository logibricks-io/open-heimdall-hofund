//! Pull-based decode of an upstream server-sent event body.
//!
//! The next chunk is read only when the caller has consumed the events already
//! decoded, so a slow client slows the upstream read.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;
use hofund_codec::StreamDecoder;
use hofund_ir::StreamEvent;
use reqwest::Error as ReqwestError;

use crate::error::ProviderError;

pub(crate) struct SseEvents {
    incoming: Pin<Box<dyn Stream<Item = Result<Bytes, ReqwestError>> + Send>>,
    decoder: Box<dyn StreamDecoder>,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    finished: bool,
    yielded: bool,
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
            yielded: false,
            strip,
        }
    }

    fn take_pending(&mut self) -> Option<Result<StreamEvent, ProviderError>> {
        loop {
            let mut item = self.pending.pop_front()?;
            if !self.yielded
                && let Ok(event) = &item
                && let Some(error) = failure_of(event)
            {
                self.pending.clear();
                self.finished = true;
                return Some(Err(error));
            }
            if self.strip
                && let Ok(event) = &mut item
            {
                if matches!(event, StreamEvent::Unknown(_)) {
                    continue;
                }
                event.strip_extensions();
            }
            if item.is_ok() {
                self.yielded = true;
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
                    return Poll::Ready(Some(Err(map_transport(error))));
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

/// A stream that reports an upstream failure before any content event is a
/// status the router can fail over, when that status is configured.
fn failure_of(event: &StreamEvent) -> Option<ProviderError> {
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
    Some(ProviderError::status(502, message))
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
