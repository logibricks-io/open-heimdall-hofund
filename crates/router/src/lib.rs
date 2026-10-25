//! Ordered routing and failover across providers.
//!
//! A model name maps to an ordered list of providers. The next provider is
//! tried only when the current one fails before any stream event has been
//! produced. This crate does not name vendors and does not read quirk data.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures_util::StreamExt;
use hofund_ir::{Request, StreamEvent, estimate_input_tokens};

pub use hofund_provider::{Completion, CountOutcome, Provider, ProviderError, ProviderErrorKind};

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

/// One provider a route may call, and the model name that provider expects.
#[derive(Clone)]
pub struct Hop {
    /// The upstream.
    pub provider: Arc<dyn Provider>,
    /// Model name sent to `provider`. Absent keeps the caller's model name,
    /// after a `provider/model` id is reduced to its model portion.
    pub wire_model: Option<String>,
}

/// A model-to-upstream table.
pub struct Router {
    routes: BTreeMap<String, Vec<Hop>>,
    providers: BTreeSet<String>,
    policy: FailoverPolicy,
}

/// A counted input.
pub struct InputCount {
    /// Input tokens.
    pub tokens: u64,
    /// `true` when no upstream counted and the number is a character estimate.
    pub estimated: bool,
    /// Provider that counted, when one did.
    pub upstream_id: String,
}

struct Resolved {
    hops: Vec<Hop>,
    /// Name sent when a hop has no wire model.
    bare: String,
}

impl Router {
    /// Builds a router. An empty list for a model is the same as no route.
    ///
    /// Each provider is called with the model name the client sent.
    #[must_use]
    pub fn new(routes: BTreeMap<String, Vec<Arc<dyn Provider>>>, policy: FailoverPolicy) -> Self {
        let routes = routes
            .into_iter()
            .map(|(model, providers)| {
                let hops = providers
                    .into_iter()
                    .map(|provider| Hop {
                        provider,
                        wire_model: None,
                    })
                    .collect();
                (model, hops)
            })
            .collect();
        Self::from_hops(routes, policy)
    }

    /// Builds a router from hops that may rename the model per provider.
    #[must_use]
    pub fn from_hops(routes: BTreeMap<String, Vec<Hop>>, policy: FailoverPolicy) -> Self {
        let mut providers = BTreeSet::new();
        for hops in routes.values() {
            for hop in hops {
                providers.insert(hop.provider.id().to_owned());
            }
        }
        Self {
            routes,
            providers,
            policy,
        }
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
    /// tell a same-protocol passthrough from a translation. The reply's model
    /// is the name the client sent. Each hop may send a different name upstream.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::NoRoute`] when the model has no upstream.
    /// Returns [`RouterError::Ambiguous`] when a bare model name is served by
    /// more than one provider. Returns [`RouterError::Upstream`] when every
    /// allowed attempt failed, or when a failure is not one the policy retries.
    pub async fn dispatch(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Routed, RouterError> {
        let asked = request.model.clone();
        let resolved = self.resolve(&asked)?;
        let policy = self.policy.clone();
        let mut last = None;
        for hop in &resolved.hops {
            let mut attempt = request.clone();
            apply_wire(&mut attempt, hop, &resolved.bare);
            let result = match hop.provider.execute(attempt, inbound_protocol).await {
                Ok(completion) => accept(completion).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(completion) => {
                    return Ok(Routed {
                        upstream_id: hop.provider.id().to_owned(),
                        completion: label_model(completion, asked),
                    });
                }
                Err(error) if policy.allows(&error) => {
                    last = Some((hop.provider.id().to_owned(), error));
                }
                Err(error) => {
                    return Err(RouterError::Upstream {
                        upstream_id: hop.provider.id().to_owned(),
                        error,
                    });
                }
            }
        }
        match last {
            Some((upstream_id, error)) => Err(RouterError::Upstream { upstream_id, error }),
            None => Err(self.missing(&request.model)),
        }
    }

    /// Counts input tokens for `request`.
    ///
    /// An upstream that has a count endpoint is asked, with the same failover
    /// rules as [`Self::dispatch`]. When every asked upstream has no count
    /// endpoint, the result is a character estimate and [`InputCount::estimated`]
    /// is set.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::NoRoute`] or [`RouterError::Ambiguous`] for the
    /// same model names as [`Self::dispatch`]. Returns [`RouterError::Upstream`]
    /// when an upstream rejects the count and no later upstream can be tried.
    pub async fn count(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<InputCount, RouterError> {
        let resolved = self.resolve(&request.model)?;
        let policy = self.policy.clone();
        let mut last = None;
        let mut unsupported = false;
        for hop in &resolved.hops {
            let mut attempt = request.clone();
            apply_wire(&mut attempt, hop, &resolved.bare);
            match hop.provider.count_input(attempt, inbound_protocol).await {
                Ok(CountOutcome::Counted(tokens)) => {
                    return Ok(InputCount {
                        tokens,
                        estimated: false,
                        upstream_id: hop.provider.id().to_owned(),
                    });
                }
                Ok(CountOutcome::Unsupported) => unsupported = true,
                Err(error) if policy.allows(&error) => {
                    last = Some((hop.provider.id().to_owned(), error));
                }
                Err(error) => {
                    return Err(RouterError::Upstream {
                        upstream_id: hop.provider.id().to_owned(),
                        error,
                    });
                }
            }
        }
        if unsupported {
            return Ok(InputCount {
                tokens: estimate_input_tokens(&request),
                estimated: true,
                upstream_id: String::new(),
            });
        }
        match last {
            Some((upstream_id, error)) => Err(RouterError::Upstream { upstream_id, error }),
            None => Err(self.missing(&request.model)),
        }
    }

    fn resolve(&self, asked: &str) -> Result<Resolved, RouterError> {
        if let Some(hops) = self.routes.get(asked).filter(|hops| !hops.is_empty()) {
            return Ok(Resolved {
                hops: hops.clone(),
                bare: asked.to_owned(),
            });
        }
        if let Some((provider_id, model)) = asked.split_once('/')
            && self.providers.contains(provider_id)
        {
            let found = self.matching(|route_model, hop| {
                hop.provider.id() == provider_id
                    && (route_model == model || hop.wire_model.as_deref() == Some(model))
            });
            return self.one(asked, model, found);
        }
        let found = self.matching(|_, hop| hop.wire_model.as_deref() == Some(asked));
        let bare = found
            .first()
            .and_then(|(_, hop)| hop.wire_model.clone())
            .unwrap_or_else(|| asked.to_owned());
        self.one(asked, &bare, found)
    }

    fn matching(&self, mut pred: impl FnMut(&str, &Hop) -> bool) -> Vec<(String, Hop)> {
        let mut found = Vec::new();
        for (route_model, hops) in &self.routes {
            for hop in hops {
                if pred(route_model, hop) {
                    found.push((route_model.clone(), hop.clone()));
                }
            }
        }
        found
    }

    fn one(
        &self,
        asked: &str,
        bare: &str,
        found: Vec<(String, Hop)>,
    ) -> Result<Resolved, RouterError> {
        match found.len() {
            0 => Err(self.missing(asked)),
            1 => Ok(Resolved {
                hops: vec![found.into_iter().next().expect("one hop").1],
                bare: bare.to_owned(),
            }),
            _ => Err(RouterError::Ambiguous {
                model: asked.to_owned(),
                candidates: candidates(&found),
            }),
        }
    }

    fn missing(&self, model: &str) -> RouterError {
        let routes = self.models().join(", ");
        let detail = if routes.is_empty() {
            String::new()
        } else {
            format!(". Routes: {routes}")
        };
        RouterError::NoRoute {
            model: model.to_owned(),
            detail,
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
    #[error("no upstream is configured for model `{model}`{detail}")]
    NoRoute {
        /// The requested model.
        model: String,
        /// Route names, when any exist. Empty when the table is empty.
        detail: String,
    },
    /// More than one provider serves this model name.
    #[error("model `{model}` is served by more than one provider: {candidates}")]
    Ambiguous {
        /// The requested model.
        model: String,
        /// `provider/route` names, comma-separated.
        candidates: String,
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

fn apply_wire(request: &mut Request, hop: &Hop, bare: &str) {
    request.model = hop.wire_model.clone().unwrap_or_else(|| bare.to_owned());
}

fn candidates(found: &[(String, Hop)]) -> String {
    found
        .iter()
        .map(|(route, hop)| format!("{}/{}", hop.provider.id(), route))
        .collect::<Vec<_>>()
        .join(", ")
}

fn label_model(completion: Completion, model: String) -> Completion {
    match completion {
        Completion::Unary(mut response) => {
            response.model = Some(model);
            Completion::Unary(response)
        }
        Completion::Stream(stream) => {
            let labeled =
                stream.map(move |item| item.map(|event: StreamEvent| event.with_model(&model)));
            Completion::Stream(Box::pin(labeled))
        }
    }
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
