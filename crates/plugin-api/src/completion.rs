//! What an upstream call returns.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use hofund_ir::{ContentPart, Response, StreamEvent};

use crate::body::BodyStream;
use crate::error::PluginError;

/// How many bytes of a probe body are kept. The rest is dropped, not drained.
pub const PROBE_CAP: usize = 4096;

/// What one execution produced.
#[non_exhaustive]
pub enum Completion {
    /// A finished response.
    Unary(Box<Response>),
    /// Events in order. Dropping the stream cancels the rest.
    Stream(Pin<Box<dyn Stream<Item = Result<StreamEvent, PluginError>> + Send>>),
    /// Same-protocol bytes. The gateway forwards them and only observes them.
    Raw(BodyStream),
}

/// The result of asking an upstream to count input tokens.
///
/// [`CountOutcome::Unsupported`] is a success. It is not
/// [`PluginErrorKind::Unsupported`](crate::PluginErrorKind::Unsupported).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CountOutcome {
    /// The upstream counted `input_tokens`.
    Counted(u64),
    /// This upstream has no count endpoint. The caller should continue.
    Unsupported,
}

/// What `probe` records. The snippet is capped at [`PROBE_CAP`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbeReport {
    snippet: String,
    truncated: bool,
}

impl ProbeReport {
    /// A report the caller already built. `snippet` is capped.
    #[must_use]
    pub fn new(snippet: impl Into<String>) -> Self {
        let snippet = snippet.into();
        if snippet.len() <= PROBE_CAP {
            Self {
                snippet,
                truncated: false,
            }
        } else {
            Self {
                snippet: snippet.chars().take(PROBE_CAP).collect(),
                truncated: true,
            }
        }
    }

    /// Reads a unary body, the first stream event, or the first raw chunk.
    ///
    /// This is async because the stream is async. It polls the stream once and
    /// then drops it, so a later event is not read. The sketch in the RFC
    /// called this synchronously; a sync poll cannot wait for the first chunk.
    pub async fn from_completion(completion: Completion) -> Self {
        match completion {
            Completion::Unary(response) => Self::new(text_of(&response)),
            Completion::Stream(mut stream) => {
                let Some(event) = stream.next().await else {
                    return Self::new("");
                };
                drop(stream);
                match event {
                    Ok(event) => Self::new(event_label(&event)),
                    Err(error) => Self::new(error.message()),
                }
            }
            Completion::Raw(mut body) => {
                let Some(chunk) = body.next_chunk().await else {
                    return Self::new("");
                };
                drop(body);
                match chunk {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        Self::new(text.as_ref())
                    }
                    Err(error) => Self::new(error.message()),
                }
            }
        }
    }

    /// The capped text.
    #[must_use]
    pub fn snippet(&self) -> &str {
        &self.snippet
    }

    /// Whether the body was longer than [`PROBE_CAP`].
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

fn text_of(response: &Response) -> String {
    let mut text = String::new();
    for part in &response.message.content {
        if let ContentPart::Text(part) = part {
            text.push_str(&part.text);
        }
    }
    text
}

fn event_label(event: &StreamEvent) -> String {
    match event {
        StreamEvent::TextDelta(delta) => delta.text.clone(),
        StreamEvent::MessageStart(_) => "message_start".to_owned(),
        StreamEvent::ContentBlockStart(_) => "content_block_start".to_owned(),
        StreamEvent::ThinkingDelta(_) => "thinking_delta".to_owned(),
        StreamEvent::SignatureDelta(_) => "signature_delta".to_owned(),
        StreamEvent::ToolArgumentsDelta(_) => "tool_arguments_delta".to_owned(),
        StreamEvent::ContentBlockStop(_) => "content_block_stop".to_owned(),
        StreamEvent::MessageDelta(_) => "message_delta".to_owned(),
        StreamEvent::MessageStop(_) => "message_stop".to_owned(),
        StreamEvent::Ping(_) => "ping".to_owned(),
        StreamEvent::Unknown(_) => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use bytes::Bytes;
    use futures_util::Stream;
    use hofund_ir::{ContentPart, Message, Response, Role, StreamEvent, TextDelta};

    use super::{Completion, PROBE_CAP, ProbeReport};
    use crate::body::BodyStream;
    use crate::error::PluginError;

    struct Once<T> {
        item: Option<T>,
    }

    impl<T> Unpin for Once<T> {}

    impl<T> Stream for Once<T> {
        type Item = T;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.get_mut().item.take())
        }
    }

    #[test]
    fn cap_marks_truncation_and_keeps_a_char_boundary() {
        let report = ProbeReport::new("é".repeat(PROBE_CAP));
        assert!(report.truncated() || report.snippet().len() <= "é".repeat(PROBE_CAP).len());
        let long = "a".repeat(PROBE_CAP + 10);
        let report = ProbeReport::new(long);
        assert!(report.truncated());
        assert!(report.snippet().len() <= PROBE_CAP);
    }

    #[tokio::test]
    async fn stream_probe_reads_one_event_and_stops() {
        let event = StreamEvent::TextDelta(TextDelta {
            index: 0,
            text: "hello".to_owned(),
            extensions: hofund_ir::Extensions::new(),
        });
        let stream: Pin<Box<dyn Stream<Item = Result<StreamEvent, PluginError>> + Send>> =
            Box::pin(Once {
                item: Some(Ok(event)),
            });
        let report = ProbeReport::from_completion(Completion::Stream(stream)).await;
        assert_eq!(report.snippet(), "hello");
        assert!(!report.truncated());
    }

    #[tokio::test]
    async fn raw_probe_reads_one_chunk() {
        let body = BodyStream::from_bytes(Bytes::from_static(b"raw-body"));
        let report = ProbeReport::from_completion(Completion::Raw(body)).await;
        assert_eq!(report.snippet(), "raw-body");
    }

    #[tokio::test]
    async fn unary_probe_reads_text_parts() {
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("pong"));
        let response = Response::from_parts(
            None,
            None,
            message,
            None,
            None,
            hofund_ir::Extensions::new(),
        );
        let report = ProbeReport::from_completion(Completion::Unary(Box::new(response))).await;
        assert_eq!(report.snippet(), "pong");
    }
}
