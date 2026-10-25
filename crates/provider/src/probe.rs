//! A minimal request used to see whether one upstream answers.
//!
//! The body is one user message, `hi`, with `max_tokens` 16. The adapter that
//! serves the route sends it, so the headers and the path are the ones a real
//! call uses.

use std::time::Instant;

use futures_util::StreamExt;
use hofund_ir::{ContentPart, Message, Request, Role};

use crate::error::ProviderError;
use crate::presets::Wire;
use crate::{Completion, Provider};

/// What one probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    /// Model id that was asked. Empty when there was nothing to ask.
    pub model: String,
    /// The upstream accepted the request and the body decoded.
    pub ok: bool,
    /// HTTP status, when the call produced one. `Some(200)` when the adapter
    /// returned a completion.
    pub status: Option<u16>,
    /// Milliseconds spent in the call. Zero when nothing was sent.
    pub latency_ms: i64,
    /// Why the probe failed. Empty when [`Self::ok`] is set.
    pub error: String,
    /// Input tokens, when the completion reported them.
    pub input_tokens: Option<i64>,
    /// Output tokens, when the completion reported them.
    pub output_tokens: Option<i64>,
    /// Whether a request was sent. A missing model does not send one.
    pub sent: bool,
}

/// The codec id passed as the inbound protocol so the adapter keeps its own
/// fields.
#[must_use]
pub fn probe_inbound(wire: Wire) -> &'static str {
    match wire {
        Wire::Chat => "openai-chat",
        Wire::Responses => "openai-responses",
        Wire::Messages => "anthropic-messages",
        Wire::Generative | Wire::Vertex => "generative-language",
        Wire::Converse => "bedrock-converse",
    }
}

/// The smallest request a probe sends.
#[must_use]
pub fn probe_request(model: &str) -> Request {
    let mut request = Request::new(model);
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("hi"));
    request.messages.push(message);
    request.params.max_tokens = Some(16);
    request
}

/// Sends [`probe_request`] through `provider`.
///
/// An empty `model` returns a report and does not call the provider.
pub async fn run_probe(provider: &dyn Provider, wire: Wire, model: &str) -> ProbeReport {
    let model = model.trim();
    if model.is_empty() {
        return ProbeReport {
            model: String::new(),
            ok: false,
            status: None,
            latency_ms: 0,
            error: "no model to try: refresh the model list".to_owned(),
            input_tokens: None,
            output_tokens: None,
            sent: false,
        };
    }
    let started = Instant::now();
    let outcome = provider
        .execute(probe_request(model), probe_inbound(wire))
        .await;
    let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    match outcome {
        Ok(Completion::Unary(response)) => {
            let (input_tokens, output_tokens) = tokens(response.usage.as_ref());
            ProbeReport {
                model: model.to_owned(),
                ok: true,
                status: Some(200),
                latency_ms,
                error: String::new(),
                input_tokens,
                output_tokens,
                sent: true,
            }
        }
        Ok(Completion::Stream(mut stream)) => {
            while let Some(item) = stream.next().await {
                if let Err(error) = item {
                    return failed(model, latency_ms, &error);
                }
            }
            ProbeReport {
                model: model.to_owned(),
                ok: true,
                status: Some(200),
                latency_ms,
                error: String::new(),
                input_tokens: None,
                output_tokens: None,
                sent: true,
            }
        }
        Err(error) => failed(model, latency_ms, &error),
    }
}

fn failed(model: &str, latency_ms: i64, error: &ProviderError) -> ProbeReport {
    ProbeReport {
        model: model.to_owned(),
        ok: false,
        status: error.status,
        latency_ms,
        error: error
            .message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        input_tokens: None,
        output_tokens: None,
        sent: true,
    }
}

fn tokens(usage: Option<&hofund_ir::Usage>) -> (Option<i64>, Option<i64>) {
    let Some(usage) = usage else {
        return (None, None);
    };
    (
        usage
            .input_tokens
            .and_then(|value| i64::try_from(value).ok()),
        usage
            .output_tokens
            .and_then(|value| i64::try_from(value).ok()),
    )
}
