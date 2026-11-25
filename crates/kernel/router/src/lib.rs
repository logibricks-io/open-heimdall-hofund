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
mod api_fitness;
pub mod at_once;
mod attempt;
mod cooldown;
pub mod credits_mode;
mod decision;
mod member;
mod overflow_memory;
mod patience;
pub mod quota;
pub mod quota_alerts;
mod scope;
mod snapshot;
pub mod strategy;
pub mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::StreamExt;
use hofund_codec::responses_empty::{
    ChatStreamCheck, EMPTY_REPLY_MESSAGE, chat_response_empty, decoded_chat_stream_empty,
    decoded_stream_empty, empty_reply_message, relayed_sse_empty_without_content,
    relayed_sse_event_boundary, relayed_sse_has_content, relayed_sse_has_opaque_content,
    relayed_sse_lifecycle_only,
};
use hofund_control_proto::{RouteTry, SkipReason};
use hofund_ir::{StreamEvent, estimate_input_tokens};
use hofund_plugin_api::{BodyStream, CountOutcome, Forward, PluginErrorKind, RawAnswer};
use tracing::Instrument;

pub use api_fitness::ApiFitness;
pub use attempt::{Attempt, AttemptError, AttemptHook, MemberAttempt};
use cooldown::HandOnRest;
pub use cooldown::{CooldownCause, CooldownPolicy, Cooldowns, Cooling, PersistedRest};
pub use decision::{region_agent_note, with_region_agent_note};
pub use hofund_plugin_api::{Completion, MemberId, PluginError, Request};
pub use member::{Member, ModelListAnswer};
pub use overflow_memory::OverflowMemory;
pub use patience::{PoolMemberPatience, WaitHook};
pub use scope::{RouteScope, model_allowed};
pub use snapshot::Snapshot;
pub use view::{
    RouteMeta, RouteTraceBook, RouteTraceHandle, TRACE_KEEP, TraceNow, UNROUTED_MOST,
    UNROUTED_PHASE_MASKING, UNROUTED_SHOW, UNROUTED_UNKNOWN, UnroutedHold, UnroutedTimeout,
    candidates_from_hops, fail_text, try_status, unrouted_timeout_status,
};

/// Returns `true` when a hop should be skipped (for example a disabled model
/// or a key that has reached its spend cap).
///
/// Arguments are the route name, the asked model, the provider id, the wire
/// model, and the provider key fingerprint. The fingerprint is empty when the
/// member has no API key.
pub type MemberSkipFn = Arc<dyn Fn(&str, &str, &str, &str, &str) -> bool + Send + Sync>;

/// One candidate the spend-cap exhausted hook may refuse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpendCapCandidate {
    /// Provider id for this hop.
    pub provider: String,
    /// Key fingerprint. Empty when the member has no API key.
    pub key_id: String,
}

/// When every candidate hop was removed and each is spend-capped, returns
/// `Some(retry_after)` with `None` inside for a lifetime cap.
pub type SpendCapExhaustedFn =
    Arc<dyn Fn(&[SpendCapCandidate]) -> Option<Option<u64>> + Send + Sync>;

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

/// One routing-unit boundary above a hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// Stable member key within the parent pool.
    pub key: String,
    /// Stable state key for this unit's own routing pool.
    pub pool: String,
    /// Strategy used to order this unit's immediate children.
    pub routing: String,
    /// This unit's share in its parent pool.
    pub weight: u64,
    /// Whether rate-limited children move behind the others in this unit.
    pub sink: bool,
    /// Whether this unit was reached through a provider fallback list.
    pub fallback: bool,
    /// Explicit patience on a `group/` unit; absent means inherit.
    pub group_patience: Option<PoolMemberPatience>,
}

/// What a member needs to open one realtime session.
///
/// `headers` carry the upstream credential. Debug output redacts them and the
/// proxy URL, which may contain a password.
pub struct RealtimeDial {
    /// WebSocket URL, including the model query.
    pub url: String,
    /// Header names and values. Values must not be logged.
    pub headers: Vec<(String, String)>,
    /// HTTP proxy URL from the member profile. `None` connects directly.
    /// `Some` must be used; a value that cannot be dialed fails the session
    /// instead of opening a direct socket.
    pub proxy: Option<String>,
}

impl std::fmt::Debug for RealtimeDial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let url = self
            .url
            .split_once('?')
            .map_or(self.url.clone(), |(base, _)| format!("{base}?<redacted>"));
        formatter
            .debug_struct("RealtimeDial")
            .field("url", &url)
            .field("headers", &self.headers.len())
            .field("proxy", &self.proxy.is_some())
            .finish()
    }
}

/// Hops a realtime session may dial for one resolved model.
#[derive(Clone)]
pub struct Candidates {
    /// Configured route name.
    pub route: String,
    /// Model name used when a hop has no wire model.
    pub model: String,
    /// Hops that passed the caller's scope, in configured order.
    pub hops: Vec<Hop>,
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
    /// Whether this row is a provider fallback rather than its own member.
    pub fallback: bool,
    /// Fixed reasoning effort for affinity identity, empty when not fixed.
    pub effort: String,
    /// Service tier this member sends. Empty keeps the client's tier.
    /// `priority` is Fast. `ultrafast` is Ultrafast.
    pub service_tier: String,
    /// How long this member's unread prompt cache stays warm.
    pub cold_after: Duration,
    /// Unit boundaries from the outermost direct member to the innermost
    /// subscription. Empty keeps legacy flat-route behavior.
    pub units: Vec<Unit>,
}

impl Hop {
    /// Patience budget for this hop when the request names a route pool.
    pub fn pool_member_patience(&self) -> PoolMemberPatience {
        for unit in self.units.iter().rev() {
            if unit.pool.starts_with("group/")
                && let Some(patience) = unit.group_patience
            {
                return patience;
            }
        }
        PoolMemberPatience::Inherited
    }
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
            fallback: false,
            effort: String::new(),
            service_tier: String::new(),
            cold_after: affinity::CACHE_COLD,
            units: Vec::new(),
        }
    }
}

/// One named route pool.
#[derive(Clone)]
pub struct Route {
    /// Members in configured order.
    pub hops: Vec<Hop>,
    /// Accounts-at-once metadata by stable member id.
    pub at_once: BTreeMap<String, at_once::AtOnceMember>,
    /// Registered routing strategy name.
    pub routing: String,
    /// Conversation affinity for this pool.
    pub stays: affinity::Stays,
    /// How long a request to this pool waits for members that failed in a
    /// way that passes to come back. Absent disables the pool's patience.
    pub patience: Option<Duration>,
    /// Whether rate-limited members with quota left move behind the others.
    pub sink: bool,
}

impl Route {
    /// A pool using smart routing, automatic affinity, and the default
    /// 60-second patience.
    #[must_use]
    pub fn new(hops: Vec<Hop>) -> Self {
        Self {
            hops,
            at_once: BTreeMap::new(),
            routing: "smart".to_owned(),
            stays: affinity::Stays::Auto,
            patience: Some(Duration::from_secs(60)),
            sink: false,
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

    /// Sets whether rate-limited members move behind the others.
    #[must_use]
    pub fn with_sink(mut self, sink: bool) -> Self {
        self.sink = sink;
        self
    }

    /// Sets accounts-at-once metadata by stable member id.
    #[must_use]
    pub fn with_at_once(mut self, at_once: BTreeMap<String, at_once::AtOnceMember>) -> Self {
        self.at_once = at_once;
        self
    }
}

/// Total time a route pool may spend across its members on one request.
pub const GROUP_WALK_BUDGET: Duration = Duration::from_secs(540);

/// A model-to-upstream table.
pub struct Router {
    routes: BTreeMap<String, Route>,
    providers: BTreeSet<String>,
    policy: FailoverPolicy,
    cooldowns: Option<Arc<Cooldowns>>,
    quotas: Option<Arc<quota::QuotaStore>>,
    credits_modes: Option<Arc<credits_mode::CreditsModeStore>>,
    strategies: Arc<strategy::StrategyState>,
    affinity: affinity::AffinityState,
    sealed: affinity::sealed::SealedState,
    overflow: OverflowMemory,
    api_fitness: ApiFitness,
    measurements: Option<Arc<dyn strategy::Measurements>>,
    member_skip: Option<MemberSkipFn>,
    spend_cap_exhausted: Option<SpendCapExhaustedFn>,
    group_walk_budget: Duration,
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
    at_once: BTreeMap<String, at_once::AtOnceMember>,
    /// Configured route used for model-scope matching.
    route: String,
    /// Name sent when a hop has no wire model.
    bare: String,
    /// Stable state key for this route pool.
    pool: String,
    /// Strategy configured for this route pool.
    routing: String,
    /// Whether rate-limited members move behind the others in this pool.
    sink: bool,
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

impl Resolved {
    fn uses_free(&self) -> bool {
        self.routing == "free"
            || self
                .hops
                .iter()
                .flat_map(|hop| &hop.units)
                .any(|unit| unit.routing == "free")
    }
}

struct Ordered {
    hops: Vec<Hop>,
    /// Other APIs of a member, asked only after that member's API is refused.
    protocol_alternates: Vec<Hop>,
    hand_on: Vec<Hop>,
    reserves: BTreeMap<String, usize>,
    affinity: Option<ReplyRoute>,
    drain_moves: Vec<RoundMove>,
}

struct RoundMove {
    pool: String,
    picked: String,
    choices: Vec<(String, String)>,
}

#[derive(Default)]
struct PoolMoves {
    weight: Vec<RoundMove>,
    drain: Vec<RoundMove>,
}

struct ReplyRoute {
    key: String,
    turn: u64,
    strict: bool,
    planned: Vec<affinity::Candidate>,
    reason: &'static str,
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
            credits_modes: None,
            strategies: Arc::new(strategy::StrategyState::with_builtins()),
            affinity: affinity::AffinityState::in_memory(),
            sealed: affinity::sealed::SealedState::default(),
            overflow: OverflowMemory::in_memory(),
            api_fitness: ApiFitness::default(),
            measurements: None,
            member_skip: None,
            spend_cap_exhausted: None,
            group_walk_budget: GROUP_WALK_BUDGET,
        }
    }

    /// Overrides the serial walk budget used when a route has several members.
    #[must_use]
    pub fn with_group_walk_budget(mut self, budget: Duration) -> Self {
        self.group_walk_budget = budget;
        self
    }

    /// Shares the allowance view with later snapshots of this route table.
    #[must_use]
    pub fn with_quotas(mut self, quotas: Arc<quota::QuotaStore>) -> Self {
        self.quotas = Some(quotas);
        self
    }

    /// Shares operator credits modes with later snapshots of this route table.
    #[must_use]
    pub fn with_credits_modes(mut self, modes: Arc<credits_mode::CreditsModeStore>) -> Self {
        self.credits_modes = Some(modes);
        self
    }

    /// Shares affinity state with later snapshots of this route table.
    #[must_use]
    pub fn with_affinity(mut self, affinity: affinity::AffinityState) -> Self {
        self.sealed = affinity.sealed();
        self.affinity = affinity;
        self
    }

    /// Shares overflow memory with later snapshots of this route table.
    #[must_use]
    pub fn with_overflow_memory(mut self, overflow: OverflowMemory) -> Self {
        self.overflow = overflow;
        self
    }

    /// Shares API-wide provider refusal memory with later snapshots.
    #[must_use]
    pub fn with_api_fitness(mut self, fitness: ApiFitness) -> Self {
        self.api_fitness = fitness;
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

    /// Skips hops for which the closure returns `true`.
    #[must_use]
    pub fn with_member_skip(mut self, skip: MemberSkipFn) -> Self {
        self.member_skip = Some(skip);
        self
    }

    /// Maps an all-candidates-spend-capped refusal to a retry hint.
    #[must_use]
    pub fn with_spend_cap_exhausted(mut self, exhausted: SpendCapExhaustedFn) -> Self {
        self.spend_cap_exhausted = Some(exhausted);
        self
    }

    fn without_skipped_hops(&self, asked: &str, resolved: &Resolved) -> Vec<Hop> {
        let Some(skip) = &self.member_skip else {
            return resolved.hops.clone();
        };
        resolved
            .hops
            .iter()
            .filter(|hop| {
                let provider = hop.member.scope_provider();
                let wire = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
                let key_id = hop.member.provider_key_id();
                !skip(&resolved.route, asked, provider, wire, key_id)
            })
            .cloned()
            .collect()
    }

    /// Cooldown state used to order members, when configured.
    #[must_use]
    pub const fn cooldowns(&self) -> Option<&Arc<Cooldowns>> {
        self.cooldowns.as_ref()
    }

    /// Service tier ids `/v1/models` should offer for `model`.
    ///
    /// Each hop contributes the tiers its host, protocol, and configured
    /// Ultrafast flag support. There is no per-account plan list.
    #[must_use]
    pub fn service_tier_ids(&self, model: &str) -> Vec<&'static str> {
        let mut fast = false;
        let mut ultra = false;
        if let Some(route) = self.routes.get(model) {
            for hop in &route.hops {
                let attempt = hop.member.attempt();
                let wire = hop.wire_model.as_deref().unwrap_or(model);
                let protocol = hofund_codec::service_tier::spoken_protocol(
                    attempt.protocol.as_str(),
                    attempt.wire_protocol.as_str(),
                );
                for id in hofund_codec::service_tier::offered_tier_ids(
                    &attempt.base_url,
                    protocol,
                    wire,
                    hop.service_tier == "ultrafast",
                ) {
                    match id {
                        "priority" => fast = true,
                        "ultrafast" => ultra = true,
                        _ => {}
                    }
                }
            }
        }
        if ultra {
            hofund_codec::service_tier::ULTRA_OFFER.to_vec()
        } else if fast {
            vec!["priority"]
        } else {
            Vec::new()
        }
    }

    /// Short name of the API `/v1/models` should name for `model`.
    ///
    /// A preference a member learned from a remote list wins. A route whose id
    /// or unit pool starts with `group/` uses Responses only when every member
    /// speaks Responses. Any other known member protocol is that member's own
    /// API. An unknown protocol omits the field.
    #[must_use]
    pub fn preferred_api(&self, model: &str) -> Option<String> {
        let route = self.routes.get(model)?;
        for hop in &route.hops {
            let wire = hop.wire_model.as_deref().unwrap_or(model);
            if let Some(name) = learned_name(hop.member.learned_preferred(wire))
                .or_else(|| learned_name(hop.member.learned_preferred(model)))
            {
                return Some(name.to_owned());
            }
        }
        if route_is_group(model, route) {
            return group_preferred(route).map(str::to_owned);
        }
        route
            .hops
            .first()
            .and_then(|hop| native_preferred(hop.member.protocol()))
            .map(str::to_owned)
    }

    /// A coding-agent catalog entry a member learned for `model`, as a JSON object.
    #[must_use]
    pub fn learned_catalog_entry(&self, model: &str) -> Option<String> {
        let route = self.routes.get(model)?;
        for hop in &route.hops {
            let wire = hop.wire_model.as_deref().unwrap_or(model);
            if let Some(entry) = hop
                .member
                .learned_catalog_entry(wire)
                .or_else(|| hop.member.learned_catalog_entry(model))
            {
                return Some(entry);
            }
        }
        None
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

    /// Ids a caller may name, including a dashed spelling of a dotted model id.
    #[must_use]
    pub fn served_models(&self) -> Vec<hofund_codec::ServedModel> {
        let mut out = Vec::new();
        for (route, definition) in &self.routes {
            let bare = route
                .rsplit_once('/')
                .map(|(_, model)| model.to_owned())
                .unwrap_or_else(|| route.clone());
            for hop in &definition.hops {
                let wire = hop.wire_model.clone().unwrap_or_else(|| bare.clone());
                let provider = hop.member.route_id().to_owned();
                let entry = |id: String| hofund_codec::ServedModel {
                    id,
                    route: route.clone(),
                    bare: bare.clone(),
                    wire: wire.clone(),
                    provider: provider.clone(),
                };
                out.push(entry(route.clone()));
                let qualified = format!("{provider}/{bare}");
                if qualified != *route {
                    out.push(entry(qualified));
                }
                if bare != *route {
                    out.push(entry(bare.clone()));
                }
            }
        }
        out
    }

    /// Whether `model` resolves to a hop `scope` allows.
    #[must_use]
    pub fn admits(&self, model: &str, scope: Option<&RouteScope>) -> bool {
        let Ok(resolved) = self.resolve(model) else {
            return false;
        };
        scoped_hops(model, &resolved, scope).is_ok_and(|hops| !hops.is_empty())
    }

    /// Members currently in reserve for `model`, with their at-once limits.
    #[must_use]
    pub fn route_reserves(&self, model: &str) -> Vec<(String, usize)> {
        let Ok(resolved) = self.resolve(model) else {
            return Vec::new();
        };
        self.order_for(&resolved, None)
            .reserves
            .into_iter()
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
        self.dispatch_inner(request, inbound_protocol, None, None, hook, None, None)
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
        self.dispatch_inner(
            request,
            inbound_protocol,
            None,
            conversation,
            hook,
            None,
            None,
        )
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
        self.dispatch_inner(request, inbound_protocol, scope, None, hook, None, None)
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
        self.dispatch_inner(request, inbound_protocol, None, None, hook, wait, None)
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
        self.dispatch_inner(request, inbound_protocol, scope, None, hook, wait, None)
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
        self.dispatch_inner(
            request,
            inbound_protocol,
            scope,
            conversation,
            hook,
            wait,
            None,
        )
        .await
    }

    /// Like [`Self::dispatch_scoped_conversation_with_wait`], and records the
    /// routing decision in `trace`.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_scoped_conversation_with_wait_traced(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
        conversation: Option<&affinity::Conversation>,
        hook: Option<Arc<dyn AttemptHook>>,
        wait: Option<Arc<dyn WaitHook>>,
        trace: RouteTraceHandle,
    ) -> Result<Routed, RouterError> {
        self.dispatch_inner(
            request,
            inbound_protocol,
            scope,
            conversation,
            hook,
            wait,
            Some(trace),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch_inner(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
        conversation: Option<&affinity::Conversation>,
        hook: Option<Arc<dyn AttemptHook>>,
        wait_hook: Option<Arc<dyn WaitHook>>,
        mut trace: Option<RouteTraceHandle>,
    ) -> Result<Routed, RouterError> {
        let asked = request.model.clone();
        let mut resolved = self.resolve(&asked)?;
        resolved.hops = scoped_hops(&asked, &resolved, scope)?;
        let before_skip = resolved.hops.clone();
        resolved.hops = self.without_skipped_hops(&asked, &resolved);
        if resolved.hops.is_empty()
            && !before_skip.is_empty()
            && let Some(exhausted) = &self.spend_cap_exhausted
        {
            let candidates: Vec<SpendCapCandidate> = before_skip
                .iter()
                .map(|hop| SpendCapCandidate {
                    provider: hop.member.scope_provider().to_owned(),
                    key_id: hop.member.provider_key_id().to_owned(),
                })
                .collect();
            if let Some(retry_after) = exhausted(&candidates) {
                return Err(RouterError::SpendCapReached { retry_after });
            }
        }
        self.refresh_free(&mut resolved).await;
        let input_tokens = estimate_input_tokens(request);
        let ordered = self.order_for(&resolved, conversation);
        let rest_left = resolved.pool_asked && !ordered.hand_on.is_empty();
        let hand_on = ordered.hand_on;
        resolved.hops = ordered.hops;
        if let Some(conversation) = conversation {
            resolved.hops =
                self.overflow
                    .demote_hops(&conversation.key, input_tokens, resolved.hops);
        }
        // `fit` leaves out a member whose known window cannot hold the
        // estimate. Other strategies keep their order: a known-too-small
        // member is still asked, and later ones are skipped only after an
        // overflow (`needs_room`). An ineligible rest is not a fitting hop.
        // When no eligible hop fits, the unsplit order stays, so the soonest
        // probe is still the one asked.
        let apply_window = resolved.routing == "fit";
        let (fitting, hand_on, over_window) = if apply_window {
            let cooldowns = self.cooldowns.clone();
            let bare = resolved.bare.clone();
            split_over_window(resolved.hops, hand_on, input_tokens, |hop, apply_window| {
                cooldowns.as_ref().is_some_and(|cooldowns| {
                    hand_on_candidate(
                        cooldowns,
                        hop,
                        &bare,
                        HandOnFailure::Momentary,
                        input_tokens,
                        apply_window,
                        false,
                    )
                })
            })
        } else {
            (resolved.hops, hand_on, Vec::new())
        };
        let protocol_alternates = ordered.protocol_alternates;
        resolved.hops = splice_protocol_alternates(fitting, &protocol_alternates);
        let over_window = splice_protocol_alternates(over_window, &protocol_alternates);
        resolved.spent = self.spent(&resolved);
        if let Some(trace) = trace.as_mut() {
            trace.set_routing(&resolved.routing);
            if resolved.pool_asked {
                trace.set_group(&resolved.route);
            }
            if let Some(affinity) = &ordered.affinity {
                trace.set_affinity_decision(affinity.reason);
            }
            let cooling = cooling_labels(self.cooldowns.as_ref(), &resolved);
            trace.set_candidates(candidates_from_hops(
                &resolved.hops,
                &resolved.bare,
                &resolved.spent,
                &cooling,
            ));
            trace.sync_open();
        }
        let hook = conversation.map_or(hook.clone(), |conversation| {
            Some(Arc::new(affinity::sealed::SealedHook::new(
                self.sealed.clone(),
                conversation,
                hook.clone(),
            )) as Arc<dyn AttemptHook>)
        });
        let opening = conversation.map(|conversation| conversation.key.clone());
        let result = DispatchState {
            asked,
            request: request.clone(),
            inbound_protocol: inbound_protocol.to_owned(),
            hook,
            rest_left,
            hand_on: if resolved.pool_asked {
                let cooldowns = self.cooldowns.clone();
                let bare = resolved.bare.clone();
                hand_on
                    .into_iter()
                    .filter(|hop| {
                        cooldowns.as_ref().is_some_and(|cooldowns| {
                            hand_on_candidate(
                                cooldowns,
                                hop,
                                &bare,
                                HandOnFailure::Momentary,
                                input_tokens,
                                apply_window,
                                false,
                            )
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            },
            resolved,
            policy: self.policy.clone(),
            input_tokens,
            last: None,
            refused_providers: BTreeSet::new(),
            skipped_plugins: BTreeSet::new(),
            region_attempts: BTreeMap::new(),
            region_closed: BTreeMap::new(),
            region_reask: Vec::new(),
            last_model: None,
            outgrown: Vec::new(),
            needs_room: None,
            missing_detail: self.models().join(", "),
            wait_hook: wait_hook.clone(),
            affinity: ordered.affinity,
            affinity_state: self.affinity.clone(),
            overflow_memory: self.overflow.clone(),
            api_fitness: self.api_fitness.clone(),
            cooldowns: self.cooldowns.clone(),
            trace,
            strategies: Arc::clone(&self.strategies),
            served: self.served_models(),
            drain_moves: ordered.drain_moves,
            over_window,
            saw_other: false,
            clear_placeholder_served: false,
            walk_started: wait_hook
                .as_ref()
                .map_or_else(Instant::now, |hook| hook.now()),
            group_walk_budget: self.group_walk_budget,
            tier_sent: String::new(),
        }
        .run(0)
        .await;
        if result.is_err()
            && let Some(key) = opening
        {
            self.affinity.opened(&key);
        }
        result
    }

    /// Whether hop `index` is resting on a quota refusal that named its deadline.
    fn vendor_timed_quota_at(&self, resolved: &Resolved, index: usize) -> bool {
        let Some(cooldowns) = &self.cooldowns else {
            return false;
        };
        let hop = &resolved.hops[index];
        let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
        vendor_timed_quota_rest(cooldowns, hop, model).is_some()
    }

    /// Counts input tokens for `request`.
    ///
    /// An upstream that has a count endpoint is asked, with the same failover
    /// rules as [`Self::dispatch`]. A vendor-timed quota rest is not asked.
    /// The next member counts, or the result is a character estimate when
    /// none remains. When every asked upstream has no count endpoint, the
    /// result is that estimate and [`InputCount::estimated`] is set.
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
        self.count_scoped_conversation(request, inbound_protocol, scope, None)
            .await
    }

    /// Counts input for one conversation.
    ///
    /// Under strict stays, a conversation that already has an answerer is
    /// counted only on that account. When that account is not among the
    /// candidates, the result is the local estimate. Other stay modes count
    /// as [`Self::count`] does.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::count_scoped`].
    pub async fn count_scoped_conversation(
        &self,
        request: &Request,
        inbound_protocol: &str,
        scope: Option<&RouteScope>,
        conversation: Option<&affinity::Conversation>,
    ) -> Result<InputCount, RouterError> {
        let mut resolved = self.resolve(&request.model)?;
        resolved.hops = scoped_hops(&request.model, &resolved, scope)?;
        resolved.hops = self.without_skipped_hops(&request.model, &resolved);
        self.refresh_free(&mut resolved).await;
        resolved.hops = self.order_hops(&resolved);
        if resolved.stays == affinity::Stays::Strict
            && let Some(conversation) = conversation
            && let Some(member) = self
                .affinity
                .stick(&conversation.key)
                .map(|stick| stick.member)
        {
            let stuck = resolved
                .hops
                .iter()
                .filter(|hop| hop.member.id().to_string() == member)
                .cloned()
                .collect::<Vec<_>>();
            if stuck.is_empty() {
                return Ok(InputCount {
                    tokens: estimate_input_tokens(request),
                    estimated: true,
                    upstream_id: String::new(),
                });
            }
            resolved.hops = stuck;
        }
        let policy = self.policy.clone();
        let mut last = None;
        let mut unsupported = false;
        let mut skipped_hold = false;
        for index in 0..resolved.hops.len() {
            if self.vendor_timed_quota_at(&resolved, index) {
                let later_open = (index + 1..resolved.hops.len())
                    .any(|later| !self.vendor_timed_quota_at(&resolved, later));
                if later_open {
                    continue;
                }
                skipped_hold = true;
                break;
            }
            let hop = &resolved.hops[index];
            let mut attempt = request.clone();
            apply_wire(&mut attempt, hop, &resolved.bare, &self.served_models());
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
        if unsupported || (skipped_hold && last.is_none()) {
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
        resolved.hops = self.without_skipped_hops(call.model(), &resolved);
        self.refresh_free(&mut resolved).await;
        resolved.hops = self.order_hops(&resolved);
        let mut last = None;
        for (index, hop) in resolved.hops.iter().enumerate() {
            let upstream_model = hop
                .wire_model
                .clone()
                .unwrap_or_else(|| resolved.bare.clone());
            let upstream_id = hop.member.route_id().to_owned();
            let provider_key_id = hop.member.provider_key_id().to_owned();
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
                        provider_key_id,
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

    async fn refresh_free(&self, resolved: &mut Resolved) {
        if !resolved.uses_free() {
            return;
        }
        let bare = resolved.bare.clone();
        for hop in &mut resolved.hops {
            let model = hop.wire_model.as_deref().unwrap_or(&bare);
            if let Some(free) = hop.member.costs_nothing(model).await {
                hop.free = free;
            }
        }
    }

    fn order_hops(&self, resolved: &Resolved) -> Vec<Hop> {
        let ordered = self.order_for(resolved, None);
        let hops = splice_protocol_alternates(ordered.hops, &ordered.protocol_alternates);
        hops.iter()
            .filter(|hop| {
                let provider = hop.member.route_id();
                if self.api_fitness.is_refused(provider, hop.member.protocol()) {
                    return false;
                }
                let Some(primary) = hop.member.stands_in_for() else {
                    return true;
                };
                hops.iter()
                    .find(|candidate| candidate.member.id().to_string() == primary)
                    .is_some_and(|candidate| {
                        self.api_fitness
                            .is_refused(provider, candidate.member.protocol())
                    })
            })
            .cloned()
            .collect()
    }

    fn order_for(
        &self,
        resolved: &Resolved,
        conversation: Option<&affinity::Conversation>,
    ) -> Ordered {
        let mut moves = PoolMoves::default();
        let (routing_hops, protocol_alternates) = split_protocol_alternates(resolved.hops.clone());
        let ordered = self.order_level(resolved, 0, routing_hops, &mut moves);
        let (ordered, left) = if let Some(cooldowns) = &self.cooldowns {
            order_hops(ordered, cooldowns, &resolved.bare)
        } else {
            (ordered, Vec::new())
        };
        let ids = ordered
            .iter()
            .map(|hop| hop.member.id().to_string())
            .collect::<Vec<_>>();
        let resting = ordered
            .iter()
            .map(|hop| {
                let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
                self.cooldowns.as_ref().is_some_and(|cooldowns| {
                    cooldowns
                        .cooling_for_model(hop.member.id(), model)
                        .is_some()
                })
            })
            .collect::<Vec<_>>();
        let strategy = (0..ordered.len()).collect::<Vec<_>>();
        let placement = at_once::place(&resolved.at_once, &ids, &strategy, &resting);
        let reserves = placement
            .reserves
            .iter()
            .map(|(index, limit)| (ids[*index].clone(), *limit))
            .collect::<BTreeMap<_, _>>();
        let ordered = placement
            .order
            .into_iter()
            .map(|index| ordered[index].clone())
            .collect::<Vec<_>>();
        let group_member = |hop: &Hop| {
            let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
            format!("{}/{model}:{}", hop.member.route_id(), hop.effort)
        };
        let mut places = BTreeMap::new();
        if resolved.pool_asked {
            for hop in resolved
                .hops
                .iter()
                .filter(|hop| hop.member.stands_in_for().is_none())
            {
                let next = places.len() + 1;
                places.entry(group_member(hop)).or_insert(next);
            }
        }
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
                    let allowance = allowances.iter().find(|entry| entry.member == member);
                    let mode = self
                        .credits_modes
                        .as_ref()
                        .map(|store| store.mode_for_member(&member))
                        .unwrap_or_default();
                    // Off never takes First/Last credit seats; those branches
                    // already require First/Last mode, so no Off gate here.
                    let on_credits = allowance
                        .is_some_and(|entry| credits_mode::on_credits_spending(entry, model, now));
                    let used = allowance
                        .and_then(|entry| entry.fullest(model, now))
                        .map(|window| window.used);
                    let week_low = match (allowance, used) {
                        (Some(entry), Some(used))
                            if affinity::keeps_week_low_routing(&resolved.routing) =>
                        {
                            entry.week_low(model, used, now)
                        }
                        _ => false,
                    };
                    let group_member = group_member(hop);
                    let reserve = reserves.get(&member).copied().unwrap_or(0);
                    affinity::Candidate {
                        member,
                        model: model.to_owned(),
                        effort: hop.effort.clone(),
                        resting: self.cooldowns.as_ref().is_some_and(|cooldowns| {
                            cooldowns
                                .cooling_for_model(hop.member.id(), model)
                                .is_some()
                        }),
                        reserve,
                        nearly_spent: {
                            let spent_by_use = used.is_some_and(|used| {
                                used >= affinity::spent_share_of(&resolved.routing)
                            });
                            let first_keeps = mode == credits_mode::CreditsMode::First
                                && allowance.is_some_and(|entry| {
                                    credits_mode::effective_spends_credits(entry, model, mode)
                                })
                                && (on_credits || spent_by_use);
                            hop.nearly_spent
                                || (spent_by_use && !first_keeps)
                                || (mode == credits_mode::CreditsMode::Last && on_credits)
                        },
                        low_allowance: used.is_some_and(|used| used >= 90.0),
                        week_low,
                        fallback: hop.fallback,
                        aside: false,
                        place: places.get(&group_member).copied().unwrap_or(0),
                        group_member,
                        cold_after: hop.cold_after,
                    }
                })
                .collect::<Vec<_>>();
            let decision =
                self.affinity
                    .order(conversation, resolved.stays, &resolved.routing, &candidates);
            (decision, candidates)
        });
        let (ordered, strict) = if let Some((decision, _)) = &decision {
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
        if decision.as_ref().is_some_and(|(decision, _)| decision.kept)
            && let Some(kept) = ordered.first()
        {
            let kept_id = kept.member.id().to_string();
            for round in &moves.weight {
                if let Some((_, key)) = round.choices.iter().find(|(member, _)| *member == kept_id)
                {
                    self.strategies.move_round(&round.pool, &round.picked, key);
                }
            }
        }
        let (mut hops, hand_on) = if strict {
            (ordered, Vec::new())
        } else if let Some(cooldowns) = &self.cooldowns {
            let mut candidates = ordered;
            candidates.extend(left);
            order_hops(candidates, cooldowns, &resolved.bare)
        } else {
            (ordered, Vec::new())
        };
        self.reorder_last_on_credits(&mut hops, resolved);
        Ordered {
            hops,
            protocol_alternates,
            hand_on,
            reserves,
            affinity: conversation
                .zip(decision)
                .map(|(conversation, (decision, planned))| ReplyRoute {
                    key: conversation.key.clone(),
                    turn: conversation.turn,
                    strict: decision.strict,
                    planned,
                    reason: decision.reason.as_str(),
                }),
            drain_moves: moves.drain,
        }
    }

    fn order_level(
        &self,
        resolved: &Resolved,
        depth: usize,
        hops: Vec<Hop>,
        moves: &mut PoolMoves,
    ) -> Vec<Hop> {
        let mut groups: Vec<(String, u64, Vec<Hop>)> = Vec::new();
        for hop in hops {
            let (unit_key, weight) = hop.units.get(depth).map_or_else(
                || (format!("hop:{}", hop.member.id()), hop.weight),
                |unit| (unit.key.clone(), unit.weight),
            );
            if let Some((_, _, group)) = groups.iter_mut().find(|(key, _, _)| *key == unit_key) {
                group.push(hop);
            } else {
                groups.push((unit_key, weight, vec![hop]));
            }
        }
        for (_, _, group) in &mut groups {
            let nested = group[0].units.get(depth + 1).is_some();
            if nested {
                *group = self.order_level(resolved, depth + 1, std::mem::take(group), moves);
                continue;
            }
            let (pool, routing, sink) = group[0].units.get(depth).map_or_else(
                || (resolved.pool.clone(), "smart".to_owned(), false),
                |unit| (unit.pool.clone(), unit.routing.clone(), unit.sink),
            );
            let leaves = std::mem::take(group)
                .into_iter()
                .map(|hop| {
                    let key = format!("hop:{}", hop.member.id());
                    (key, hop.weight, vec![hop])
                })
                .collect();
            *group = self.order_units(resolved, &pool, &routing, sink, leaves, moves);
        }
        let (pool, routing, sink) = if depth == 0 {
            (
                resolved.pool.clone(),
                resolved.routing.clone(),
                resolved.sink,
            )
        } else {
            let unit = &groups[0].2[0].units[depth - 1];
            (unit.pool.clone(), unit.routing.clone(), unit.sink)
        };
        self.order_units(resolved, &pool, &routing, sink, groups, moves)
    }

    fn order_units(
        &self,
        resolved: &Resolved,
        pool: &str,
        routing: &str,
        sink: bool,
        groups: Vec<(String, u64, Vec<Hop>)>,
        moves: &mut PoolMoves,
    ) -> Vec<Hop> {
        let heads = groups
            .iter()
            .map(|(_, _, group)| group[0].clone())
            .collect::<Vec<_>>();
        let mut seats = self.seats(resolved, &heads, pool, routing);
        for ((key, weight, _), seat) in groups.iter().zip(&mut seats) {
            seat.key.clone_from(key);
            seat.weight = *weight;
        }
        let mut order = if groups.len() < 2 {
            (0..groups.len()).collect()
        } else {
            self.strategies.order(routing, pool, &seats)
        };
        if groups.len() > 1 {
            credits_mode::adjust_order(&mut order, &seats);
        }
        if groups.len() > 1 && routing == "weight" {
            moves.weight.push(RoundMove {
                pool: pool.to_owned(),
                picked: groups[order[0]].0.clone(),
                choices: groups
                    .iter()
                    .flat_map(|(key, _, group)| {
                        group
                            .iter()
                            .map(|hop| (hop.member.id().to_string(), key.clone()))
                    })
                    .collect(),
            });
        }
        if groups.len() > 1 && routing == "drain" {
            moves.drain.push(RoundMove {
                pool: pool.to_owned(),
                picked: groups[order[0]].0.clone(),
                choices: groups
                    .iter()
                    .flat_map(|(key, _, group)| {
                        group
                            .iter()
                            .map(|hop| (hop.member.id().to_string(), key.clone()))
                    })
                    .collect(),
            });
        }
        let mut ordered = order
            .into_iter()
            .flat_map(|index| groups[index].2.clone())
            .collect::<Vec<_>>();
        if sink
            && !matches!(routing, "rotate" | "weight")
            && let Some(cooldowns) = &self.cooldowns
        {
            ordered.sort_by_key(|hop| {
                let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
                cooldowns
                    .sunk_at(hop.member.id(), model)
                    .map_or((0, 0), |sequence| (1, sequence))
            });
        }
        ordered
    }

    fn seats(
        &self,
        resolved: &Resolved,
        hops: &[Hop],
        pool: &str,
        routing: &str,
    ) -> Vec<strategy::Seat> {
        let needs = self.strategies.needs(routing);
        let mut seats = hops
            .iter()
            .map(|hop| {
                let mut seat = strategy::Seat::new(hop.member.id().to_string());
                seat.account = hop.member.id().account().is_some();
                seat.learns = hop.member.learns_allowance();
                seat.weight = hop.weight;
                seat.free = hop.free;
                if needs.window {
                    seat.window = hop.context_window.filter(|window| *window > 0).unwrap_or(0);
                }
                let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
                seat.resting = self.cooldowns.as_ref().is_some_and(|cooldowns| {
                    cooldowns
                        .cooling_for_model(hop.member.id(), model)
                        .is_some()
                });
                seat
            })
            .collect::<Vec<_>>();
        if hops.len() > 1
            && let Some(measurements) = &self.measurements
        {
            let models = hops
                .iter()
                .map(|hop| {
                    hop.wire_model
                        .clone()
                        .unwrap_or_else(|| resolved.bare.clone())
                })
                .collect::<Vec<_>>();
            measurements.weigh(&mut seats, &models, pool, needs, SystemTime::now());
        }
        seats
    }

    fn reorder_last_on_credits(&self, hops: &mut Vec<Hop>, resolved: &Resolved) {
        if hops.len() < 2 {
            return;
        }
        let Some(modes) = &self.credits_modes else {
            return;
        };
        let now = SystemTime::now();
        let allowances = self
            .quotas
            .as_ref()
            .map(|quotas| quotas.allowances(now))
            .unwrap_or_default();
        let mut first = Vec::new();
        let mut middle = Vec::with_capacity(hops.len());
        let mut last = Vec::new();
        for hop in hops.drain(..) {
            let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
            let member = hop.member.id().to_string();
            let mode = modes.mode_for_member(&member);
            let allowance = allowances.iter().find(|entry| entry.member == member);
            let on_credits =
                allowance.is_some_and(|entry| credits_mode::on_credits_spending(entry, model, now));
            let spends = allowance
                .is_some_and(|entry| credits_mode::effective_spends_credits(entry, model, mode));
            let used = allowance
                .and_then(|entry| entry.fullest(model, now))
                .map(|window| window.used)
                .unwrap_or(0.0);
            if mode == credits_mode::CreditsMode::First
                && spends
                && (on_credits || used >= affinity::spent_share_of(&resolved.routing))
            {
                first.push(hop);
            } else if mode == credits_mode::CreditsMode::Last && on_credits {
                last.push(hop);
            } else {
                middle.push(hop);
            }
        }
        if first.is_empty() && last.is_empty() {
            *hops = middle;
            return;
        }
        first.extend(middle);
        first.extend(last);
        *hops = first;
    }

    /// Resolves `asked` the same way a chat call does, then applies `scope`.
    ///
    /// # Errors
    ///
    /// Returns the same routing errors as a chat call for an unknown model,
    /// an ambiguous name, or a model or account outside `scope`.
    pub fn candidates(
        &self,
        asked: &str,
        scope: Option<&RouteScope>,
    ) -> Result<Candidates, RouterError> {
        let resolved = self.resolve(asked)?;
        let hops = scoped_hops(asked, &resolved, scope)?;
        Ok(Candidates {
            route: resolved.route,
            model: resolved.bare,
            hops,
        })
    }

    fn resolve(&self, asked: &str) -> Result<Resolved, RouterError> {
        if let Some(route) = self
            .routes
            .get(asked)
            .filter(|route| !route.hops.is_empty())
        {
            return Ok(Resolved {
                hops: route.hops.clone(),
                at_once: route.at_once.clone(),
                route: asked.to_owned(),
                bare: asked.to_owned(),
                pool: asked.to_owned(),
                routing: route.routing.clone(),
                sink: route.sink,
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
            if found.len() == 1
                && let Some((route_model, primary, stays, _)) = found.first()
                && let Some(route) = self.routes.get(route_model)
                && route
                    .hops
                    .iter()
                    .any(|hop| hop.units.iter().any(|unit| unit.fallback))
            {
                return Ok(Resolved {
                    hops: route.hops.clone(),
                    at_once: route.at_once.clone(),
                    route: route_model.clone(),
                    bare: model.to_owned(),
                    pool: asked.to_owned(),
                    routing: "smart".to_owned(),
                    sink: false,
                    stays: *stays,
                    patience: primary.patience,
                    pool_asked: false,
                    spent: Vec::new(),
                });
            }
            let patience = found.first().and_then(|(_, hop, ..)| hop.patience);
            let mut resolved = self.one(asked, model, found)?;
            resolved.patience = patience;
            return Ok(resolved);
        }
        let found = self.matching(|_, hop| hop.wire_model.as_deref() == Some(asked));
        let bare = found
            .first()
            .and_then(|(_, hop, ..)| hop.wire_model.clone())
            .unwrap_or_else(|| asked.to_owned());
        self.one(asked, &bare, found)
    }

    fn matching(
        &self,
        mut pred: impl FnMut(&str, &Hop) -> bool,
    ) -> Vec<(String, Hop, affinity::Stays, Option<at_once::AtOnceMember>)> {
        let mut found = Vec::new();
        for (route_model, route) in &self.routes {
            for hop in &route.hops {
                if pred(route_model, hop) {
                    found.push((
                        route_model.clone(),
                        hop.clone(),
                        route.stays,
                        route
                            .at_once
                            .get(hop.member.id().to_string().as_str())
                            .cloned(),
                    ));
                }
            }
        }
        found
    }

    fn one(
        &self,
        asked: &str,
        bare: &str,
        found: Vec<(String, Hop, affinity::Stays, Option<at_once::AtOnceMember>)>,
    ) -> Result<Resolved, RouterError> {
        match found.len() {
            0 => Err(self.missing(asked)),
            1 => {
                let (route, hop, stays, at_once) = found.into_iter().next().expect("one hop");
                let at_once = at_once
                    .map(|member| (hop.member.id().to_string(), member))
                    .into_iter()
                    .collect();
                Ok(Resolved {
                    hops: vec![hop],
                    at_once,
                    route,
                    bare: bare.to_owned(),
                    pool: asked.to_owned(),
                    routing: "smart".to_owned(),
                    sink: false,
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

fn learned_name(name: Option<String>) -> Option<&'static str> {
    name.as_deref()
        .and_then(hofund_plugin_api::preferred_api_name)
}

fn native_preferred(protocol: &str) -> Option<&'static str> {
    hofund_plugin_api::preferred_api_name(protocol)
}

fn route_is_group(model: &str, route: &Route) -> bool {
    model.starts_with("group/")
        || route
            .hops
            .iter()
            .any(|hop| hop.units.iter().any(|unit| unit.pool.starts_with("group/")))
}

/// Responses when every member speaks it. A Chat-only or Messages-only member
/// leaves the field off, so the route stays on Chat.
fn group_preferred(route: &Route) -> Option<&'static str> {
    if route.hops.is_empty() {
        return None;
    }
    let mut responses = false;
    for hop in &route.hops {
        match native_preferred(hop.member.protocol()) {
            Some("responses") => responses = true,
            Some(_) => return None,
            None if hop.member.protocol().is_empty() => {}
            None => return None,
        }
    }
    responses.then_some("responses")
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
    rest_left: bool,
    hand_on: Vec<Hop>,
    resolved: Resolved,
    policy: FailoverPolicy,
    input_tokens: u64,
    last: Option<(String, PluginError)>,
    refused_providers: BTreeSet<String>,
    /// Plugin ids that must not be asked again on this request.
    skipped_plugins: BTreeSet<String>,
    region_attempts: BTreeMap<String, u32>,
    region_closed: BTreeMap<String, u16>,
    region_reask: Vec<usize>,
    last_model: Option<String>,
    outgrown: Vec<(String, String)>,
    needs_room: Option<u64>,
    /// `fit` members whose known window cannot hold the estimate. Asked only
    /// as a last resort, after every member that may hold it is out of quota
    /// or credit.
    over_window: Vec<Hop>,
    /// A fitting member already failed for a reason other than quota or credit.
    saw_other: bool,
    missing_detail: String,
    wait_hook: Option<Arc<dyn WaitHook>>,
    /// Where an answer's stick is recorded, when the request has a conversation.
    affinity: Option<ReplyRoute>,
    affinity_state: affinity::AffinityState,
    overflow_memory: OverflowMemory,
    api_fitness: ApiFitness,
    cooldowns: Option<Arc<Cooldowns>>,
    trace: Option<RouteTraceHandle>,
    strategies: Arc<strategy::StrategyState>,
    served: Vec<hofund_codec::ServedModel>,
    drain_moves: Vec<RoundMove>,
    walk_started: Instant,
    group_walk_budget: Duration,
    tier_sent: String,
    /// Drop placeholder `auto` and `default` served words for this hop's host.
    clear_placeholder_served: bool,
}

/// One member's answer within a dispatch round.
struct Answered {
    index: usize,
    upstream_id: String,
    provider_key_id: String,
    upstream_model: String,
    completion: Completion,
    tier_sent: String,
    clear_placeholder_served: bool,
}

/// Which guessed rest a left-out member is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandOnSeat {
    /// Guessed rate rest. Accepts any own failure, including 401-404 and quota 429.
    Rate,
    /// Outage, slow-start, or overload. Momentary failures only.
    Momentary,
    /// The rest has ended. A momentary failure may still ask when the guard is clear.
    Expired,
    /// Quota, credit, key, or a vendor-timed retry. Never handed on.
    Ineligible,
}

/// The failure a hand-on decision is being made for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandOnFailure {
    /// Connect, timeout, transport, interrupted, or a busy status.
    Momentary,
    /// Any other own failure failover may hand on, including 401-404 and quota 429.
    Own,
    /// The allowance last resort. Any guessed or expired rest may answer, subject to the guard.
    LastResort,
}

/// Whether one left-out member would be asked for `failure`.
///
/// The hand-on list, the fit split, the another-candidate flag, the
/// fitting-follower check, and [`DispatchState::take_hand_on`] all use this
/// predicate. A live 30-second ask guard or a known-too-small window blocks
/// every seat. The check does not record an ask.
fn hand_on_eligible(
    seat: HandOnSeat,
    failure: HandOnFailure,
    window_blocks: bool,
    guarded: bool,
) -> bool {
    if guarded || window_blocks {
        return false;
    }
    match seat {
        HandOnSeat::Ineligible => false,
        HandOnSeat::Rate => true,
        HandOnSeat::Momentary | HandOnSeat::Expired => matches!(
            failure,
            HandOnFailure::Momentary | HandOnFailure::LastResort
        ),
    }
}

fn hand_on_seat(cooldowns: &Cooldowns, hop: &Hop, bare: &str) -> HandOnSeat {
    let model = hop.wire_model.as_deref().unwrap_or(bare);
    match cooldowns.cooling_for_model(hop.member.id(), model) {
        Some(cooling) => match cooling.hand_on() {
            Some(HandOnRest::Rate) => HandOnSeat::Rate,
            Some(HandOnRest::Momentary) => HandOnSeat::Momentary,
            None => HandOnSeat::Ineligible,
        },
        None => HandOnSeat::Expired,
    }
}

/// Resolves `hop` at the cooldown book's clock and applies [`hand_on_eligible`].
fn hand_on_candidate(
    cooldowns: &Cooldowns,
    hop: &Hop,
    bare: &str,
    failure: HandOnFailure,
    tokens: u64,
    apply_window: bool,
    honor_guard: bool,
) -> bool {
    let window_blocks = apply_window
        && hop
            .context_window
            .is_some_and(|window| window > 0 && tokens >= window);
    hand_on_eligible(
        hand_on_seat(cooldowns, hop, bare),
        failure,
        window_blocks,
        honor_guard && cooldowns.hand_on_asked_recently(hop.member.id()),
    )
}

fn hand_on_failure(error: &PluginError, allows: bool, now: Instant) -> Option<HandOnFailure> {
    if error.status() == Some(400)
        || decision::outgrown(error)
        || decision::context_overflow_memory(error)
        || decision::content_refused(error)
        || !allows
    {
        return None;
    }
    let momentary = matches!(
        error.kind(),
        PluginErrorKind::Connect
            | PluginErrorKind::Timeout
            | PluginErrorKind::Transport
            | PluginErrorKind::Interrupted
    ) || patience::busy(error, 0, PoolMemberPatience::Off, now).is_some();
    Some(if momentary {
        HandOnFailure::Momentary
    } else {
        HandOnFailure::Own
    })
}

/// Splits ordinary and left-out hops into members that may hold `tokens` and
/// members whose known window is no larger than that estimate. An unknown
/// window stays with the members that may hold the request. A left-out hop
/// counts as fitting only when `eligible` says this failure could ask it, so
/// a guessed quota, credit, key, or vendor-timed rest is not promoted into
/// the ask list. When no eligible hop fits, or none is too small, both orders
/// are returned unchanged.
fn split_over_window(
    hops: Vec<Hop>,
    left: Vec<Hop>,
    tokens: u64,
    eligible: impl Fn(&Hop, bool) -> bool,
) -> (Vec<Hop>, Vec<Hop>, Vec<Hop>) {
    let total = hops.len() + left.len();
    if total < 2 {
        return (hops, left, Vec::new());
    }
    let too_small = |hop: &Hop| {
        hop.context_window
            .is_some_and(|window| window > 0 && tokens >= window)
    };
    let fitting_count = hops.iter().filter(|hop| !too_small(hop)).count()
        + left.iter().filter(|hop| eligible(hop, true)).count();
    let any_too_small = hops.iter().chain(left.iter()).any(too_small);
    if fitting_count == 0 || !any_too_small {
        return (hops, left, Vec::new());
    }
    let mut fitting = Vec::new();
    let mut over_window = Vec::new();
    for hop in hops {
        if too_small(&hop) {
            over_window.push(hop);
        } else {
            fitting.push(hop);
        }
    }
    let mut left_fitting = Vec::new();
    for hop in left {
        if too_small(&hop) {
            if eligible(&hop, false) {
                over_window.push(hop);
            }
        } else if eligible(&hop, true) {
            left_fitting.push(hop);
        }
    }
    if fitting.is_empty() && !left_fitting.is_empty() {
        fitting.push(left_fitting.remove(0));
    }
    (fitting, left_fitting, over_window)
}

/// Separates ordinary route members from APIs that stand in after a refusal.
fn split_protocol_alternates(hops: Vec<Hop>) -> (Vec<Hop>, Vec<Hop>) {
    let mut routing = Vec::with_capacity(hops.len());
    let mut alternates = Vec::new();
    for hop in hops {
        if hop.member.stands_in_for().is_some() {
            alternates.push(hop);
        } else {
            routing.push(hop);
        }
    }
    (routing, alternates)
}

/// Places each alternate immediately after the member it stands in for.
fn splice_protocol_alternates(hops: Vec<Hop>, alternates: &[Hop]) -> Vec<Hop> {
    if alternates.is_empty() {
        return hops;
    }
    let mut out = Vec::with_capacity(hops.len() + alternates.len());
    for hop in hops {
        let id = hop.member.id().to_string();
        out.push(hop);
        for alternate in alternates {
            if alternate.member.stands_in_for() == Some(id.as_str()) {
                out.push(alternate.clone());
            }
        }
    }
    out
}

/// Quota or credit, and not a refusal, a passing retry, or an overflow.
fn allowance_failure(error: &PluginError) -> bool {
    if decision::outgrown(error)
        || decision::content_refused(error)
        || patience::busy(error, 0, PoolMemberPatience::Off, Instant::now()).is_some()
    {
        return false;
    }
    patience::spent(error)
}

fn region_key(provider: &str, model: &str) -> String {
    format!("{provider}\0{model}")
}

/// What to do with a vendor-timed quota rest before calling the member.
enum QuotaHold {
    /// This hop is not that kind of rest.
    Proceed,
    /// This hop rests that way, and a later hop can still be asked.
    Skip,
    /// Every hop still to be asked rests that way. Do not call.
    Answer(PluginError),
}

impl DispatchState {
    /// Whether failover may ask a later member on this request.
    ///
    /// A left-out member counts only when [`hand_on_eligible`] would accept an
    /// own failure that is not momentary. The probe's failure is not known
    /// yet, and this check does not record the 30-second guard. A momentary
    /// rest, or a rate rest still inside that guard, must not make the probe
    /// skip its queue wait.
    fn another_candidate_follows(&self, index: usize) -> bool {
        if self.hand_on_follows(HandOnFailure::Own) {
            return true;
        }
        for hop in self.resolved.hops.iter().skip(index + 1) {
            let upstream_model = hop.wire_model.as_deref().unwrap_or(&self.resolved.bare);
            let plugin_id = hop.member.plugin_id();
            if self.refused_providers.contains(hop.member.route_id())
                || self.api_refused_for(hop)
                || self.protocol_alternate_idle(hop)
            {
                continue;
            }
            if !plugin_id.is_empty() && self.skipped_plugins.contains(plugin_id) {
                continue;
            }
            if self.region_closed_for(hop) {
                continue;
            }
            if self
                .outgrown
                .iter()
                .any(|(route, model)| route == hop.member.route_id() && *model == upstream_model)
            {
                continue;
            }
            if self.needs_room.is_some_and(|tokens| {
                hop.context_window
                    .is_some_and(|window| window > 0 && tokens >= window)
            }) {
                continue;
            }
            return true;
        }
        false
    }

    fn api_refused_for(&self, hop: &Hop) -> bool {
        self.api_fitness
            .is_refused(hop.member.route_id(), hop.member.protocol())
    }

    /// Whether `hop` is another API of a member that has not refused its own.
    fn protocol_alternate_idle(&self, hop: &Hop) -> bool {
        let Some(primary) = hop.member.stands_in_for() else {
            return false;
        };
        let Some(protocol) = self.resolved.hops.iter().find_map(|candidate| {
            (candidate.member.id().to_string() == primary)
                .then(|| candidate.member.protocol().to_owned())
        }) else {
            return true;
        };
        !self
            .api_fitness
            .is_refused(hop.member.route_id(), &protocol)
    }

    fn region_closed_for(&self, hop: &Hop) -> bool {
        let model = hop.wire_model.as_deref().unwrap_or(&self.resolved.bare);
        let key = region_key(hop.member.scope_provider(), model);
        self.region_closed.contains_key(&key)
    }

    fn region_closed_status(&self, hop: &Hop) -> Option<u16> {
        let model = hop.wire_model.as_deref().unwrap_or(&self.resolved.bare);
        let key = region_key(hop.member.scope_provider(), model);
        self.region_closed.get(&key).copied()
    }

    fn now(&self) -> Instant {
        self.wait_hook
            .as_ref()
            .map_or_else(Instant::now, |hook| hook.now())
    }

    fn group_budget_exhausted(&self) -> Option<PluginError> {
        let routing_hops = self
            .resolved
            .hops
            .iter()
            .filter(|hop| hop.member.stands_in_for().is_none())
            .count();
        if routing_hops <= 1 {
            return None;
        }
        let elapsed = self.now().duration_since(self.walk_started);
        if elapsed < self.group_walk_budget {
            return None;
        }
        Some(
            PluginError::status_error(
                504,
                format!(
                    "no member of the group answered within its {}s budget",
                    self.group_walk_budget.as_secs()
                ),
            )
            .without_cooldown(),
        )
    }

    fn take_hand_on(&mut self) -> Option<usize> {
        let error = &self.last.as_ref()?.1;
        let allows = self.policy.allows(error) || decision::asks_next(error);
        let failure = hand_on_failure(error, allows, self.now())?;
        let cooldowns = self.cooldowns.clone()?;
        let bare = self.resolved.bare.clone();
        let tokens = self.input_tokens;
        let apply_window = self.resolved.routing == "fit";
        let position = self.hand_on.iter().position(|hop| {
            !self.member_skipped(hop)
                && hand_on_candidate(&cooldowns, hop, &bare, failure, tokens, apply_window, true)
                && cooldowns
                    .claim_hand_on(hop.member.id(), hop.wire_model.as_deref().unwrap_or(&bare))
        })?;
        let hop = self.hand_on.remove(position);
        let index = self.resolved.hops.len();
        self.resolved.spent.push(false);
        self.resolved.hops.push(hop);
        Some(index)
    }

    async fn run(mut self, start: usize) -> Result<Routed, RouterError> {
        let mut round = 0_u32;
        let mut wait_from = None;
        let mut asking: Vec<usize> = (start..self.resolved.hops.len()).collect();
        let mut busy = Vec::new();
        loop {
            let mut ending = Ending::Answered;
            match self.round(&asking, &mut busy, &mut ending).await {
                Ok(Some(answered)) => {
                    let next = answered.index + 1;
                    let has_continuation = self
                        .resolved
                        .hops
                        .iter()
                        .skip(next)
                        .any(|hop| !self.protocol_alternate_idle(hop));
                    let affinity_reason = self
                        .affinity
                        .as_ref()
                        .map(|affinity| affinity.reason.to_owned());
                    if let Some(trace) = self.trace.as_mut()
                        && self.asked != answered.upstream_model
                    {
                        let why = affinity_reason
                            .as_deref()
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or("stand-in");
                        trace.set_moved(&answered.upstream_model, why);
                    }
                    let protocol = self.resolved.hops[answered.index]
                        .member
                        .protocol()
                        .to_owned();
                    if has_continuation {
                        if let Some(trace) = self.trace.as_mut() {
                            trace.sync_open();
                        }
                        let continuation = RouteContinuation { state: self, next };
                        return Ok(Routed {
                            upstream_id: answered.upstream_id,
                            provider_key_id: answered.provider_key_id,
                            upstream_model: answered.upstream_model,
                            protocol,
                            completion: answered.completion,
                            affinity_reason,
                            service_tier_sent: answered.tier_sent,
                            clear_placeholder_served: answered.clear_placeholder_served,
                            continuation: Some(continuation),
                        });
                    }
                    if let Some(trace) = self.trace.take() {
                        let hop = &self.resolved.hops[answered.index];
                        let member = hop.member.id().to_string();
                        trace.finish(
                            Some(&member),
                            Some(hop.member.route_id()),
                            Some(200),
                            None,
                            affinity_reason.as_deref(),
                        );
                    }
                    return Ok(Routed {
                        upstream_id: answered.upstream_id,
                        provider_key_id: answered.provider_key_id,
                        upstream_model: answered.upstream_model,
                        protocol,
                        service_tier_sent: answered.tier_sent,
                        clear_placeholder_served: answered.clear_placeholder_served,
                        completion: answered.completion,
                        affinity_reason,
                        continuation: None,
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    let message = error.to_string();
                    if let Some(trace) = self.trace.take() {
                        trace.finish(None, None, None, Some(&message), None);
                    }
                    return Err(error);
                }
            }
            if ending != Ending::Answered
                && let Some(next) = self.take_hand_on()
            {
                asking = vec![next];
                continue;
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
                && patience::any_member_may_replan(&busy, self.resolved.patience)
                && let Some(hook) = self.wait_hook.clone()
            {
                let from = *wait_from.get_or_insert(now);
                if let Some((next, wait)) =
                    patience::replan(&mut busy, round, from, self.resolved.patience, now)
                {
                    hook.waiting();
                    hook.wait(wait).await;
                    round = round.saturating_add(1);
                    asking = next;
                    busy.clear();
                    continue;
                }
            }
            if !self.region_reask.is_empty() {
                if let Some(hook) = self.wait_hook.as_ref() {
                    hook.waiting();
                    hook.wait(patience::REGION_RETRY_PAUSE).await;
                }
                asking = std::mem::take(&mut self.region_reask);
                busy.clear();
                continue;
            }
            if let Some(next) = self.take_over_window() {
                asking = next;
                busy.clear();
                continue;
            }
            break;
        }
        let error_message = self
            .last
            .as_ref()
            .map(|(_, error)| error.message().to_owned());
        if let Some(trace) = self.trace.take() {
            trace.finish(
                None,
                None,
                self.last.as_ref().and_then(|(_, error)| error.status()),
                error_message.as_deref(),
                None,
            );
        }
        match self.last {
            Some((upstream_id, error)) => {
                let model = self.last_model.as_deref().unwrap_or(&self.request.model);
                let error = if decision::region_refused(&error) {
                    decision::with_region_agent_note(error, model)
                } else {
                    error
                };
                Err(RouterError::Upstream { upstream_id, error })
            }
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
            if self.protocol_alternate_idle(&hop) {
                continue;
            }
            let mut attempt = self.request.clone();
            apply_wire(&mut attempt, &hop, &self.resolved.bare, &self.served);
            self.tier_sent = hofund_codec::service_tier::extension_tier(&attempt.extensions);
            self.clear_placeholder_served = hofund_codec::service_tier::clear_placeholder_served(
                &hop.member.attempt().base_url,
            );
            let upstream_model = attempt.model.clone();
            let plugin_id = hop.member.plugin_id();
            if self.refused_providers.contains(hop.member.route_id())
                || self.api_refused_for(&hop)
                || (!plugin_id.is_empty() && self.skipped_plugins.contains(plugin_id))
                || self.region_closed_for(&hop)
                || self.outgrown.iter().any(|(route, model)| {
                    route == hop.member.route_id() && *model == upstream_model
                })
                || self.needs_room.is_some_and(|tokens| {
                    hop.context_window
                        .is_some_and(|window| window > 0 && tokens >= window)
                })
            {
                if self.refused_providers.contains(hop.member.route_id()) {
                    self.note_skip(index as u32, SkipReason::ProviderRefused);
                } else if self.api_refused_for(&hop) {
                    self.note_skip(
                        index as u32,
                        SkipReason::Failure {
                            status: self
                                .api_fitness
                                .refusal_status(hop.member.route_id(), hop.member.protocol()),
                            message: "the provider refuses this API for every model".to_owned(),
                        },
                    );
                } else if !plugin_id.is_empty() && self.skipped_plugins.contains(plugin_id) {
                    self.note_skip(
                        index as u32,
                        SkipReason::Failure {
                            status: None,
                            message: "the same plugin already declined this request".to_owned(),
                        },
                    );
                } else if self.region_closed_for(&hop) {
                    self.note_skip(
                        index as u32,
                        SkipReason::Failure {
                            status: self.region_closed_status(&hop),
                            message: "regional model refusal".to_owned(),
                        },
                    );
                } else if self.needs_room.is_some_and(|tokens| {
                    hop.context_window
                        .is_some_and(|window| window > 0 && tokens >= window)
                }) {
                    self.note_skip(
                        index as u32,
                        SkipReason::Context {
                            window: hop.context_window,
                        },
                    );
                } else {
                    self.note_skip(
                        index as u32,
                        SkipReason::Failure {
                            status: None,
                            message: "prompt too long for this member".to_owned(),
                        },
                    );
                }
                continue;
            }
            if self.rest_left
                && let Some(cooldowns) = &self.cooldowns
            {
                cooldowns.note_asked_while_cooling(hop.member.id(), &upstream_model);
            }
            match self.quota_hold(asking, index) {
                QuotaHold::Proceed => {}
                QuotaHold::Skip => {
                    self.note_skip(index as u32, SkipReason::Quota);
                    continue;
                }
                QuotaHold::Answer(error) => {
                    let upstream_id = hop.member.route_id().to_owned();
                    self.note_try(
                        index,
                        &hop,
                        &upstream_model,
                        true,
                        try_status(&error),
                        fail_text(&error),
                    );
                    self.last = Some((upstream_id, error));
                    self.last_model = Some(upstream_model);
                    *ending = Ending::Other;
                    return Ok(None);
                }
            }
            if self.another_candidate_follows(index) {
                attempt.extensions.insert(
                    hofund_plugin_api::ANOTHER_ROUTING_CANDIDATE,
                    serde_json::json!(true),
                );
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
            let mut empty_retried = false;
            let result = loop {
                let (chat, result) = call_member(
                    &hop,
                    index,
                    &self.inbound_protocol,
                    attempt.clone(),
                    self.hook.clone(),
                )
                .await;
                if chat
                    && !empty_retried
                    && let Err(error) = &result
                    && error.message().contains(EMPTY_REPLY_MESSAGE)
                {
                    self.note_try(
                        index,
                        &hop,
                        &upstream_model,
                        true,
                        try_status(error),
                        fail_text(error),
                    );
                    empty_retried = true;
                    continue;
                }
                break result;
            };
            match result {
                Ok(completion) => {
                    if let Some(hook) = self.hook.as_ref() {
                        hook.answered(&info);
                    }
                    if let Some(affinity) = &self.affinity {
                        let member = hop.member.id().to_string();
                        self.overflow_memory.clear(&affinity.key, &member);
                    }
                    let member_id = hop.member.id().to_string();
                    for pool_move in &self.drain_moves {
                        if let Some((_, key)) = pool_move
                            .choices
                            .iter()
                            .find(|(member, _)| *member == member_id)
                        {
                            self.strategies.record_drain(&pool_move.pool, key);
                        }
                    }
                    self.note_try(index, &hop, &upstream_model, true, Some(200), String::new());
                    let completion = if self.asked == upstream_model {
                        completion
                    } else {
                        label_model(completion, self.asked.clone())
                    };
                    let completion = if let Some(affinity) = &self.affinity {
                        let member = hop.member.id().to_string();
                        let answerer = affinity.planned.iter().find(|candidate| {
                            candidate.member == member
                                && candidate.model == upstream_model
                                && candidate.effort == hop.effort
                        });
                        let stood_in = answerer.is_some_and(|answerer| {
                            affinity::AffinityState::stands_in_for(
                                answerer,
                                &affinity.planned,
                                |candidate| {
                                    self.cooldowns.as_ref().is_some_and(|cooldowns| {
                                        self.resolved.hops.iter().any(|hop| {
                                            hop.member.id().to_string() == candidate.member
                                                && hop
                                                    .wire_model
                                                    .as_deref()
                                                    .unwrap_or(&self.resolved.bare)
                                                    == candidate.model
                                                && cooldowns
                                                    .cooling_for_model(
                                                        hop.member.id(),
                                                        &candidate.model,
                                                    )
                                                    .is_some()
                                        })
                                    })
                                },
                            )
                        });
                        affinity_reply::watch(
                            completion,
                            affinity_reply::Reply {
                                state: self.affinity_state.clone(),
                                key: affinity.key.clone(),
                                turn: affinity.turn,
                                strict: affinity.strict,
                                member,
                                model: upstream_model.clone(),
                                effort: hop.effort.clone(),
                                stood_in,
                            },
                        )
                    } else {
                        completion
                    };
                    return Ok(Some(Answered {
                        index,
                        upstream_id: hop.member.route_id().to_owned(),
                        provider_key_id: hop.member.provider_key_id().to_owned(),
                        upstream_model,
                        completion,
                        tier_sent: self.tier_sent.clone(),
                        clear_placeholder_served: self.clear_placeholder_served,
                    }));
                }
                Err(error) => {
                    let upstream_id = hop.member.route_id().to_owned();
                    let record_overflow = decision::context_overflow_memory(&error);
                    let inferred = decision::too_long_unsaid(
                        &error,
                        self.input_tokens,
                        hop.context_window,
                        &upstream_model,
                        &upstream_id,
                    );
                    let error = inferred.unwrap_or(error);
                    if let Some(status) = error
                        .status()
                        .filter(|status| hofund_ir::api_refused(*status, error.message()))
                    {
                        self.api_fitness
                            .refuse(&upstream_id, hop.member.protocol(), status);
                    }
                    if decision::region_refused(&error) {
                        hop.member.close_idle_connections();
                        let key = region_key(hop.member.scope_provider(), &upstream_model);
                        let status = error.status().unwrap_or(400);
                        let count = self.region_attempts.entry(key.clone()).or_insert(0);
                        *count += 1;
                        if *count > 1 {
                            self.region_closed.insert(key, status);
                        } else {
                            let has_mate = self.resolved.hops.iter().enumerate().any(|(j, h)| {
                                j > index
                                    && h.member.scope_provider() == hop.member.scope_provider()
                                    && h.wire_model.as_deref().unwrap_or(&self.resolved.bare)
                                        == upstream_model
                            });
                            if !has_mate {
                                self.region_reask.push(index);
                            }
                        }
                    }
                    if decision::outgrown(&error) {
                        // As long on every account of this model; another
                        // member not known to be too small may hold it.
                        // An overflow is not the allowance failure that asks
                        // a member left out for its window.
                        if error.status() != Some(413) {
                            self.needs_room = Some(self.input_tokens);
                        }
                        if record_overflow && let Some(affinity) = &self.affinity {
                            self.overflow_memory.record(
                                &affinity.key,
                                &hop.member.id().to_string(),
                                self.input_tokens,
                            );
                        }
                        self.outgrown.push((upstream_id.clone(), upstream_model));
                        *ending = Ending::Other;
                        self.note_other_if_another_follows(asking, index, &error);
                        self.last = Some((upstream_id, error));
                        continue;
                    }
                    if error.skips_failover_same_plugin() {
                        let plugin = hop.member.plugin_id();
                        if !plugin.is_empty() {
                            self.skipped_plugins.insert(plugin.to_owned());
                        }
                    }
                    if decision::skips_provider(&error) {
                        // Every account of the provider refuses the same
                        // content; another provider is asked once.
                        self.refused_providers.insert(upstream_id.clone());
                    } else if !decision::asks_next(&error) && !self.policy.allows(&error) {
                        self.note_try(
                            index,
                            &hop,
                            &upstream_model,
                            true,
                            try_status(&error),
                            fail_text(&error),
                        );
                        return Err(RouterError::Upstream { upstream_id, error });
                    }
                    self.note_try(
                        index,
                        &hop,
                        &upstream_model,
                        true,
                        try_status(&error),
                        fail_text(&error),
                    );
                    if (self.policy.allows(&error) || decision::asks_next(&error))
                        && let Some(billed) = error.billed().filter(|b| b.token_bearing())
                        && let Some(hook) = self.hook.as_ref()
                    {
                        hook.failed_handoff(
                            &info,
                            try_status(&error).unwrap_or(502),
                            billed.clone(),
                        );
                    }
                    if self.policy.allows(&error) {
                        self.note_skip(
                            index as u32,
                            SkipReason::Failure {
                                status: try_status(&error),
                                message: fail_text(&error),
                            },
                        );
                    }
                    if let Some(budget) = self.group_budget_exhausted() {
                        return Err(RouterError::Upstream {
                            upstream_id,
                            error: budget,
                        });
                    }
                    let member_patience = if self.resolved.pool_asked {
                        hop.pool_member_patience()
                    } else {
                        PoolMemberPatience::Inherited
                    };
                    if !allowance_failure(&error) {
                        // A passing failure of the member that is currently
                        // last is replanned. It is not an earlier failure;
                        // the try after that replan is the one that counts.
                        self.note_other_if_another_follows(asking, index, &error);
                    }
                    if let Some(failure) =
                        patience::busy(&error, index, member_patience, self.now())
                    {
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
                    self.last_model = Some(upstream_model);
                }
            }
        }
        Ok(None)
    }

    /// A vendor-timed quota rest is not a probe. A later member that can still
    /// be asked is tried instead. When every remaining member rests that way,
    /// the first stored words and the soonest reset are the answer.
    fn quota_hold(&self, asking: &[usize], index: usize) -> QuotaHold {
        let Some(cooldowns) = self.cooldowns.as_ref() else {
            return QuotaHold::Proceed;
        };
        let now = self.now();
        let mut first_message = None;
        let mut soonest: Option<(Duration, u16)> = None;
        let mut current_held = false;
        let mut other = false;
        let mut any = false;
        for candidate in asking
            .iter()
            .copied()
            .filter(|candidate| *candidate >= index)
        {
            let model = {
                let hop = &self.resolved.hops[candidate];
                hop.wire_model
                    .clone()
                    .unwrap_or_else(|| self.resolved.bare.clone())
            };
            if self.index_left_out(candidate, &model) {
                continue;
            }
            any = true;
            let held = vendor_timed_quota_rest(cooldowns, &self.resolved.hops[candidate], &model);
            let Some(cooling) = held else {
                other = true;
                continue;
            };
            if candidate == index {
                current_held = true;
            }
            if first_message.is_none() {
                let text = cooling.error();
                first_message = Some(if text.is_empty() {
                    "out of quota".to_owned()
                } else {
                    text.to_owned()
                });
            }
            let left = cooling.until().saturating_duration_since(now);
            let status = cooling.status().unwrap_or(429);
            if soonest.as_ref().is_none_or(|(best, _)| left < *best) {
                soonest = Some((left, status));
            }
        }
        if !any || (!current_held && other) {
            return QuotaHold::Proceed;
        }
        if other {
            return QuotaHold::Skip;
        }
        let Some((left, status)) = soonest else {
            return QuotaHold::Proceed;
        };
        let message = first_message.unwrap_or_else(|| "out of quota".to_owned());
        let mut error = PluginError::quota_status(status, message).with_model_scope();
        if !left.is_zero() {
            error = error.with_reset_after(left).with_retry_after(left);
        }
        QuotaHold::Answer(error)
    }

    /// Whether this hop would be passed over before it is called.
    fn index_left_out(&self, index: usize, model: &str) -> bool {
        let (route, plugin, window, scope, wire) = {
            let hop = &self.resolved.hops[index];
            (
                hop.member.route_id().to_owned(),
                hop.member.plugin_id().to_owned(),
                hop.context_window,
                hop.member.scope_provider().to_owned(),
                hop.wire_model.clone(),
            )
        };
        let region_model = wire.as_deref().unwrap_or(&self.resolved.bare);
        self.refused_providers.contains(&route)
            || self.api_refused_for(&self.resolved.hops[index])
            || self.protocol_alternate_idle(&self.resolved.hops[index])
            || (!plugin.is_empty() && self.skipped_plugins.contains(&plugin))
            || self
                .region_closed
                .contains_key(&region_key(&scope, region_model))
            || self
                .outgrown
                .iter()
                .any(|(known_route, known_model)| known_route == &route && known_model == model)
            || self
                .needs_room
                .is_some_and(|tokens| window.is_some_and(|window| window > 0 && tokens >= window))
    }

    fn note_try(
        &mut self,
        _index: usize,
        hop: &Hop,
        upstream_model: &str,
        done: bool,
        status: Option<u16>,
        fail: String,
    ) {
        let Some(trace) = self.trace.as_mut() else {
            return;
        };
        trace.push_try(
            RouteTry::new(
                hop.member.id().to_string(),
                hop.member.route_id(),
                upstream_model,
                done,
                status,
                fail,
            )
            .with_tier(hofund_control_proto::sent_tier_label(&self.tier_sent)),
        );
        trace.sync_open();
    }

    /// Puts the left-out members on the ask list when the final try failed
    /// for quota or credit, and no earlier fitting member failed for another
    /// reason. A refusal, a passing retry, or an overflow does not qualify.
    /// A member already resting for quota or credit stays out.
    fn take_over_window(&mut self) -> Option<Vec<usize>> {
        if self.over_window.is_empty() || self.saw_other {
            return None;
        }
        let error = &self.last.as_ref()?.1;
        if !allowance_failure(error) {
            return None;
        }
        let extra: Vec<Hop> = std::mem::take(&mut self.over_window)
            .into_iter()
            .filter(|hop| {
                !self.member_skipped(hop)
                    && self.cooldowns.as_ref().is_none_or(|cooldowns| {
                        hand_on_candidate(
                            cooldowns,
                            hop,
                            &self.resolved.bare,
                            HandOnFailure::LastResort,
                            self.input_tokens,
                            false,
                            true,
                        )
                    })
            })
            .collect();
        if extra.is_empty() {
            return None;
        }
        let start = self.resolved.hops.len();
        self.resolved.hops.extend(extra);
        Some((start..self.resolved.hops.len()).collect())
    }

    /// A non-allowance failure counts as earlier only when another fitting
    /// member is still to be asked.
    fn note_other_if_another_follows(
        &mut self,
        asking: &[usize],
        index: usize,
        error: &PluginError,
    ) {
        if self.another_fitting_follows(asking, index, error) {
            self.saw_other = true;
        }
    }

    fn another_fitting_follows(&self, asking: &[usize], index: usize, error: &PluginError) -> bool {
        let Some(pos) = asking.iter().position(|item| *item == index) else {
            return false;
        };
        if asking[pos + 1..].iter().any(|later| {
            self.resolved
                .hops
                .get(*later)
                .is_some_and(|hop| !self.member_skipped(hop))
        }) {
            return true;
        }
        let allows = self.policy.allows(error) || decision::asks_next(error);
        let Some(failure) = hand_on_failure(error, allows, self.now()) else {
            return false;
        };
        let Some(cooldowns) = &self.cooldowns else {
            return false;
        };
        self.hand_on.iter().any(|hop| {
            !self.member_skipped(hop)
                && hand_on_candidate(
                    cooldowns,
                    hop,
                    &self.resolved.bare,
                    failure,
                    self.input_tokens,
                    self.resolved.routing == "fit",
                    true,
                )
        })
    }

    /// Whether a retained hand-on hop would be asked for `failure` now.
    ///
    /// Does not record the guard.
    fn hand_on_follows(&self, failure: HandOnFailure) -> bool {
        let Some(cooldowns) = &self.cooldowns else {
            return false;
        };
        self.hand_on.iter().any(|hop| {
            !self.member_skipped(hop)
                && hand_on_candidate(
                    cooldowns,
                    hop,
                    &self.resolved.bare,
                    failure,
                    self.input_tokens,
                    self.resolved.routing == "fit",
                    true,
                )
        })
    }

    fn member_skipped(&self, hop: &Hop) -> bool {
        let upstream_model = hop.wire_model.as_deref().unwrap_or(&self.resolved.bare);
        let plugin_id = hop.member.plugin_id();
        self.refused_providers.contains(hop.member.route_id())
            || self.api_refused_for(hop)
            || self.protocol_alternate_idle(hop)
            || (!plugin_id.is_empty() && self.skipped_plugins.contains(plugin_id))
            || self.region_closed_for(hop)
            || self
                .outgrown
                .iter()
                .any(|(route, model)| route == hop.member.route_id() && model == upstream_model)
            || self.needs_room.is_some_and(|tokens| {
                hop.context_window
                    .is_some_and(|window| window > 0 && tokens >= window)
            })
    }

    fn note_skip(&mut self, order: u32, reason: SkipReason) {
        if let Some(trace) = self.trace.as_mut() {
            trace.skip_candidate(order, reason);
        }
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
    /// Fingerprint of the provider API key that produced the completion.
    /// Empty when that member has no API key.
    pub provider_key_id: String,
    /// Model name sent to that provider. The gateway compares it with the
    /// name the client asked for before touching a raw body.
    pub upstream_model: String,
    /// Codec id the answering member speaks. Usage recording uses it to tell
    /// an input total that already includes cache reads from one that does not.
    pub protocol: String,
    /// Why conversation affinity kept or released the previous answerer.
    pub affinity_reason: Option<String>,
    /// The completion. A stream has produced its first content event, a raw
    /// body its first chunk, or either ended empty. An earlier failure is not returned here.
    pub completion: Completion,
    /// Service tier sent to the member that answered.
    pub service_tier_sent: String,
    /// Drop placeholder `auto` and `default` served words for this member's host.
    pub clear_placeholder_served: bool,
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
    /// Fingerprint of the provider API key that answered.
    /// Empty when that member has no API key.
    pub provider_key_id: String,
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
    /// Every candidate key for the model reached its spend cap.
    #[error("spend cap reached")]
    SpendCapReached {
        /// Seconds until the next period reset, when known.
        retry_after: Option<u64>,
    },
}

fn cooling_labels(cooldowns: Option<&Arc<Cooldowns>>, resolved: &Resolved) -> Vec<Option<String>> {
    resolved
        .hops
        .iter()
        .map(|hop| {
            let model = hop.wire_model.as_deref().unwrap_or(&resolved.bare);
            cooldowns.and_then(|cooldowns| {
                cooldowns
                    .cooling_for_model(hop.member.id(), model)
                    .map(|state| state.cause().as_str().to_owned())
            })
        })
        .collect()
}

/// The active rest when it is a quota refusal that named its own deadline.
fn vendor_timed_quota_rest(cooldowns: &Cooldowns, hop: &Hop, model: &str) -> Option<Cooling> {
    cooldowns
        .cooling_for_model(hop.member.id(), model)
        .filter(|cooling| cooling.cause() == CooldownCause::Quota && cooling.vendor_timed())
}

fn order_hops(hops: Vec<Hop>, cooldowns: &Cooldowns, bare: &str) -> (Vec<Hop>, Vec<Hop>) {
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
    if cooling.len() < 2 {
        ready.extend(cooling.into_iter().map(|(hop, _)| hop));
        return (ready, Vec::new());
    }
    let mut probe = None;
    for (index, (_, until)) in cooling.iter().enumerate() {
        if probe.is_none_or(|(_, soonest): (usize, Instant)| *until < soonest) {
            probe = Some((index, *until));
        }
    }
    let probe = probe.map(|(index, _)| index);
    let mut left = Vec::new();
    for (index, (hop, until)) in cooling.into_iter().enumerate() {
        if probe == Some(index) {
            ready.push(hop);
        } else {
            tracing::debug!(
                "member.id" = %hop.member.id(),
                "cooldown.until" = ?until,
                "member_cooldown_skip"
            );
            left.push((hop, until));
        }
    }
    left.sort_by_key(|(_, until)| *until);
    (ready, left.into_iter().map(|(hop, _)| hop).collect())
}

fn apply_wire(request: &mut Request, hop: &Hop, bare: &str, served: &[hofund_codec::ServedModel]) {
    request.model = hop.wire_model.clone().unwrap_or_else(|| bare.to_owned());
    if request.params.max_tokens.is_none() {
        request.params.max_tokens = hop.max_output_tokens;
    }
    let upstream_model = request.model.clone();
    hofund_codec::prepare_advisor_for_hop(
        request,
        hop.member.protocol(),
        hop.member.route_id(),
        &upstream_model,
        served,
    );
    let attempt = hop.member.attempt();
    hofund_codec::service_tier::apply_outbound(
        request,
        &hop.service_tier,
        &attempt.base_url,
        hofund_codec::service_tier::spoken_protocol(
            attempt.protocol.as_str(),
            attempt.wire_protocol.as_str(),
        ),
    );
}

fn candidates(found: &[(String, Hop, affinity::Stays, Option<at_once::AtOnceMember>)]) -> String {
    found
        .iter()
        .map(|(route, hop, ..)| format!("{}/{}", hop.member.route_id(), route))
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

async fn accept(completion: Completion, chat: bool) -> Result<Completion, PluginError> {
    let empty = || PluginError::status_error(502, EMPTY_REPLY_MESSAGE).without_cooldown();
    match completion {
        Completion::Stream(mut stream) => {
            let mut held = Vec::new();
            while let Some(item) = stream.next().await {
                let event = item?;
                let commits = stream_commits(&event);
                let terminal = matches!(&event, StreamEvent::MessageStop(_));
                held.push(event);
                if chat {
                    if let Some(usage) = decoded_chat_stream_empty(&held) {
                        if terminal {
                            return Err(empty_with_usage(usage));
                        }
                        continue;
                    }
                    let head = futures_util::stream::iter(held.into_iter().map(Ok));
                    return Ok(Completion::Stream(Box::pin(head.chain(stream))));
                }
                if commits {
                    if decoded_stream_empty(&held) {
                        return Err(empty());
                    }
                    let head = futures_util::stream::iter(held.into_iter().map(Ok));
                    return Ok(Completion::Stream(Box::pin(head.chain(stream))));
                }
            }
            if chat {
                if let Some(usage) = decoded_chat_stream_empty(&held) {
                    return Err(empty_with_usage(usage));
                }
            } else if decoded_stream_empty(&held) {
                return Err(empty());
            }
            Ok(Completion::Stream(Box::pin(futures_util::stream::iter(
                held.into_iter().map(Ok),
            ))))
        }
        Completion::Raw(mut body) => {
            let mut buffered = Vec::new();
            let mut chat_check = ChatStreamCheck::default();
            loop {
                // Lifecycle-only events are held only while the next chunk is
                // already there: an upstream that goes quiet after
                // `response.created` gets that event released, so the client
                // learns the response id (and can interrupt) instead of the
                // turn hanging until the upstream speaks again.
                let next = if !chat && relayed_sse_event_boundary(&buffered) {
                    match futures_util::FutureExt::now_or_never(body.next_chunk()) {
                        Some(next) => next,
                        None => {
                            let head = Bytes::from(std::mem::take(&mut buffered));
                            return Ok(Completion::Raw(BodyStream::resume(head, body)));
                        }
                    }
                } else {
                    body.next_chunk().await
                };
                match next {
                    None => {
                        if chat {
                            chat_check.finish();
                            if let Some(usage) = chat_check.empty_usage() {
                                return Err(empty_with_usage(usage));
                            }
                        } else if relayed_sse_empty_without_content(&buffered) {
                            return Err(empty());
                        }
                        // the upstream has ended: hand back only what was read,
                        // never the finished stream (it must not be polled again)
                        return Ok(if buffered.is_empty() {
                            Completion::Raw(BodyStream::from_stream(futures_util::stream::empty()))
                        } else {
                            Completion::Raw(BodyStream::from_bytes(Bytes::from(buffered)))
                        });
                    }
                    Some(Err(error)) if buffered.is_empty() => return Err(error),
                    Some(Err(error)) => {
                        // a stream severed after some bytes keeps them, then
                        // carries the error, as an unheld stream would
                        let head = Bytes::from(std::mem::take(&mut buffered));
                        return Ok(Completion::Raw(BodyStream::from_stream(
                            futures_util::stream::iter([Ok(head), Err(error)]),
                        )));
                    }
                    Some(Ok(chunk)) => {
                        buffered.extend_from_slice(&chunk);
                        if chat {
                            chat_check.observe(&chunk);
                            if chat_check.is_terminal()
                                && let Some(usage) = chat_check.empty_usage()
                            {
                                return Err(empty_with_usage(usage));
                            }
                            if chat_check.has_reply() || chat_check.is_opaque() {
                                let head = Bytes::from(std::mem::take(&mut buffered));
                                return Ok(Completion::Raw(BodyStream::resume(head, body)));
                            }
                            continue;
                        }
                        if relayed_sse_empty_without_content(&buffered) {
                            return Err(empty());
                        }
                        if relayed_sse_has_content(&buffered)
                            || relayed_sse_has_opaque_content(&buffered)
                            || (relayed_sse_event_boundary(&buffered)
                                && !relayed_sse_lifecycle_only(&buffered))
                        {
                            let head = Bytes::from(std::mem::take(&mut buffered));
                            return Ok(Completion::Raw(BodyStream::resume(head, body)));
                        }
                    }
                }
            }
        }
        Completion::Unary(response) => {
            if chat && chat_response_empty(&response) {
                return Err(empty_with_usage(response.usage.clone().unwrap_or_default()));
            }
            if !chat
                && response.usage.as_ref().is_some_and(|usage| {
                    usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0) == 0
                })
                && response.message.content.is_empty()
            {
                return Err(empty());
            }
            Ok(Completion::Unary(response))
        }
        other => Ok(other),
    }
}

fn empty_with_usage(usage: hofund_ir::Usage) -> PluginError {
    PluginError::status_error(502, empty_reply_message(&usage)).without_cooldown()
}

fn stream_commits(event: &StreamEvent) -> bool {
    match event {
        StreamEvent::MessageStart(_)
        | StreamEvent::ContentBlockStart(_)
        | StreamEvent::ContentBlockStop(_)
        | StreamEvent::Ping(_) => false,
        StreamEvent::StillWorking(_) => true,
        StreamEvent::MessageDelta(delta) => delta.stop.is_some(),
        _ => true,
    }
}

fn member_speaks_chat(hop: &Hop) -> bool {
    let attempt = hop.member.attempt();
    hofund_codec::service_tier::spoken_protocol(
        hop.member.protocol(),
        attempt.wire_protocol.as_str(),
    ) == hofund_codec::CHAT_PROTOCOL_ID
}

fn produced_chat(codec: Option<hofund_plugin_api::ProtocolId>) -> bool {
    codec.is_some_and(|codec| codec.as_str() == hofund_codec::CHAT_PROTOCOL_ID)
}

async fn call_member(
    hop: &Hop,
    index: usize,
    inbound_protocol: &str,
    attempt: Request,
    hook: Option<Arc<dyn AttemptHook>>,
) -> (bool, Result<Completion, PluginError>) {
    let span = member_span(hop, index, inbound_protocol);
    let started = Instant::now();
    let member_chat = member_speaks_chat(hop);
    let (chat, result) = async {
        match hop
            .member
            .execute_with_attempt(attempt, inbound_protocol, hook)
            .await
        {
            Ok(completion) => {
                let (codec, completion) = completion.split_produced();
                let chat = member_chat || produced_chat(codec);
                (chat, accept(completion, chat).await)
            }
            Err(error) => (member_chat, Err(error)),
        }
    }
    .instrument(span.clone())
    .await;
    record_call(&span, &result_status(result.as_ref().err()), started);
    (chat, result)
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

#[cfg(test)]
mod hand_on_eligible_tests {
    use super::{HandOnFailure, HandOnSeat, hand_on_eligible};

    #[test]
    fn d265_hand_on_eligible_table() {
        let cases = [
            (
                HandOnSeat::Rate,
                HandOnFailure::Momentary,
                false,
                false,
                true,
            ),
            (HandOnSeat::Rate, HandOnFailure::Own, false, false, true),
            (HandOnSeat::Rate, HandOnFailure::Own, false, true, false),
            (
                HandOnSeat::Rate,
                HandOnFailure::Momentary,
                true,
                false,
                false,
            ),
            (
                HandOnSeat::Momentary,
                HandOnFailure::Momentary,
                false,
                false,
                true,
            ),
            (
                HandOnSeat::Momentary,
                HandOnFailure::Own,
                false,
                false,
                false,
            ),
            (
                HandOnSeat::Momentary,
                HandOnFailure::Momentary,
                false,
                true,
                false,
            ),
            (
                HandOnSeat::Expired,
                HandOnFailure::Momentary,
                false,
                false,
                true,
            ),
            (HandOnSeat::Expired, HandOnFailure::Own, false, false, false),
            (
                HandOnSeat::Expired,
                HandOnFailure::Momentary,
                false,
                true,
                false,
            ),
            (
                HandOnSeat::Ineligible,
                HandOnFailure::Momentary,
                false,
                false,
                false,
            ),
            (
                HandOnSeat::Ineligible,
                HandOnFailure::Own,
                false,
                false,
                false,
            ),
            (
                HandOnSeat::Ineligible,
                HandOnFailure::Own,
                true,
                true,
                false,
            ),
            (
                HandOnSeat::Rate,
                HandOnFailure::LastResort,
                false,
                false,
                true,
            ),
            (
                HandOnSeat::Momentary,
                HandOnFailure::LastResort,
                false,
                false,
                true,
            ),
            (
                HandOnSeat::Expired,
                HandOnFailure::LastResort,
                false,
                false,
                true,
            ),
            (
                HandOnSeat::Momentary,
                HandOnFailure::LastResort,
                false,
                true,
                false,
            ),
            (
                HandOnSeat::Ineligible,
                HandOnFailure::LastResort,
                false,
                false,
                false,
            ),
        ];
        for (seat, failure, window_blocks, guarded, expected) in cases {
            assert_eq!(
                hand_on_eligible(seat, failure, window_blocks, guarded),
                expected,
                "{seat:?} {failure:?} window={window_blocks} guard={guarded}"
            );
        }
    }
}

#[cfg(test)]
mod stream_commits_tests {
    use super::stream_commits;
    use hofund_ir::{MessageStart, Ping, StillWorking, StreamEvent};

    #[test]
    fn still_working_commits_and_start_or_ping_do_not() {
        assert!(stream_commits(&StreamEvent::StillWorking(
            StillWorking::default()
        )));
        assert!(!stream_commits(&StreamEvent::Ping(Ping::default())));
        assert!(!stream_commits(&StreamEvent::MessageStart(MessageStart {
            id: None,
            model: None,
            usage: None,
            extensions: Default::default(),
        })));
    }
}
