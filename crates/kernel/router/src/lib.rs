//! Ordered routing and failover across members.
//!
//! A model name maps to an ordered list of members. The next member is tried
//! when the current one fails before any content stream event or raw chunk has
//! been produced. A successful result also carries the untried route so the
//! gateway can resume it after a thinking-only refusal. This crate does not name vendors or the plugin
//! upstream and host types, and it does not read quirk data. It does not
//! depend on `tower`.
//! The daemon adapts each member into a service and wraps that service in the
//! kernel layers.

pub mod affinity;
mod affinity_reply;
mod attempt;
mod cooldown;
mod decision;
mod member;
mod patience;
pub mod quota;
mod scope;
mod snapshot;
pub mod strategy;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures_util::StreamExt;
use hofund_ir::{StreamEvent, estimate_input_tokens};
use hofund_plugin_api::{BodyStream, CountOutcome, Forward, PluginErrorKind, RawAnswer};
use tracing::Instrument;

pub use attempt::{Attempt, AttemptError, AttemptHook, MemberAttempt};
pub use cooldown::{CooldownCause, CooldownPolicy, Cooldowns, Cooling, PersistedRest};
pub use hofund_plugin_api::{Completion, MemberId, PluginError, Request};
pub use member::{Member, ModelListAnswer};
pub use patience::WaitHook;
pub use scope::RouteScope;
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
    /// This policy judges failures before a completion or first content stream
    /// event is returned (`reference: internal/gateway/fallback.go:1487-1517`).
    /// A gateway resuming a route makes the separate thinking-only refusal decision.
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
    /// Retry budget for a direct request to this member's provider. Absent
    /// disables patience. A request to a route pool uses [`Route::patience`]
    /// for every member instead.
    pub patience: Option<Duration>,
    /// Relative request share under weighted routing.
    pub weight: u64,
    /// Whether this model costs nothing on this member.
    pub free: bool,
    /// Whether the member's known allowance is nearly spent.
    pub nearly_spent: bool,
    /// How long this member's unread prompt cache stays warm.
    pub cold_after: Duration,
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
            patience: Some(Duration::from_secs(60)),
            weight: 1,
            free: false,
            nearly_spent: false,
            cold_after: affinity::CACHE_COLD,
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
    /// Conversation affinity for this pool.
    pub stays: affinity::Stays,
    /// How long a request to this pool waits for members that failed in a
    /// way that passes to come back. Absent disables the pool's patience.
    pub patience: Option<Duration>,
}

impl Route {
    /// A pool using smart routing, automatic affinity, and the default
    /// 60-second patience.
    #[must_use]
    pub fn new(hops: Vec<Hop>) -> Self {
        Self {
            hops,
            routing: "smart".to_owned(),
            stays: affinity::Stays::Auto,
            patience: Some(Duration::from_secs(60)),
        }
    }

    /// Sets the pool's patience. `None` turns it off.
    #[must_use]
    pub fn with_patience(mut self, patience: Option<Duration>) -> Self {
        self.patience = patience;
        self
    }

    /// Sets the registered strategy name.
    #[must_use]
    pub fn with_routing(mut self, routing: impl Into<String>) -> Self {
        self.routing = routing.into();
        self
    }

    /// Sets conversation affinity.
    #[must_use]
    pub fn with_stays(mut self, stays: affinity::Stays) -> Self {
        self.stays = stays;
        self
    }
}

/// A model-to-upstream table.
pub struct Router {
    routes: BTreeMap<String, Route>,
    providers: BTreeSet<String>,
    policy: FailoverPolicy,
    cooldowns: Option<Arc<Cooldowns>>,
    quotas: Option<Arc<quota::QuotaStore>>,
    strategies: Arc<strategy::StrategyState>,
    affinity: affinity::AffinityState,
    sealed: affinity::sealed::SealedState,
    measurements: Option<Arc<dyn strategy::Measurements>>,
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
    /// Configured route used for model-scope matching.
    route: String,
    /// Name sent when a hop has no wire model.
    bare: String,
    /// Stable state key for this route pool.
    pool: String,
    /// Strategy configured for this route pool.
    routing: String,
    /// Retry budget: the pool's for a route, the provider's for a direct
    /// request.
    patience: Option<Duration>,
    /// Whether the request named a route pool rather than one provider.
    pool_asked: bool,
    /// Per ordered hop: already resting out of its allowance (quota or
    /// credit) when the request was planned.
    spent: Vec<bool>,
    /// Conversation affinity configured for this route pool.
    stays: affinity::Stays,
}

struct Ordered {
    hops: Vec<Hop>,
    affinity: Option<ReplyRoute>,
}

struct ReplyRoute {
    key: String,
    turn: u64,
    strict: bool,
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
            quotas: None,
            strategies: Arc::new(strategy::StrategyState::with_builtins()),
            affinity: affinity::AffinityState::in_memory(),
            sealed: affinity::sealed::SealedState::default(),
            measurements: None,
        }
    }

    /// Shares the allowance view with later snapshots of this route table.
    #[must_use]
    pub fn with_quotas(mut self, quotas: Arc<quota::QuotaStore>) -> Self {
        self.quotas = Some(quotas);
        self
    }

    /// Shares affinity state with later snapshots of this route table.
    #[must_use]
    pub fn with_affinity(mut self, affinity: affinity::AffinityState) -> Self {
        self.sealed = affinity.sealed();
        self.affinity = affinity;
        self
    }

    /// Shares strategy rounds with later snapshots of this route table.
    #[must_use]
    pub fn with_strategies(mut self, strategies: Arc<strategy::StrategyState>) -> Self {
        self.strategies = strategies;
        self
    }

    /// Supplies live allowance, usage, speed, and price measurements.
    #[must_use]
    pub fn with_measurements(mut self, measurements: Arc<dyn strategy::Measurements>) -> Self {
        self.measurements = Some(measurements);
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
        self.models_scoped(None)
    }

    /// Model names with at least one upstream the caller may use.
    #[must_use]
    pub fn models_scoped(&self, scope: Option<&RouteScope>) -> Vec<String> {
        self.routes
            .iter()
            .filter(|(model, route)| {
                !route.hops.is_empty()
                    && scope.is_none_or(|scope| {
                        route
                            .hops
                            .iter()
                            .any(|hop| scope.allows_hop(model, model, hop))
                    })
            })
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
        self.dispatch_inner(request, inbound_protocol, None, None, hook, None)
            .await
    }

    /// Dispatches with conversation identity and optional attempt callbacks.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::dispatch`], or an attempt rejection.
    pub async fn dispatch_with_context(
        &self,
        request: &Request,
        inbound_protocol: &str,
        conversation: Option<&affinity::Conversation>,
        hook: Option<Arc<dyn AttemptHook>>,
    ) -> Result<Routed, RouterError> {
        self.dispatch_inner(request, inbound_protocol, None, conversation, hook, None)
            .await
    }

    /// Dispatches with per-caller scope filtering and optional attempt callbacks.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::dispatch`], or a scope refusal before
    /// any unauthorized member is contacted.
    pub async fn dispatch_scoped(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
        hook: Option<Arc<dyn AttemptHook>>,
    ) -> Result<Routed, RouterError> {
        self.dispatch_inner(request, inbound_protocol, scope, None, hook, None)
            .await
    }

    /// Dispatches with attempt callbacks and a notification before patience waits.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::dispatch_with_attempt`].
    pub async fn dispatch_with_wait(
        &self,
        request: &Request,
        inbound_protocol: &str,
        hook: Option<Arc<dyn AttemptHook>>,
        wait: Option<Arc<dyn WaitHook>>,
    ) -> Result<Routed, RouterError> {
        self.dispatch_inner(request, inbound_protocol, None, None, hook, wait)
            .await
    }

    /// Dispatches with per-caller scope filtering, attempt callbacks, and a
    /// notification before patience waits.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::dispatch_scoped`].
    pub async fn dispatch_scoped_with_wait(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
        hook: Option<Arc<dyn AttemptHook>>,
        wait: Option<Arc<dyn WaitHook>>,
    ) -> Result<Routed, RouterError> {
        self.dispatch_inner(request, inbound_protocol, scope, None, hook, wait)
            .await
    }

    /// Dispatches with per-caller scope filtering, conversation identity,
    /// attempt callbacks, and a notification before patience waits.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::dispatch_scoped`].
    pub async fn dispatch_scoped_conversation_with_wait(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
        conversation: Option<&affinity::Conversation>,
        hook: Option<Arc<dyn AttemptHook>>,
        wait: Option<Arc<dyn WaitHook>>,
    ) -> Result<Routed, RouterError> {
        self.dispatch_inner(request, inbound_protocol, scope, conversation, hook, wait)
            .await
    }

    async fn dispatch_inner(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
        conversation: Option<&affinity::Conversation>,
        hook: Option<Arc<dyn AttemptHook>>,
        wait_hook: Option<Arc<dyn WaitHook>>,
    ) -> Result<Routed, RouterError> {
        let asked = request.model.clone();
        let mut resolved = self.resolve(&asked)?;
        resolved.hops = scoped_hops(&asked, &resolved, scope)?;
        let ordered = self.order_for(&resolved, conversation);
        resolved.hops = ordered.hops;
        resolved.spent = self.spent(&resolved);
        let hook = conversation.map_or(hook.clone(), |conversation| {
            Some(Arc::new(affinity::sealed::SealedHook::new(
                self.sealed.clone(),
                conversation,
                hook.clone(),
            )) as Arc<dyn AttemptHook>)
        });
        DispatchState {
            asked,
            request: request.clone(),
            inbound_protocol: inbound_protocol.to_owned(),
            hook,
            resolved,
            policy: self.policy.clone(),
            input_tokens: estimate_input_tokens(request),
            last: None,
            refused_providers: BTreeSet::new(),
            outgrown: Vec::new(),
            needs_room: None,
            missing_detail: self.models().join(", "),
            wait_hook,
            affinity: ordered.affinity,
            affinity_state: self.affinity.clone(),
        }
        .run(0)
        .await
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
        self.count_scoped(request, inbound_protocol, None).await
    }

    /// Counts input using only members the caller may use.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::count`], or a scope refusal.
    pub async fn count_scoped(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
    ) -> Result<InputCount, RouterError> {
        let mut resolved = self.resolve(&request.model)?;
        resolved.hops = scoped_hops(&request.model, &resolved, scope)?;
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
        self.forward_scoped(call, None).await
    }

    /// Forwards using only members the caller may use.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::forward`], or a scope refusal.
    pub async fn forward_scoped(
        &self,
        call: &Forward,
        scope: Option<&RouteScope>,
    ) -> Result<Forwarded, RouterError> {
        let mut resolved = self.resolve(call.model())?;
        resolved.hops = scoped_hops(call.model(), &resolved, scope)?;
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
                .forward_for_model(call.path(), body, &upstream_model)
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

    /// Which ordered hops already rest out of their allowance, quota or
    /// credit, before this request asks them (`reference:
    /// internal/gateway/fallback.go:1230-1241`, `spentAfter`).
    fn spent(&self, resolved: &Resolved) -> Vec<bool> {
        resolved
            .hops
            .iter()
            .map(|hop| {
                let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
                self.cooldowns.as_ref().is_some_and(|cooldowns| {
                    cooldowns
                        .cooling_for_model(hop.member.id(), model)
                        .is_some_and(|cooling| {
                            matches!(
                                cooling.cause(),
                                CooldownCause::Quota | CooldownCause::Credit
                            )
                        })
                })
            })
            .collect()
    }

    fn order_hops(&self, resolved: &Resolved) -> Vec<Hop> {
        self.order_for(resolved, None).hops
    }

    fn order_for(
        &self,
        resolved: &Resolved,
        conversation: Option<&affinity::Conversation>,
    ) -> Ordered {
        let ordered = if resolved.hops.len() < 2 {
            resolved.hops.clone()
        } else {
            let mut seats = resolved
                .hops
                .iter()
                .map(|hop| {
                    let mut seat = strategy::Seat::new(hop.member.route_id());
                    seat.account = hop.member.id().account().is_some();
                    seat.learns = hop.member.learns_allowance();
                    seat.weight = hop.weight;
                    seat.free = hop.free;
                    let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
                    seat.resting = self.cooldowns.as_ref().is_some_and(|cooldowns| {
                        cooldowns
                            .cooling_for_model(hop.member.id(), model)
                            .is_some()
                    });
                    seat
                })
                .collect::<Vec<_>>();
            if let Some(measurements) = &self.measurements {
                let models = resolved
                    .hops
                    .iter()
                    .map(|hop| {
                        hop.wire_model
                            .clone()
                            .unwrap_or_else(|| resolved.bare.clone())
                    })
                    .collect::<Vec<_>>();
                measurements.weigh(
                    &mut seats,
                    &models,
                    &resolved.pool,
                    self.strategies.needs(&resolved.routing),
                    std::time::SystemTime::now(),
                );
            }
            self.strategies
                .order(&resolved.routing, &resolved.pool, &seats)
                .into_iter()
                .map(|index| resolved.hops[index].clone())
                .collect()
        };
        let decision = conversation.map(|conversation| {
            let now = SystemTime::now();
            let allowances = self
                .quotas
                .as_ref()
                .map(|quotas| quotas.allowances(now))
                .unwrap_or_default();
            let candidates = ordered
                .iter()
                .map(|hop| {
                    let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
                    let member = hop.member.id().to_string();
                    let nearly_spent = hop.nearly_spent
                        || allowances
                            .iter()
                            .find(|allowance| allowance.member == member)
                            .and_then(|allowance| allowance.fullest(model, now))
                            .is_some_and(|window| {
                                window.used >= affinity::spent_share_of(&resolved.routing)
                            });
                    affinity::Candidate {
                        member,
                        model: model.to_owned(),
                        resting: self.cooldowns.as_ref().is_some_and(|cooldowns| {
                            cooldowns
                                .cooling_for_model(hop.member.id(), model)
                                .is_some()
                        }),
                        nearly_spent,
                        cold_after: hop.cold_after,
                    }
                })
                .collect::<Vec<_>>();
            self.affinity
                .order(conversation, resolved.stays, &resolved.routing, &candidates)
        });
        let (ordered, strict) = if let Some(decision) = &decision {
            (
                decision
                    .order
                    .iter()
                    .map(|index| ordered[*index].clone())
                    .collect::<Vec<_>>(),
                decision.strict,
            )
        } else {
            (ordered, false)
        };
        let hops = if strict {
            ordered
        } else if let Some(cooldowns) = &self.cooldowns {
            order_hops(ordered, cooldowns, &resolved.bare)
        } else {
            ordered
        };
        Ordered {
            hops,
            affinity: conversation
                .zip(decision)
                .map(|(conversation, decision)| ReplyRoute {
                    key: conversation.key.clone(),
                    turn: conversation.turn,
                    strict: decision.strict,
                }),
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
                route: asked.to_owned(),
                bare: asked.to_owned(),
                pool: asked.to_owned(),
                routing: route.routing.clone(),
                stays: route.stays,
                patience: route.patience,
                pool_asked: true,
                spent: Vec::new(),
            });
        }
        if let Some((provider_id, model)) = asked.split_once('/')
            && self.providers.contains(provider_id)
        {
            let found = self.matching(|route_model, hop| {
                hop.member.route_id() == provider_id
                    && (route_model == model || hop.wire_model.as_deref() == Some(model))
            });
            let patience = found.first().and_then(|(_, hop, _)| hop.patience);
            let mut resolved = self.one(asked, model, found)?;
            resolved.patience = patience;
            return Ok(resolved);
        }
        let found = self.matching(|_, hop| hop.wire_model.as_deref() == Some(asked));
        let bare = found
            .first()
            .and_then(|(_, hop, _)| hop.wire_model.clone())
            .unwrap_or_else(|| asked.to_owned());
        self.one(asked, &bare, found)
    }

    fn matching(
        &self,
        mut pred: impl FnMut(&str, &Hop) -> bool,
    ) -> Vec<(String, Hop, affinity::Stays)> {
        let mut found = Vec::new();
        for (route_model, route) in &self.routes {
            for hop in &route.hops {
                if pred(route_model, hop) {
                    found.push((route_model.clone(), hop.clone(), route.stays));
                }
            }
        }
        found
    }

    fn one(
        &self,
        asked: &str,
        bare: &str,
        found: Vec<(String, Hop, affinity::Stays)>,
    ) -> Result<Resolved, RouterError> {
        match found.len() {
            0 => Err(self.missing(asked)),
            1 => {
                let (route, hop, stays) = found.into_iter().next().expect("one hop");
                Ok(Resolved {
                    hops: vec![hop],
                    route,
                    bare: bare.to_owned(),
                    pool: asked.to_owned(),
                    routing: "smart".to_owned(),
                    stays,
                    patience: None,
                    pool_asked: false,
                    spent: Vec::new(),
                })
            }
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

fn scoped_hops(
    asked: &str,
    resolved: &Resolved,
    scope: Option<&RouteScope>,
) -> Result<Vec<Hop>, RouterError> {
    let Some(scope) = scope else {
        return Ok(resolved.hops.clone());
    };
    let model_hops: Vec<Hop> = resolved
        .hops
        .iter()
        .filter(|hop| scope.allows_model(asked, &resolved.route, hop))
        .cloned()
        .collect();
    if model_hops.is_empty() && !scope.models.is_empty() {
        return Err(RouterError::ModelScope {
            model: asked.to_owned(),
            patterns: scope.models.clone(),
        });
    }
    let account_hops: Vec<Hop> = model_hops
        .into_iter()
        .filter(|hop| scope.allows_member(hop.member.as_ref()))
        .collect();
    if account_hops.is_empty() && !scope.accounts.is_empty() {
        return Err(RouterError::AccountScope {
            model: asked.to_owned(),
            accounts: scope.accounts.clone(),
        });
    }
    Ok(account_hops)
}

struct DispatchState {
    asked: String,
    request: Request,
    inbound_protocol: String,
    hook: Option<Arc<dyn AttemptHook>>,
    resolved: Resolved,
    policy: FailoverPolicy,
    input_tokens: u64,
    last: Option<(String, PluginError)>,
    refused_providers: BTreeSet<String>,
    outgrown: Vec<(String, String)>,
    needs_room: Option<u64>,
    missing_detail: String,
    wait_hook: Option<Arc<dyn WaitHook>>,
    /// Where an answer's stick is recorded, when the request has a conversation.
    affinity: Option<ReplyRoute>,
    affinity_state: affinity::AffinityState,
}

/// One member's answer within a dispatch round.
struct Answered {
    index: usize,
    upstream_id: String,
    upstream_model: String,
    completion: Completion,
}

impl DispatchState {
    fn now(&self) -> Instant {
        self.wait_hook
            .as_ref()
            .map_or_else(Instant::now, |hook| hook.now())
    }

    async fn run(mut self, start: usize) -> Result<Routed, RouterError> {
        let mut round = 0_u32;
        let mut wait_from = None;
        let mut asking: Vec<usize> = (start..self.resolved.hops.len()).collect();
        loop {
            let mut busy = Vec::new();
            let mut ending = Ending::Answered;
            if let Some(answered) = self.round(&asking, &mut busy, &mut ending).await? {
                let next = answered.index + 1;
                let continuation = (next < self.resolved.hops.len())
                    .then(|| RouteContinuation { state: self, next });
                return Ok(Routed {
                    upstream_id: answered.upstream_id,
                    upstream_model: answered.upstream_model,
                    completion: answered.completion,
                    continuation,
                });
            }
            let now = self.now();
            // Every member asked has failed with nothing of a reply sent.
            // The last one's failure passes (busy), or, in a pool, it is
            // newly out of its allowance after another member was busy:
            // those busy are asked again in turn after a pause, while the
            // patience lasts, counted from the first time all had failed
            // (`reference: internal/gateway/gateway.go:3956-4006`).
            let last_spent =
                self.resolved.pool_asked && !busy.is_empty() && ending == Ending::Spent;
            if (ending == Ending::Busy || last_spent)
                && let Some(budget) = self.resolved.patience
                && let Some(hook) = self.wait_hook.clone()
            {
                let from = *wait_from.get_or_insert(now);
                if let Some((next, wait)) = patience::replan(&mut busy, round, from, budget, now) {
                    hook.waiting();
                    hook.wait(wait).await;
                    round = round.saturating_add(1);
                    asking = next;
                    continue;
                }
            }
            break;
        }
        match self.last {
            Some((upstream_id, error)) => Err(RouterError::Upstream { upstream_id, error }),
            None => {
                let detail = if self.missing_detail.is_empty() {
                    String::new()
                } else {
                    format!(". Routes: {}", self.missing_detail)
                };
                Err(RouterError::NoRoute {
                    model: self.request.model,
                    detail,
                })
            }
        }
    }

    /// Asks each member in `asking` once, in that order. `Some` is an
    /// answer; `None` means every member failed in a way that may try the
    /// next one. `ending` tells how the last member asked failed.
    async fn round(
        &mut self,
        asking: &[usize],
        busy: &mut Vec<patience::Busy>,
        ending: &mut Ending,
    ) -> Result<Option<Answered>, RouterError> {
        for &index in asking {
            let hop = self.resolved.hops[index].clone();
            let mut attempt = self.request.clone();
            apply_wire(&mut attempt, &hop, &self.resolved.bare);
            let upstream_model = attempt.model.clone();
            if self.refused_providers.contains(hop.member.route_id())
                || self.outgrown.iter().any(|(route, model)| {
                    route == hop.member.route_id() && *model == upstream_model
                })
                || self.needs_room.is_some_and(|tokens| {
                    hop.context_window
                        .is_some_and(|window| window > 0 && tokens >= window)
                })
            {
                continue;
            }
            let info = Attempt {
                member: hop.member.attempt(),
                inbound_protocol: self.inbound_protocol.clone(),
            };
            if let Some(hook) = self.hook.as_ref()
                && let Err(error) = hook.before(&info)
            {
                return Err(match error {
                    AttemptError::PreviousResponseNotFound { id } => {
                        RouterError::PreviousResponseNotFound { id }
                    }
                });
            }
            let result = call_member(
                &hop,
                index,
                &self.inbound_protocol,
                attempt,
                self.hook.clone(),
            )
            .await;
            match result {
                Ok(completion) => {
                    if let Some(hook) = self.hook.as_ref() {
                        hook.answered(&info);
                    }
                    let completion = if self.asked == upstream_model {
                        completion
                    } else {
                        label_model(completion, self.asked.clone())
                    };
                    let completion = if let Some(affinity) = &self.affinity {
                        affinity_reply::watch(
                            completion,
                            self.affinity_state.clone(),
                            affinity.key.clone(),
                            affinity.turn,
                            affinity.strict,
                            hop.member.id().to_string(),
                            upstream_model.clone(),
                        )
                    } else {
                        completion
                    };
                    return Ok(Some(Answered {
                        index,
                        upstream_id: hop.member.route_id().to_owned(),
                        upstream_model,
                        completion,
                    }));
                }
                Err(error) => {
                    let upstream_id = hop.member.route_id().to_owned();
                    let inferred = decision::too_long_unsaid(
                        &error,
                        self.input_tokens,
                        hop.context_window,
                        &upstream_model,
                        &upstream_id,
                    );
                    let error = inferred.unwrap_or(error);
                    if decision::outgrown(&error) {
                        // As long on every account of this model; another
                        // member not known to be too small may hold it.
                        if error.status() != Some(413) {
                            self.needs_room = Some(self.input_tokens);
                        }
                        self.outgrown.push((upstream_id.clone(), upstream_model));
                        self.last = Some((upstream_id, error));
                        *ending = Ending::Other;
                        continue;
                    }
                    if decision::skips_provider(&error) {
                        // Every account of the provider refuses the same
                        // content; another provider is asked once.
                        self.refused_providers.insert(upstream_id.clone());
                    } else if !decision::asks_next(&error) && !self.policy.allows(&error) {
                        return Err(RouterError::Upstream { upstream_id, error });
                    }
                    if let Some(failure) = patience::busy(&error, index, self.now()) {
                        // Each member is asked once a round, so noted once.
                        busy.push(failure);
                        *ending = Ending::Busy;
                    } else if patience::spent(&error)
                        && !self.resolved.spent.get(index).copied().unwrap_or(false)
                    {
                        *ending = Ending::Spent;
                    } else {
                        *ending = Ending::Other;
                    }
                    self.last = Some((upstream_id, error));
                }
            }
        }
        Ok(None)
    }
}

/// How the last member asked in a round failed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// No member failed yet, or one answered.
    Answered,
    /// In a way that passes: busy, overloaded, rate limited for a moment.
    Busy,
    /// Newly out of its allowance (quota or credit), not resting so already.
    Spent,
    /// In a way that does not pass.
    Other,
}

/// The untried remainder of a route after a streamed member refuses.
pub struct RouteContinuation {
    state: DispatchState,
    next: usize,
}

impl RouteContinuation {
    /// Asks the remaining members in their existing routing order.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Router::dispatch`].
    pub async fn dispatch(self) -> Result<Routed, RouterError> {
        self.state.run(self.next).await
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
    continuation: Option<RouteContinuation>,
}

impl Routed {
    /// Takes the untried remainder, leaving no continuation in this result.
    pub fn take_continuation(&mut self) -> Option<RouteContinuation> {
        self.continuation.take()
    }
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
    /// Every candidate is outside the caller's model scope.
    #[error("gateway key is not allowed to use model `{model}`; allowed models: {}", patterns.join(", "))]
    ModelScope {
        /// The requested model.
        model: String,
        /// The caller's model patterns.
        patterns: Vec<String>,
    },
    /// Every candidate is outside the caller's account scope.
    #[error("gateway key is not allowed to use the accounts behind model `{model}`; allowed accounts: {}", accounts.join(", "))]
    AccountScope {
        /// The requested model.
        model: String,
        /// The caller's account patterns.
        accounts: Vec<String>,
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

fn order_hops(hops: Vec<Hop>, cooldowns: &Cooldowns, bare: &str) -> Vec<Hop> {
    let mut ready = Vec::with_capacity(hops.len());
    let mut cooling = Vec::new();
    for hop in hops {
        let model = hop.wire_model.as_deref().unwrap_or(bare);
        if let Some(state) = cooldowns.cooling_for_model(hop.member.id(), model) {
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

fn candidates(found: &[(String, Hop, affinity::Stays)]) -> String {
    found
        .iter()
        .map(|(route, hop, _)| format!("{}/{}", hop.member.route_id(), route))
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
