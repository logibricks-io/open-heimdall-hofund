//! Ordered routing and failover across members.
//!
//! A model name maps to an ordered list of members. The next member is tried
//! only when the current one fails before any content stream event or raw
//! chunk has been produced. This crate does not name vendors or the plugin
//! upstream and host types, and it does not read quirk data. It does not
//! depend on `tower`.
//! The daemon adapts each member into a service and wraps that service in the
//! kernel layers.

mod attempt;
mod cooldown;
mod decision;
mod member;
mod snapshot;
pub mod strategy;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use hofund_ir::{StreamEvent, estimate_input_tokens};
use hofund_plugin_api::{BodyStream, CountOutcome, Forward, PluginErrorKind, RawAnswer};
use tracing::Instrument;

pub use attempt::{Attempt, AttemptError, AttemptHook, MemberAttempt};
pub use cooldown::{CooldownCause, CooldownPolicy, Cooldowns, Cooling};
pub use hofund_plugin_api::{Completion, MemberId, PluginError, Request};
pub use member::Member;
pub use snapshot::Snapshot;

/// Which failures may try the next member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailoverPolicy {
    /// Connect, timeout, transport, and interrupted-body errors try the next member.
    pub connect_errors: bool,
    /// HTTP statuses that try the next member. Body-word classes are independent.
    pub statuses: BTreeSet<u16>,
}

impl Default for FailoverPolicy {
    fn default() -> Self {
        Self {
            connect_errors: true,
            statuses: [401, 402, 403, 404, 408, 429]
                .into_iter()
                .chain(500..=599)
                .collect(),
        }
    }
}

impl FailoverPolicy {
    /// Whether `error` happened early enough, and is of a kind, that another
    /// member may be tried.
    ///
    /// Overflow is ruled out first. Listed statuses, transport failures, credit
    /// refusals, and the body-word classes then follow their independent rules.
    /// Failover occurs only before a completion or first content stream event
    /// is returned (`reference: internal/gateway/fallback.go:1487-1517`).
    #[must_use]
    pub fn allows(&self, error: &PluginError) -> bool {
        decision::allows(error, self.connect_errors, &self.statuses)
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
    /// Input context window, when the route or catalog knows it.
    pub context_window: Option<u64>,
    /// Relative request share under weighted routing.
    pub weight: u64,
    /// Whether this model costs nothing on this member.
    pub free: bool,
}

impl Hop {
    /// A hop with no model overrides, weight one, and no free-model marking.
    #[must_use]
    pub fn new(member: Arc<dyn Member>) -> Self {
        Self {
            member,
            wire_model: None,
            max_output_tokens: None,
            context_window: None,
            weight: 1,
            free: false,
        }
    }
}

/// One named route pool.
#[derive(Clone)]
pub struct Route {
    /// Members in configured order.
    pub hops: Vec<Hop>,
    /// Registered routing strategy name.
    pub routing: String,
}

impl Route {
    /// A pool using smart routing.
    #[must_use]
    pub fn new(hops: Vec<Hop>) -> Self {
        Self {
            hops,
            routing: "smart".to_owned(),
        }
    }

    /// Sets the registered strategy name.
    #[must_use]
    pub fn with_routing(mut self, routing: impl Into<String>) -> Self {
        self.routing = routing.into();
        self
    }
}

/// A model-to-upstream table.
pub struct Router {
    routes: BTreeMap<String, Route>,
    providers: BTreeSet<String>,
    policy: FailoverPolicy,
    cooldowns: Option<Arc<Cooldowns>>,
    strategies: Arc<strategy::StrategyState>,
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
    /// Stable state key for this route pool.
    pool: String,
    /// Strategy configured for this route pool.
    routing: String,
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
                let hops = members.into_iter().map(Hop::new).collect();
                (model, hops)
            })
            .collect();
        Self::from_hops(routes, policy)
    }

    /// Builds a smart-routing router from hops that may rename the model per member.
    #[must_use]
    pub fn from_hops(routes: BTreeMap<String, Vec<Hop>>, policy: FailoverPolicy) -> Self {
        let routes = routes
            .into_iter()
            .map(|(model, hops)| (model, Route::new(hops)))
            .collect();
        Self::from_routes(routes, policy)
    }

    /// Builds a router from named route pools.
    #[must_use]
    pub fn from_routes(routes: BTreeMap<String, Route>, policy: FailoverPolicy) -> Self {
        let mut providers = BTreeSet::new();
        for route in routes.values() {
            for hop in &route.hops {
                providers.insert(hop.member.route_id().to_owned());
            }
        }
        Self {
            routes,
            providers,
            policy,
            cooldowns: None,
            strategies: Arc::new(strategy::StrategyState::with_builtins()),
        }
    }

    /// Shares strategy rounds with later snapshots of this route table.
    #[must_use]
    pub fn with_strategies(mut self, strategies: Arc<strategy::StrategyState>) -> Self {
        self.strategies = strategies;
        self
    }

    /// Shares cooldown state with this router and later snapshots.
    #[must_use]
    pub fn with_cooldowns(mut self, cooldowns: Arc<Cooldowns>) -> Self {
        self.cooldowns = Some(cooldowns);
        self
    }

    /// Cooldown state used to order members, when configured.
    #[must_use]
    pub const fn cooldowns(&self) -> Option<&Arc<Cooldowns>> {
        self.cooldowns.as_ref()
    }

    /// Member ids on the current routes, in first-seen order.
    #[must_use]
    pub fn member_ids(&self) -> Vec<MemberId> {
        let mut seen = BTreeSet::new();
        let mut ids = Vec::new();
        for route in self.routes.values() {
            for hop in &route.hops {
                if seen.insert(hop.member.id().to_string()) {
                    ids.push(hop.member.id().clone());
                }
            }
        }
        ids
    }

    /// Model names that have at least one upstream, in order.
    #[must_use]
    pub fn models(&self) -> Vec<String> {
        self.routes
            .iter()
            .filter(|(_, route)| !route.hops.is_empty())
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
        self.dispatch_with_attempt(request, inbound_protocol, None)
            .await
    }

    /// Dispatches with callbacks for each concrete member attempt.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::dispatch`], or an attempt rejection.
    pub async fn dispatch_with_attempt(
        &self,
        request: &Request,
        inbound_protocol: &str,
        hook: Option<Arc<dyn AttemptHook>>,
    ) -> Result<Routed, RouterError> {
        let asked = request.model.clone();
        let mut resolved = self.resolve(&asked)?;
        resolved.hops = self.order_hops(&resolved);
        let policy = self.policy.clone();
        let input_tokens = estimate_input_tokens(request);
        let mut last = None;
        let mut refused_providers = BTreeSet::new();
        let mut outgrown = Vec::<(String, String)>::new();
        let mut needs_room = None;
        for (index, hop) in resolved.hops.iter().enumerate() {
            let mut attempt = request.clone();
            apply_wire(&mut attempt, hop, &resolved.bare);
            let upstream_model = attempt.model.clone();
            if refused_providers.contains(hop.member.route_id())
                || outgrown.iter().any(|(route, model)| {
                    route == hop.member.route_id() && *model == upstream_model
                })
                || needs_room.is_some_and(|tokens| {
                    hop.context_window
                        .is_some_and(|window| window > 0 && tokens >= window)
                })
            {
                continue;
            }
            let info = Attempt {
                member: hop.member.attempt(),
                inbound_protocol: inbound_protocol.to_owned(),
            };
            if let Some(hook) = hook.as_ref()
                && let Err(error) = hook.before(&info)
            {
                return Err(match error {
                    AttemptError::PreviousResponseNotFound { id } => {
                        RouterError::PreviousResponseNotFound { id }
                    }
                });
            }
            let result = call_member(hop, index, inbound_protocol, attempt, hook.clone()).await;
            match result {
                Ok(completion) => {
                    if let Some(hook) = hook.as_ref() {
                        hook.answered(&info);
                    }
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
                Err(error) => {
                    let upstream_id = hop.member.route_id().to_owned();
                    let inferred = decision::too_long_unsaid(
                        &error,
                        input_tokens,
                        hop.context_window,
                        &upstream_model,
                        &upstream_id,
                    );
                    let error = inferred.unwrap_or(error);
                    if decision::outgrown(&error) {
                        // As long on every account of this model; another
                        // member not known to be too small may hold it.
                        if error.status() != Some(413) {
                            needs_room = Some(input_tokens);
                        }
                        outgrown.push((upstream_id.clone(), upstream_model));
                        last = Some((upstream_id, error));
                        continue;
                    }
                    if decision::skips_provider(&error) {
                        // Every account of the provider refuses the same
                        // content; another provider is asked once.
                        refused_providers.insert(upstream_id.clone());
                    } else if !decision::asks_next(&error) && !policy.allows(&error) {
                        return Err(RouterError::Upstream { upstream_id, error });
                    }
                    last = Some((upstream_id, error));
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
        let mut resolved = self.resolve(&request.model)?;
        resolved.hops = self.order_hops(&resolved);
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

    /// Forwards `call` to the members of its model in routing order.
    ///
    /// Each member gets the body built for the model name it expects. A
    /// member that does not serve the path, which is `unsupported` or a
    /// `404`, `405` or `501`, passes the call to the next one, as does a
    /// failure the policy retries.
    ///
    /// # Errors
    ///
    /// Returns [`RouterError::NoRoute`] or [`RouterError::Ambiguous`] for the
    /// same model names as [`Self::dispatch`]. Returns [`RouterError::Upstream`]
    /// with the last member's error when none answered, or at once when the
    /// body cannot be built or a member fails in a way that is not retried.
    pub async fn forward(&self, call: &Forward) -> Result<Forwarded, RouterError> {
        let mut resolved = self.resolve(call.model())?;
        resolved.hops = self.order_hops(&resolved);
        let mut last = None;
        for (index, hop) in resolved.hops.iter().enumerate() {
            let upstream_model = hop
                .wire_model
                .clone()
                .unwrap_or_else(|| resolved.bare.clone());
            let upstream_id = hop.member.route_id().to_owned();
            let body = call
                .body_for(&upstream_model)
                .map_err(|error| RouterError::Upstream {
                    upstream_id: upstream_id.clone(),
                    error,
                })?;
            let span = member_span(hop, index, "forward");
            let started = Instant::now();
            let result = hop
                .member
                .forward(call.path(), body)
                .instrument(span.clone())
                .await;
            record_call(&span, &result_status(result.as_ref().err()), started);
            match result {
                Ok(answer) => {
                    return Ok(Forwarded {
                        upstream_id,
                        upstream_model,
                        answer,
                    });
                }
                Err(error) if not_served(&error) || self.policy.allows(&error) => {
                    last = Some((upstream_id, error));
                }
                Err(error) => return Err(RouterError::Upstream { upstream_id, error }),
            }
        }
        match last {
            Some((upstream_id, error)) => Err(RouterError::Upstream { upstream_id, error }),
            None => Err(self.missing(call.model())),
        }
    }

    fn order_hops(&self, resolved: &Resolved) -> Vec<Hop> {
        let ordered = if resolved.hops.len() < 2 {
            resolved.hops.clone()
        } else {
            let seats = resolved
                .hops
                .iter()
                .map(|hop| {
                    let mut seat = strategy::Seat::new(hop.member.id().to_string());
                    seat.weight = hop.weight;
                    seat.free = hop.free;
                    seat.resting = self
                        .cooldowns
                        .as_ref()
                        .is_some_and(|cooldowns| cooldowns.cooling(hop.member.id()).is_some());
                    seat
                })
                .collect::<Vec<_>>();
            self.strategies
                .order(&resolved.routing, &resolved.pool, &seats)
                .into_iter()
                .map(|index| resolved.hops[index].clone())
                .collect()
        };
        if let Some(cooldowns) = &self.cooldowns {
            order_hops(ordered, cooldowns)
        } else {
            ordered
        }
    }

    fn resolve(&self, asked: &str) -> Result<Resolved, RouterError> {
        if let Some(route) = self
            .routes
            .get(asked)
            .filter(|route| !route.hops.is_empty())
        {
            return Ok(Resolved {
                hops: route.hops.clone(),
                bare: asked.to_owned(),
                pool: asked.to_owned(),
                routing: route.routing.clone(),
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
        for (route_model, route) in &self.routes {
            for hop in &route.hops {
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
                pool: asked.to_owned(),
                routing: "smart".to_owned(),
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
    /// The completion. A stream has produced its first content event, a raw
    /// body its first chunk, or either ended empty. An earlier failure is not returned here.
    pub completion: Completion,
}

/// A forwarded call a member answered.
#[derive(Debug)]
pub struct Forwarded {
    /// Id of the provider that answered.
    pub upstream_id: String,
    /// Model name that provider was sent.
    pub upstream_model: String,
    /// The provider's answer.
    pub answer: RawAnswer,
}

/// Whether `error` says the member has no such endpoint, rather than that the
/// call failed there.
fn not_served(error: &PluginError) -> bool {
    error.kind() == PluginErrorKind::Unsupported
        || (error.kind() == PluginErrorKind::Status
            && matches!(error.status(), Some(404 | 405 | 501)))
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
    /// An attempt cannot resolve the response id the client named.
    #[error("Previous response with id '{id}' not found.")]
    PreviousResponseNotFound {
        /// Client-supplied response id.
        id: String,
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

fn order_hops(hops: Vec<Hop>, cooldowns: &Cooldowns) -> Vec<Hop> {
    let mut ready = Vec::with_capacity(hops.len());
    let mut cooling = Vec::new();
    for hop in hops {
        if let Some(state) = cooldowns.cooling(hop.member.id()) {
            cooling.push((hop, state.until()));
        } else {
            ready.push(hop);
        }
    }
    let mut probe = None;
    for (index, (_, until)) in cooling.iter().enumerate() {
        if probe.is_none_or(|(_, soonest): (usize, Instant)| *until < soonest) {
            probe = Some((index, *until));
        }
    }
    let probe = probe.map(|(index, _)| index);
    for (index, (hop, until)) in cooling.into_iter().enumerate() {
        if probe == Some(index) {
            ready.push(hop);
        } else {
            tracing::debug!(
                "member.id" = %hop.member.id(),
                "cooldown.until" = ?until,
                "member_cooldown_skip"
            );
        }
    }
    ready
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
        Completion::Stream(mut stream) => {
            let mut held = Vec::new();
            while let Some(item) = stream.next().await {
                let event = item?;
                let commits = stream_commits(&event);
                held.push(event);
                if commits {
                    let head = futures_util::stream::iter(held.into_iter().map(Ok));
                    return Ok(Completion::Stream(Box::pin(head.chain(stream))));
                }
            }
            Ok(Completion::Stream(Box::pin(futures_util::stream::iter(
                held.into_iter().map(Ok),
            ))))
        }
        Completion::Raw(mut body) => match body.next_chunk().await {
            None => Ok(Completion::Raw(body)),
            Some(Err(error)) => Err(error),
            Some(Ok(chunk)) => Ok(Completion::Raw(BodyStream::resume(chunk, body))),
        },
        other => Ok(other),
    }
}

fn stream_commits(event: &StreamEvent) -> bool {
    match event {
        StreamEvent::MessageStart(_)
        | StreamEvent::ContentBlockStart(_)
        | StreamEvent::ContentBlockStop(_)
        | StreamEvent::Ping(_) => false,
        StreamEvent::MessageDelta(delta) => delta.stop.is_some(),
        _ => true,
    }
}

async fn call_member(
    hop: &Hop,
    index: usize,
    inbound_protocol: &str,
    attempt: Request,
    hook: Option<Arc<dyn AttemptHook>>,
) -> Result<Completion, PluginError> {
    let span = member_span(hop, index, inbound_protocol);
    let started = Instant::now();
    let result = async {
        match hop
            .member
            .execute_with_attempt(attempt, inbound_protocol, hook)
            .await
        {
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
