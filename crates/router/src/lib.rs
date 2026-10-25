//! Ordered routing and failover across members.
//!
//! A model name maps to an ordered list of members. The next member is tried
//! only when the current one fails before any stream event or raw chunk has
//! been produced. This crate does not name vendors or the plugin upstream and
//! host types, and it does not read quirk data. It does not depend on `tower`.
//! The daemon adapts each member into a service and wraps that service in the
//! kernel layers.

mod member;
mod snapshot;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use hofund_ir::{StreamEvent, estimate_input_tokens};
use hofund_plugin_api::{BodyStream, CountOutcome, PluginErrorKind};
use tracing::Instrument;

pub use hofund_plugin_api::{Completion, MemberId, PluginError, Request};
pub use member::Member;
pub use snapshot::Snapshot;

/// Which failures may try the next member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailoverPolicy {
    /// Connect and timeout errors try the next member.
    pub connect_errors: bool,
    /// HTTP statuses that try the next member.
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
    /// member may be tried.
    ///
    /// Kinds other than connect, timeout, and a listed status do not fail
    /// over. A transport failure means the request was already sent, so it
    /// does not try another member. The wildcard is required because
    /// [`PluginErrorKind`] is `#[non_exhaustive]`: a new kind does not break
    /// this match.
    #[must_use]
    pub fn allows(&self, error: &PluginError) -> bool {
        match error.kind() {
            PluginErrorKind::Connect | PluginErrorKind::Timeout => self.connect_errors,
            PluginErrorKind::Status => error
                .status()
                .is_some_and(|code| self.statuses.contains(&code)),
            PluginErrorKind::Transport
            | PluginErrorKind::Decode
            | PluginErrorKind::Unsupported
            | PluginErrorKind::Auth
            | PluginErrorKind::Quota
            | PluginErrorKind::Config
            | PluginErrorKind::Cancelled
            | PluginErrorKind::Io
            | PluginErrorKind::Internal => false,
            _ => false,
        }
    }
}

/// One member a route may call, and the model name that member expects.
#[derive(Clone)]
pub struct Hop {
    /// The upstream.
    pub member: Arc<dyn Member>,
    /// Model name sent to `member`. Absent keeps the caller's model name,
    /// after a `provider/model` id is reduced to its model portion.
    pub wire_model: Option<String>,
    /// Output cap applied when the request omitted one.
    pub max_output_tokens: Option<u64>,
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
    /// Each member is called with the model name the client sent.
    #[must_use]
    pub fn new(routes: BTreeMap<String, Vec<Arc<dyn Member>>>, policy: FailoverPolicy) -> Self {
        let routes = routes
            .into_iter()
            .map(|(model, members)| {
                let hops = members
                    .into_iter()
                    .map(|member| Hop {
                        member,
                        wire_model: None,
                        max_output_tokens: None,
                    })
                    .collect();
                (model, hops)
            })
            .collect();
        Self::from_hops(routes, policy)
    }

    /// Builds a router from hops that may rename the model per member.
    #[must_use]
    pub fn from_hops(routes: BTreeMap<String, Vec<Hop>>, policy: FailoverPolicy) -> Self {
        let mut providers = BTreeSet::new();
        for hops in routes.values() {
            for hop in hops {
                providers.insert(hop.member.route_id().to_owned());
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
    /// `inbound_protocol` is forwarded to each member so an adapter can tell
    /// a same-protocol passthrough from a translation. When the name sent
    /// upstream differs from the name the client asked for, a decoded body is
    /// relabelled with the client name. When the names are equal, the model
    /// the member returned is kept, including a dated snapshot. A raw body
    /// follows the same rule in the gateway. Each hop may send a different
    /// name upstream.
    ///
    /// The per-attempt clone of `request` stays. The caller does not clone
    /// before this call.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::NoRoute`] when the model has no upstream.
    /// Returns [`RouterError::Ambiguous`] when a bare model name is served by
    /// more than one provider. Returns [`RouterError::Upstream`] when every
    /// allowed attempt failed, or when a failure is not one the policy retries.
    pub async fn dispatch(
        &self,
        request: &Request,
        inbound_protocol: &str,
    ) -> Result<Routed, RouterError> {
        let asked = request.model.clone();
        let resolved = self.resolve(&asked)?;
        let policy = self.policy.clone();
        let mut last = None;
        for (index, hop) in resolved.hops.iter().enumerate() {
            let mut attempt = request.clone();
            apply_wire(&mut attempt, hop, &resolved.bare);
            let upstream_model = attempt.model.clone();
            let result = call_member(hop, index, inbound_protocol, attempt).await;
            match result {
                Ok(completion) => {
                    let completion = if asked == upstream_model {
                        completion
                    } else {
                        label_model(completion, asked)
                    };
                    return Ok(Routed {
                        upstream_id: hop.member.route_id().to_owned(),
                        upstream_model,
                        completion,
                    });
                }
                Err(error) if policy.allows(&error) => {
                    last = Some((hop.member.route_id().to_owned(), error));
                }
                Err(error) => {
                    return Err(RouterError::Upstream {
                        upstream_id: hop.member.route_id().to_owned(),
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
        request: &Request,
        inbound_protocol: &str,
    ) -> Result<InputCount, RouterError> {
        let resolved = self.resolve(&request.model)?;
        let policy = self.policy.clone();
        let mut last = None;
        let mut unsupported = false;
        for (index, hop) in resolved.hops.iter().enumerate() {
            let mut attempt = request.clone();
            apply_wire(&mut attempt, hop, &resolved.bare);
            let span = member_span(hop, index, inbound_protocol);
            let started = Instant::now();
            let result = hop
                .member
                .count_input(attempt, inbound_protocol)
                .instrument(span.clone())
                .await;
            record_call(&span, &result_status(result.as_ref().err()), started);
            match result {
                Ok(CountOutcome::Counted(tokens)) => {
                    return Ok(InputCount {
                        tokens,
                        estimated: false,
                        upstream_id: hop.member.route_id().to_owned(),
                    });
                }
                // `Unsupported` and any future variant are not a count. The
                // wildcard is required because [`CountOutcome`] is
                // `#[non_exhaustive]`.
                Ok(_) => unsupported = true,
                Err(error) if policy.allows(&error) => {
                    last = Some((hop.member.route_id().to_owned(), error));
                }
                Err(error) => {
                    return Err(RouterError::Upstream {
                        upstream_id: hop.member.route_id().to_owned(),
                        error,
                    });
                }
            }
        }
        if unsupported {
            return Ok(InputCount {
                tokens: estimate_input_tokens(request),
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
                hop.member.route_id() == provider_id
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
    /// Model name sent to that provider. The gateway compares it with the
    /// name the client asked for before touching a raw body.
    pub upstream_model: String,
    /// The completion. A stream or raw body has already produced its first
    /// chunk, or ended empty. A failure before that is not returned here.
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
        error: PluginError,
    },
}

fn apply_wire(request: &mut Request, hop: &Hop, bare: &str) {
    request.model = hop.wire_model.clone().unwrap_or_else(|| bare.to_owned());
    if request.params.max_tokens.is_none() {
        request.params.max_tokens = hop.max_output_tokens;
    }
}

fn candidates(found: &[(String, Hop)]) -> String {
    found
        .iter()
        .map(|(route, hop)| format!("{}/{}", hop.member.route_id(), route))
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
        Completion::Raw(body) => Completion::Raw(body), // the gateway patches the model
        _ => completion,
    }
}

async fn accept(completion: Completion) -> Result<Completion, PluginError> {
    match completion {
        Completion::Stream(mut stream) => match stream.next().await {
            None => Ok(Completion::Stream(stream)),
            Some(Err(error)) => Err(error),
            Some(Ok(event)) => {
                let head = futures_util::stream::once(async move { Ok(event) });
                Ok(Completion::Stream(Box::pin(head.chain(stream))))
            }
        },
        Completion::Raw(mut body) => match body.next_chunk().await {
            None => Ok(Completion::Raw(body)),
            Some(Err(error)) => Err(error),
            Some(Ok(chunk)) => Ok(Completion::Raw(BodyStream::resume(chunk, body))),
        },
        other => Ok(other),
    }
}

async fn call_member(
    hop: &Hop,
    index: usize,
    inbound_protocol: &str,
    attempt: Request,
) -> Result<Completion, PluginError> {
    let span = member_span(hop, index, inbound_protocol);
    let started = Instant::now();
    let result = async {
        match hop.member.execute(attempt, inbound_protocol).await {
            Ok(completion) => accept(completion).await,
            Err(error) => Err(error),
        }
    }
    .instrument(span.clone())
    .await;
    record_call(&span, &result_status(result.as_ref().err()), started);
    result
}

fn member_span(hop: &Hop, index: usize, inbound_protocol: &str) -> tracing::Span {
    tracing::info_span!(
        "member",
        "plugin.id" = hop.member.plugin_id(),
        "preset.id" = hop.member.preset_id(),
        "member.id" = %hop.member.id(),
        attempt = index + 1,
        "protocol.in" = inbound_protocol,
        "protocol.out" = hop.member.protocol(),
        status = tracing::field::Empty,
        ttft_ms = tracing::field::Empty,
    )
}

fn record_call(span: &tracing::Span, status: &str, started: Instant) {
    let ttft_ms = u128_as_u64(started.elapsed().as_millis());
    span.record("status", status);
    span.record("ttft_ms", ttft_ms);
    metrics::counter!("hofund_member_requests").increment(1);
    metrics::histogram!("hofund_member_ttft_ms").record(ttft_ms as f64);
    metrics::histogram!("hofund_member_ttfb_ms").record(ttft_ms as f64);
    if status != "ok" {
        metrics::counter!("hofund_member_errors", "kind" => status.to_owned()).increment(1);
    }
}

fn result_status(error: Option<&PluginError>) -> String {
    match error {
        None => "ok".to_owned(),
        Some(error) => error.kind().to_string(),
    }
}

fn u128_as_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
