//! The [`Provider`] trait and the upstream adapters that execute a canonical
//! request against one endpoint.
//!
//! Adapters own protocol selection and quirk application. Callers above this
//! crate see providers, not vendors. A streaming result is a pull-based
//! sequence of IR events: the next upstream chunk is read when the caller
//! polls, which is the backpressure.

mod anthropic;
mod error;
mod http;
mod openai_compat;
mod stream;

pub use anthropic::Anthropic;
pub use error::{ProviderError, ProviderErrorKind};
pub use http::Endpoint;
pub use openai_compat::{OpenAiCompat, OpenAiProtocol};

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use hofund_ir::{Request, Response, StreamEvent};

/// What one execution produced.
///
/// The response is boxed so a stream handle and a finished response have the
/// same size. Callers that only forward the value do not copy the message.
pub enum Completion {
    /// A finished response.
    Unary(Box<Response>),
    /// Events in order. The stream ends when the upstream body ends.
    Stream(Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>),
}

/// One upstream endpoint.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable id for routes and usage rows.
    fn id(&self) -> &str;

    /// Executes `request`.
    ///
    /// `inbound_protocol` is the codec id of the client protocol. When it
    /// differs from this adapter's codec id, the extension bag is dropped
    /// before the upstream body is encoded and again on the decoded response
    /// and stream events. The same id keeps the bag, so a same-protocol
    /// passthrough still round-trips unknown fields.
    ///
    /// When `request.stream` is set, the result is [`Completion::Stream`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the upstream cannot be reached, rejects
    /// the call, or returns a body this adapter cannot decode.
    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError>;
}
