//! The [`Provider`] trait and the upstream adapters that execute a canonical
//! request against one endpoint.
//!
//! Adapters own protocol selection and quirk application. Callers above this
//! crate see providers, not vendors. A streaming result is a pull-based
//! sequence of IR events: the next upstream chunk is read when the caller
//! polls, which is the backpressure.

mod anthropic;
mod azure;
mod bedrock;
mod billing;
mod error;
mod generative;
mod http;
mod mantle;
mod openai_compat;
mod presets;
mod sigv4;
mod stream;
mod vertex;

pub use anthropic::Anthropic;
pub use azure::{AzureDeployment, list_deployments};
pub use bedrock::Bedrock;
pub use error::{ProviderError, ProviderErrorKind};
pub use generative::Generative;
pub use http::{Credential, Endpoint};
pub use openai_compat::{OpenAiCompat, OpenAiProtocol};
pub use presets::{PresetError, Resolve, Resolved, Wire, catalog_commit, resolve};
pub use vertex::Vertex;

pub use hofund_plugin_api::{
    DynSubscriptionFactory, DynUpstreamFactory, InboundSurface, Subscription, SubscriptionFactory,
    UpstreamFactory, UpstreamProvider,
};

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

/// The result of asking an upstream to count input tokens.
pub enum CountOutcome {
    /// The upstream counted `input_tokens`.
    Counted(u64),
    /// This upstream has no count endpoint.
    Unsupported,
}

/// One upstream endpoint.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable id for routes and usage rows.
    fn id(&self) -> &str;

    /// Counts input tokens for `request`.
    ///
    /// The default is [`CountOutcome::Unsupported`]. An adapter that has a
    /// count endpoint overrides this.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the upstream rejects the count for a
    /// reason other than "this endpoint does not exist".
    async fn count_input(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<CountOutcome, ProviderError> {
        let _ = (request, inbound_protocol);
        Ok(CountOutcome::Unsupported)
    }

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
