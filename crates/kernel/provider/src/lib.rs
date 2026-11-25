//! The [`Provider`] trait and the upstream adapters that execute a canonical
//! request against one endpoint.
//!
//! Adapters own protocol selection and quirk application. Callers above this
//! crate see providers, not vendors. A streaming result is a pull-based
//! sequence of IR events: the next upstream chunk is read when the caller
//! polls, which is the backpressure.

mod anthropic;
mod attempt;
pub mod balance;
mod billing;
mod error;
mod generative;
mod http;
mod models;
mod openai_compat;
pub mod pass_headers;
mod probe;
mod realtime;
mod stream;
mod wire;

pub use anthropic::Anthropic;
pub use attempt::{AttemptHook, AttemptRoute};
pub use error::{ProviderError, ProviderErrorKind};
pub use generative::Generative;
pub use hofund_ir::api_refused;
pub use http::{
    ClientRefresh, Credential, Endpoint, PooledClientSlot, pooled_client_slot, pooled_reqwest,
};
pub use models::{both_auth_applies, both_auth_refused, parse_model_rows};
pub use openai_compat::{OpenAiCompat, OpenAiProtocol, responses_kept_by_host};
pub use realtime::{realtime_model_url, realtime_target};
pub use wire::{ModelPlan, Wire, credential_for};

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
    /// `inner` was produced by `codec`.
    ///
    /// The daemon copies this onto the plugin completion so a member whose
    /// label is not the codec still tells the router which codec ran.
    Produced {
        /// Codec that encoded the request and decoded or relayed this body.
        codec: hofund_plugin_api::ProtocolId,
        /// The unary, stream, or raw body.
        inner: Box<Completion>,
    },
}

impl Completion {
    /// Tags `self` with the codec that produced it.
    #[must_use]
    pub fn produced(self, codec: hofund_plugin_api::ProtocolId) -> Self {
        Self::Produced {
            codec,
            inner: Box::new(self),
        }
    }

    /// Removes producing-codec tags and returns the body.
    #[must_use]
    pub fn peel(self) -> Self {
        match self {
            Self::Produced { inner, .. } => inner.peel(),
            other => other,
        }
    }

    /// Splits producing-codec tags from the body.
    ///
    /// Nested tags keep the codec closest to the body. An untagged completion
    /// returns `None` and itself.
    #[must_use]
    pub fn split_produced(self) -> (Option<hofund_plugin_api::ProtocolId>, Self) {
        match self {
            Self::Produced { codec, inner } => {
                let (inner_codec, body) = inner.split_produced();
                (Some(inner_codec.unwrap_or(codec)), body)
            }
            other => (None, other),
        }
    }
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

    /// Route facts available to an encoded-body attempt hook.
    fn attempt_route(&self) -> AttemptRoute {
        AttemptRoute::default()
    }

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

    /// Executes with an observer for the encoded per-attempt body.
    ///
    /// Adapters that support body-level continuation override this method. The
    /// default preserves the behavior of every other adapter.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::execute`].
    async fn execute_with_attempt(
        &self,
        request: Request,
        inbound_protocol: &str,
        hook: Option<&dyn AttemptHook>,
    ) -> Result<Completion, ProviderError> {
        let _ = hook;
        self.execute(request, inbound_protocol).await
    }

    /// Posts `body` to `path` under this adapter's base URL and returns the
    /// answer unchanged.
    ///
    /// The default is `unsupported`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when this adapter does not serve `path` or
    /// the call fails.
    async fn forward(
        &self,
        path: &str,
        body: hofund_plugin_api::RawBody,
    ) -> Result<hofund_plugin_api::RawAnswer, ProviderError> {
        let _ = body;
        Err(ProviderError::unsupported(format!(
            "this upstream does not serve {path}"
        )))
    }

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

    /// Whether a background rest probe may list with this key.
    fn rest_probe(&self) -> bool {
        false
    }

    /// The list a background rest probe asks.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`](hofund_plugin_api::PluginError) when the list
    /// fails or this adapter has no probe list.
    async fn list_for_probe(
        &self,
    ) -> Result<hofund_plugin_api::ModelList, hofund_plugin_api::PluginError> {
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

    /// Drops idle keep-alive connections before the next upstream attempt.
    fn close_idle_connections(&self) {}

    /// `chat`, `responses`, or `anthropic` when a list named the API `model` is asked on best.
    fn learned_preferred(&self, _model: &str) -> Option<String> {
        None
    }

    /// A coding-agent catalog entry learned for `model`, as a JSON object.
    fn learned_catalog_entry(&self, _model: &str) -> Option<String> {
        None
    }
}
