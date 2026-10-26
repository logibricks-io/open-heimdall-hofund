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
mod models;
mod openai_compat;
mod presets;
mod probe;
mod stream;
mod vertex;

pub use anthropic::Anthropic;
pub use azure::{AzureDeployment, list_deployments};
pub use bedrock::Bedrock;
pub use error::{ProviderError, ProviderErrorKind};
pub use generative::Generative;
pub use http::{Credential, Endpoint, pooled_reqwest};
pub use models::{azure_host, parse_model_rows};
pub use openai_compat::{OpenAiCompat, OpenAiProtocol};
pub use presets::{
    ModelPlan, PresetError, Resolve, Resolved, Wire, catalog_commit, model_plan, resolve,
};
pub use vertex::Vertex;

pub use hofund_plugin_api::{
    DynSubscriptionFactory, DynUpstreamFactory, InboundSurface, Subscription, SubscriptionFactory,
    UpstreamFactory, UpstreamProvider,
};

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
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
    /// Same-protocol bytes. The gateway forwards them and only observes them.
    Raw(Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>),
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
    /// When `request.stream` is set and `inbound_protocol` differs from this
    /// adapter, the result is [`Completion::Stream`]. The same protocol returns
    /// [`Completion::Raw`] and the caller forwards the bytes.
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

    /// Lists models on this adapter's configured wire.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`](hofund_plugin_api::PluginError) when the list
    /// fails or this adapter has no list.
    async fn list_models(
        &self,
        query: &hofund_plugin_api::ListQuery,
    ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
        let _ = query;
        Err(hofund_plugin_api::PluginError::unsupported(
            "this upstream has no model list",
        ))
    }

    /// Sends one minimal request on this adapter's configured wire.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`](hofund_plugin_api::PluginError) when the probe
    /// fails or is unsupported.
    async fn probe(
        &self,
        query: &hofund_plugin_api::ProbeQuery,
    ) -> Result<hofund_plugin_api::ProbeReport, hofund_plugin_api::PluginError> {
        let _ = query;
        Err(hofund_plugin_api::PluginError::unsupported(
            "this upstream has no probe",
        ))
    }
}
