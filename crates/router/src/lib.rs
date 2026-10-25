//! Ordered routing and failover across providers.
//!
//! A model name maps to an ordered list of providers. The next provider is
//! tried only when the current one fails before any stream event has been
//! produced. This crate does not name vendors and does not read quirk data.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures_util::StreamExt;
use hofund_ir::Request;

pub use hofund_provider::{Completion, Provider, ProviderError, ProviderErrorKind};

/// Which failures may try the next provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailoverPolicy {
    /// Connect and timeout errors try the next provider.
    pub connect_errors: bool,
    /// HTTP statuses that try the next provider.
    pub statuses: BTreeSet<u16>,
}

impl Default for FailoverPolicy {
    fn default() -> Self {
        Self {
            connect_errors: true,
            statuses: [429, 500, 502, 503, 504].into_iter().collect(),
        }
    }
}

impl FailoverPolicy {
    /// Whether `error` happened early enough, and is of a kind, that another
    /// provider may be tried.
    #[must_use]
    pub fn allows(&self, error: &ProviderError) -> bool {
        match error.kind {
            ProviderErrorKind::Connect | ProviderErrorKind::Timeout => self.connect_errors,
            ProviderErrorKind::Status => error
                .status
                .is_some_and(|code| self.statuses.contains(&code)),
            ProviderErrorKind::Decode => false,
        }
    }
}

/// A model-to-upstream table.
pub struct Router {
    routes: BTreeMap<String, Vec<Arc<dyn Provider>>>,
    policy: FailoverPolicy,
}

impl Router {
    /// Builds a router. An empty list for a model is the same as no route.
    #[must_use]
    pub fn new(routes: BTreeMap<String, Vec<Arc<dyn Provider>>>, policy: FailoverPolicy) -> Self {
        Self { routes, policy }
    }

    /// Model names that have at least one upstream, in order.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.routes
            .iter()
            .filter(|(_, providers)| !providers.is_empty())
            .map(|(model, _)| model.clone())
            .collect()
    }

    /// Runs `request` against the upstreams configured for its model.
    ///
    /// `inbound_protocol` is forwarded to each provider so an adapter can
    /// tell a same-protocol passthrough from a translation.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::NoRoute`] when the model has no upstream.
    /// Returns [`RouterError::Upstream`] when every allowed attempt failed,
    /// or when a failure is not one the policy retries.
    pub async fn dispatch(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Routed, RouterError> {
        let Some(providers) = self
            .routes
            .get(&request.model)
            .filter(|list| !list.is_empty())
            .cloned()
        else {
            return Err(RouterError::NoRoute {
                model: request.model,
            });
        };
        let policy = self.policy.clone();
        let mut last = None;
        for provider in &providers {
            let result = match provider.execute(request.clone(), inbound_protocol).await {
                Ok(completion) => accept(completion).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(completion) => {
                    return Ok(Routed {
                        upstream_id: provider.id().to_owned(),
                        completion,
                    });
                }
                Err(error) if policy.allows(&error) => {
                    last = Some((provider.id().to_owned(), error));
                }
                Err(error) => {
                    return Err(RouterError::Upstream {
                        upstream_id: provider.id().to_owned(),
                        error,
                    });
                }
            }
        }
        match last {
            Some((upstream_id, error)) => Err(RouterError::Upstream { upstream_id, error }),
            None => Err(RouterError::NoRoute {
                model: request.model,
            }),
        }
    }
}

/// A successful dispatch.
pub struct Routed {
    /// Id of the provider that produced the completion.
    pub upstream_id: String,
    /// The completion. A stream has already produced its first event, or
    /// ended empty. A failure before that is not returned here.
    pub completion: Completion,
}

/// Why a dispatch did not produce a completion.
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    /// No upstream is configured for the model.
    #[error("no upstream is configured for model `{model}`")]
    NoRoute {
        /// The requested model.
        model: String,
    },
    /// The upstreams that were tried failed.
    #[error("upstream `{upstream_id}` failed: {error}")]
    Upstream {
        /// The last upstream that was asked.
        upstream_id: String,
        /// Its error.
        error: ProviderError,
    },
}

async fn accept(completion: Completion) -> Result<Completion, ProviderError> {
    let Completion::Stream(mut stream) = completion else {
        return Ok(completion);
    };
    match stream.next().await {
        None => Ok(Completion::Stream(stream)),
        Some(Err(error)) => Err(error),
        Some(Ok(event)) => {
            let head = futures_util::stream::once(async move { Ok(event) });
            Ok(Completion::Stream(Box::pin(head.chain(stream))))
        }
    }
}
