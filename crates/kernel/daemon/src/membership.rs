//! Adapts each configured provider into a [`Member`], then into a tower service.
//!
//! The service is wrapped in the stage-1 layers: a kernel deadline, panic
//! containment, cooldown accounting, and the router's span (the span is entered
//! by the router, which knows the attempt). A panic marks the member failed and
//! publishes a snapshot that leaves it out until its timed cooldown expires.
//! The body deadline is idle time since the last chunk. A stream that keeps
//! sending is not cut off.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{FutureExt, Stream, StreamExt};
use hofund_plugin_api::{
    BodyStream, Completion, CountOutcome, MemberId, MemberKey, PluginError, PresetId,
    QuotaObservation, RawAnswer, RawBody, Request,
};
use hofund_plugin_host::ObserveBackend;
use hofund_provider::{
    AttemptHook as ProviderAttemptHook, Completion as ProviderCompletion,
    CountOutcome as ProviderCount, Provider, ProviderError, ProviderErrorKind,
};
use hofund_router::{
    Attempt, AttemptHook, Cooldowns, FailoverPolicy, Hop, Member, MemberAttempt, Route, Router,
    Snapshot, strategy::StrategyState,
};
use tower::Service;

use crate::DaemonError;

#[derive(Clone)]
pub(crate) struct BuiltProvider {
    pub adapter: Arc<dyn Provider>,
    pub protocol: &'static str,
    pub timeout: Duration,
    /// Catalog or implementation id recorded on the member span.
    pub plugin_id: String,
    /// Preset id from configuration. Empty when the provider has no preset.
    pub preset_id: String,
    pub fingerprint: String,
}

pub(crate) struct Table {
    hops: Mutex<BTreeMap<String, Vec<HopSpec>>>,
    policy: Mutex<FailoverPolicy>,
    cooldowns: Arc<Cooldowns>,
    strategies: Arc<StrategyState>,
    snapshot: Snapshot,
}

struct HopSpec {
    member: Arc<LiveMember>,
    wire_model: Option<String>,
    max_output_tokens: Option<u64>,
    routing: String,
    weight: u64,
    free: bool,
}

struct LiveMember {
    id: MemberId,
    route_id: String,
    plugin_id: String,
    preset_id: String,
    protocol: &'static str,
    route: hofund_provider::AttemptRoute,
    subscription: bool,
    failed: Arc<AtomicU64>,
    stack: Cooldown<Guard<ProviderCall>>,
}

#[derive(Clone)]
struct ProviderCall {
    provider: Arc<dyn Provider>,
}

enum Job {
    Execute(Request, String, Option<Arc<dyn AttemptHook>>, Attempt),
    Count(Request, String),
    Forward(String, RawBody),
}

struct AttemptAdapter {
    hook: Arc<dyn AttemptHook>,
    info: Attempt,
}

impl ProviderAttemptHook for AttemptAdapter {
    fn prepare(&self, _route: &hofund_provider::AttemptRoute, body: &[u8]) -> Option<Vec<u8>> {
        self.hook.prepare(&self.info, body)
    }

    fn refused(&self, _route: &hofund_provider::AttemptRoute, status: u16, body: &[u8]) -> bool {
        self.hook.refused(&self.info, status, body)
    }
}

enum Out {
    Execute(Completion),
    Count(CountOutcome),
    Forward(RawAnswer),
}

#[derive(Clone)]
struct Guard<S> {
    inner: S,
    id: MemberId,
    timeout: Duration,
    failed: Arc<AtomicU64>,
    cooldowns: Arc<Cooldowns>,
    reload: Weak<Table>,
}

#[derive(Clone)]
struct Cooldown<S> {
    inner: S,
    id: MemberId,
    cooldowns: Arc<Cooldowns>,
}

struct MemberObserve {
    id: MemberId,
    cooldowns: Arc<Cooldowns>,
}

impl ObserveBackend for MemberObserve {
    fn observe(&self, observation: QuotaObservation) -> Result<(), PluginError> {
        self.cooldowns.observe(&self.id, &observation);
        Ok(())
    }
}

pub(crate) fn observe_backend(
    provider_id: &str,
    cooldowns: Arc<Cooldowns>,
) -> Arc<dyn ObserveBackend> {
    observe_backend_for(member_id(provider_id), cooldowns)
}

pub(crate) fn observe_backend_for(
    id: MemberId,
    cooldowns: Arc<Cooldowns>,
) -> Arc<dyn ObserveBackend> {
    Arc::new(MemberObserve { id, cooldowns })
}

#[cfg(test)]
pub(crate) fn open(
    built: &BTreeMap<String, BuiltProvider>,
    routes: BTreeMap<String, Vec<PlannedHop>>,
    policy: FailoverPolicy,
    cooldowns: Arc<Cooldowns>,
) -> Result<Snapshot, DaemonError> {
    let table = build_table(built, routes, policy, cooldowns)?;
    let snapshot = table.snapshot.clone();
    let owner = Arc::clone(&table) as Arc<dyn std::any::Any + Send + Sync>;
    Ok(snapshot.hold(owner))
}

pub(crate) fn open_with_table(
    built: &BTreeMap<String, BuiltProvider>,
    routes: BTreeMap<String, Vec<PlannedHop>>,
    policy: FailoverPolicy,
    cooldowns: Arc<Cooldowns>,
) -> Result<(Snapshot, Arc<Table>), DaemonError> {
    let table = build_table(built, routes, policy, cooldowns)?;
    Ok((table.snapshot.clone(), table))
}

fn build_table(
    built: &BTreeMap<String, BuiltProvider>,
    routes: BTreeMap<String, Vec<PlannedHop>>,
    policy: FailoverPolicy,
    cooldowns: Arc<Cooldowns>,
) -> Result<Arc<Table>, DaemonError> {
    let strategies = Arc::new(StrategyState::with_builtins());
    let snapshot = Snapshot::new(
        Router::from_hops(BTreeMap::new(), policy.clone())
            .with_strategies(Arc::clone(&strategies))
            .with_cooldowns(Arc::clone(&cooldowns)),
    );
    let table = Arc::new(Table {
        hops: Mutex::new(BTreeMap::new()),
        policy: Mutex::new(policy),
        cooldowns,
        strategies,
        snapshot,
    });
    let stored = build_specs(&table, built, routes)?;
    *table.hops.lock().expect("membership") = stored;
    table.publish();
    Ok(table)
}

fn build_specs(
    table: &Arc<Table>,
    built: &BTreeMap<String, BuiltProvider>,
    routes: BTreeMap<String, Vec<PlannedHop>>,
) -> Result<BTreeMap<String, Vec<HopSpec>>, DaemonError> {
    let mut stored = BTreeMap::new();
    let mut seen_ids: BTreeMap<String, String> = BTreeMap::new();
    for (model, planned) in routes {
        let mut specs = Vec::new();
        for hop in planned {
            if let Some(subscription) = &hop.subscription {
                return Err(DaemonError::new(format!(
                    "route `{model}` subscription `{subscription}` was not expanded into members"
                )));
            }
            let label = hop
                .member_id
                .clone()
                .unwrap_or_else(|| member_id(&hop.provider))
                .to_string();
            if let Some(previous) = seen_ids.insert(label.clone(), hop.provider.clone())
                && previous != hop.provider
            {
                return Err(DaemonError::new(format!(
                    "member id `{label}` is shared by `{previous}` and `{}`",
                    hop.provider
                )));
            }
            let built = built.get(&hop.provider).ok_or_else(|| {
                DaemonError::new(format!(
                    "route `{model}` names unknown provider `{}`",
                    hop.provider
                ))
            })?;
            let subscription = hop.member_id.is_some();
            specs.push(HopSpec {
                member: Arc::new(LiveMember::new(
                    built,
                    &hop.provider,
                    hop.member_id.clone(),
                    subscription,
                    Arc::clone(&table.cooldowns),
                    Arc::downgrade(table),
                )),
                wire_model: hop.wire_model,
                max_output_tokens: hop.max_output_tokens,
                routing: hop.routing,
                weight: hop.weight,
                free: hop.free,
            });
        }
        stored.insert(model, specs);
    }
    Ok(stored)
}

#[derive(Clone)]
pub(crate) struct PlannedHop {
    /// Provider config id when this hop is a provider. Empty for subscription hops
    /// before expansion. After expansion, the built-map key for the member.
    pub provider: String,
    /// Subscription config id when this hop should expand to account members.
    pub subscription: Option<String>,
    /// Explicit member id for subscription accounts. Provider hops leave this unset.
    pub member_id: Option<MemberId>,
    pub wire_model: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub routing: String,
    pub weight: u64,
    pub free: bool,
}

impl Table {
    pub(crate) fn replace(
        self: &Arc<Self>,
        built: &BTreeMap<String, BuiltProvider>,
        routes: BTreeMap<String, Vec<PlannedHop>>,
        policy: FailoverPolicy,
    ) -> Result<(), DaemonError> {
        let stored = build_specs(self, built, routes)?;
        *self.hops.lock().expect("membership") = stored;
        *self.policy.lock().expect("failover policy") = policy;
        self.publish();
        Ok(())
    }

    pub(crate) fn publish(&self) {
        let hops = self.hops.lock().expect("membership");
        let routes = hops
            .iter()
            .map(|(model, specs)| {
                let list = specs
                    .iter()
                    .filter(|spec| !spec.member.failed())
                    .map(|spec| Hop {
                        member: Arc::clone(&spec.member) as Arc<dyn Member>,
                        wire_model: spec.wire_model.clone(),
                        max_output_tokens: spec.max_output_tokens,
                        context_window: None,
                        weight: spec.weight,
                        free: spec.free,
                    })
                    .collect();
                let routing = specs.first().map_or("smart", |spec| spec.routing.as_str());
                (model.clone(), Route::new(list).with_routing(routing))
            })
            .collect();
        let policy = self.policy.lock().expect("failover policy").clone();
        self.snapshot.store(
            Router::from_routes(routes, policy)
                .with_strategies(Arc::clone(&self.strategies))
                .with_cooldowns(Arc::clone(&self.cooldowns)),
        );
    }
}

impl LiveMember {
    fn new(
        built: &BuiltProvider,
        route_id: &str,
        id: Option<MemberId>,
        subscription: bool,
        cooldowns: Arc<Cooldowns>,
        reload: Weak<Table>,
    ) -> Self {
        let id = id.unwrap_or_else(|| member_id(route_id));
        let failed = Arc::new(AtomicU64::new(0));
        let guard = Guard {
            inner: ProviderCall {
                provider: Arc::clone(&built.adapter),
            },
            id: id.clone(),
            timeout: built.timeout,
            failed: Arc::clone(&failed),
            cooldowns: Arc::clone(&cooldowns),
            reload,
        };
        let stack = Cooldown {
            inner: guard,
            id: id.clone(),
            cooldowns,
        };
        Self {
            id,
            route_id: route_id.to_owned(),
            plugin_id: built.plugin_id.clone(),
            preset_id: built.preset_id.clone(),
            protocol: built.protocol,
            route: built.adapter.attempt_route(),
            subscription,
            failed,
            stack,
        }
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire) != 0
    }
}

#[async_trait]
impl Member for LiveMember {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        &self.route_id
    }

    fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    fn preset_id(&self) -> &str {
        &self.preset_id
    }

    fn protocol(&self) -> &str {
        self.protocol
    }

    fn attempt(&self) -> MemberAttempt {
        MemberAttempt {
            provider_id: self.route_id.clone(),
            protocol: self.protocol.to_owned(),
            base_url: self.route.base_url.clone(),
            route: self.route.route.clone(),
            keeps_responses: self.route.keeps_responses,
            subscription: self.subscription,
            plugin: !self.plugin_id.is_empty(),
        }
    }

    async fn execute(&self, request: Request, inbound: &str) -> Result<Completion, PluginError> {
        self.execute_with_attempt(request, inbound, None).await
    }

    async fn execute_with_attempt(
        &self,
        request: Request,
        inbound: &str,
        hook: Option<Arc<dyn AttemptHook>>,
    ) -> Result<Completion, PluginError> {
        let info = Attempt {
            member: self.attempt(),
            inbound_protocol: inbound.to_owned(),
        };
        let mut stack = self.stack.clone();
        match stack
            .call(Job::Execute(request, inbound.to_owned(), hook, info))
            .await
        {
            Ok(Out::Execute(completion)) => Ok(completion),
            Ok(Out::Count(_)) => Err(PluginError::internal("member returned a count")),
            Ok(Out::Forward(_)) => Err(PluginError::internal("member returned an answer")),
            Err(error) => Err(error),
        }
    }

    async fn count_input(
        &self,
        request: Request,
        inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        let mut stack = self.stack.clone();
        match stack.call(Job::Count(request, inbound.to_owned())).await {
            Ok(Out::Count(outcome)) => Ok(outcome),
            Ok(Out::Execute(_)) => Err(PluginError::internal("member returned a completion")),
            Ok(Out::Forward(_)) => Err(PluginError::internal("member returned an answer")),
            Err(error) => Err(error),
        }
    }

    async fn forward(&self, path: &str, body: RawBody) -> Result<RawAnswer, PluginError> {
        let mut stack = self.stack.clone();
        match stack.call(Job::Forward(path.to_owned(), body)).await {
            Ok(Out::Forward(answer)) => Ok(answer),
            Ok(Out::Execute(_)) => Err(PluginError::internal("member returned a completion")),
            Ok(Out::Count(_)) => Err(PluginError::internal("member returned a count")),
            Err(error) => Err(error),
        }
    }
}

impl<S> Service<Job> for Guard<S>
where
    S: Service<Job, Response = Out, Error = PluginError> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = Out;
    type Error = PluginError;
    type Future = Pin<Box<dyn Future<Output = Result<Out, PluginError>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, job: Job) -> Self::Future {
        let mut inner = self.inner.clone();
        let id = self.id.clone();
        let timeout = self.timeout;
        let failed = Arc::clone(&self.failed);
        let cooldowns = Arc::clone(&self.cooldowns);
        let reload = Weak::clone(&self.reload);
        Box::pin(async move {
            let fut = AssertUnwindSafe(inner.call(job));
            match tokio::time::timeout(timeout, fut.catch_unwind()).await {
                Ok(Ok(Ok(Out::Execute(completion)))) => Ok(Out::Execute(watch_completion(
                    completion, id, timeout, failed, cooldowns, reload,
                ))),
                Ok(Ok(result)) => result,
                Ok(Err(_panic)) => {
                    member_panicked(id, failed, cooldowns, reload);
                    Err(PluginError::internal("member panicked"))
                }
                Err(_elapsed) => Err(PluginError::timeout("kernel deadline exceeded")),
            }
        })
    }
}

impl<S> Service<Job> for Cooldown<S>
where
    S: Service<Job, Response = Out, Error = PluginError> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = Out;
    type Error = PluginError;
    type Future = Pin<Box<dyn Future<Output = Result<Out, PluginError>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, job: Job) -> Self::Future {
        let mut inner = self.inner.clone();
        let id = self.id.clone();
        let cooldowns = Arc::clone(&self.cooldowns);
        let revision = cooldowns.revision(&id);
        // A count or a forwarded call is a side call: it is never a served
        // chat, and an endpoint it does not have says nothing about the member.
        let is_side = matches!(job, Job::Count(..) | Job::Forward(..));
        Box::pin(async move {
            match inner.call(job).await {
                Ok(Out::Execute(completion)) => Ok(Out::Execute(record_completion(
                    completion, id, cooldowns, revision,
                ))),
                Ok(out @ (Out::Count(_) | Out::Forward(_))) => Ok(out),
                Err(error) => {
                    if !is_side || count_cools(&error) {
                        cooldowns.record_failure(&id, &error);
                    }
                    Err(error)
                }
            }
        })
    }
}

fn count_cools(error: &PluginError) -> bool {
    use hofund_plugin_api::PluginErrorKind;

    match error.kind() {
        PluginErrorKind::Status => error.status() == Some(429),
        PluginErrorKind::Connect
        | PluginErrorKind::Timeout
        | PluginErrorKind::Transport
        | PluginErrorKind::Interrupted
        | PluginErrorKind::Io => true,
        _ => false,
    }
}

fn member_panicked(
    id: MemberId,
    failed: Arc<AtomicU64>,
    cooldowns: Arc<Cooldowns>,
    reload: Weak<Table>,
) {
    let cooling = cooldowns.record_panic(&id);
    let generation = failed.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    if let Some(table) = reload.upgrade() {
        table.publish();
    }
    tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(cooling.until())).await;
        if failed
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && let Some(table) = reload.upgrade()
        {
            table.publish();
        }
    });
}

impl Service<Job> for ProviderCall {
    type Response = Out;
    type Error = PluginError;
    type Future = Pin<Box<dyn Future<Output = Result<Out, PluginError>> + Send>>;

    fn poll_ready(&mut self, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, job: Job) -> Self::Future {
        let provider = Arc::clone(&self.provider);
        Box::pin(async move {
            match job {
                Job::Execute(request, inbound, hook, info) => {
                    let adapter = hook.map(|hook| AttemptAdapter { hook, info });
                    let completion = provider
                        .execute_with_attempt(
                            request,
                            &inbound,
                            adapter
                                .as_ref()
                                .map(|adapter| adapter as &dyn ProviderAttemptHook),
                        )
                        .await
                        .map_err(into_plugin)?;
                    Ok(Out::Execute(map_completion(completion)))
                }
                Job::Count(request, inbound) => {
                    let outcome = provider
                        .count_input(request, &inbound)
                        .await
                        .map_err(into_plugin)?;
                    Ok(Out::Count(map_count(outcome)))
                }
                Job::Forward(path, body) => {
                    let answer = provider.forward(&path, body).await.map_err(into_plugin)?;
                    Ok(Out::Forward(answer))
                }
            }
        })
    }
}

fn watch_completion(
    completion: Completion,
    id: MemberId,
    idle: Duration,
    failed: Arc<AtomicU64>,
    cooldowns: Arc<Cooldowns>,
    reload: Weak<Table>,
) -> Completion {
    match completion {
        Completion::Stream(stream) => Completion::Stream(Box::pin(Watched::new(
            stream, id, idle, failed, cooldowns, reload,
        ))),
        Completion::Raw(body) => Completion::Raw(BodyStream::from_stream(Watched::new(
            body_to_stream(body),
            id,
            idle,
            failed,
            cooldowns,
            reload,
        ))),
        other => other,
    }
}

fn record_completion(
    completion: Completion,
    id: MemberId,
    cooldowns: Arc<Cooldowns>,
    revision: u64,
) -> Completion {
    match completion {
        Completion::Stream(stream) => Completion::Stream(Box::pin(OutcomeStream::new(
            stream, id, cooldowns, revision,
        ))),
        Completion::Raw(body) => Completion::Raw(BodyStream::from_stream(OutcomeStream::new(
            body_to_stream(body),
            id,
            cooldowns,
            revision,
        ))),
        other => {
            cooldowns.record_success_if_unchanged(&id, revision);
            other
        }
    }
}

struct OutcomeStream<S> {
    inner: Pin<Box<S>>,
    id: MemberId,
    cooldowns: Arc<Cooldowns>,
    revision: u64,
    succeeded: bool,
}

impl<S> OutcomeStream<S> {
    fn new(inner: S, id: MemberId, cooldowns: Arc<Cooldowns>, revision: u64) -> Self {
        Self {
            inner: Box::pin(inner),
            id,
            cooldowns,
            revision,
            succeeded: false,
        }
    }
}

impl<S, T> Stream for OutcomeStream<S>
where
    S: Stream<Item = Result<T, PluginError>>,
{
    type Item = Result<T, PluginError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(context) {
            Poll::Ready(Some(Ok(item))) => {
                if !this.succeeded {
                    this.cooldowns
                        .record_success_if_unchanged(&this.id, this.revision);
                    this.succeeded = true;
                }
                Poll::Ready(Some(Ok(item)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.cooldowns.record_failure(&this.id, &error);
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }
}

fn body_to_stream(
    body: BodyStream,
) -> impl Stream<Item = Result<bytes::Bytes, PluginError>> + Send {
    futures_util::stream::unfold(body, |mut body| async move {
        let next = body.next_chunk().await?;
        Some((next, body))
    })
}

struct Watched<S> {
    inner: Pin<Box<S>>,
    id: MemberId,
    idle: Duration,
    idle_deadline: tokio::time::Instant,
    sleep: Pin<Box<tokio::time::Sleep>>,
    failed: Arc<AtomicU64>,
    cooldowns: Arc<Cooldowns>,
    reload: Weak<Table>,
    done: bool,
}

impl<S> Watched<S> {
    fn new(
        inner: S,
        id: MemberId,
        idle: Duration,
        failed: Arc<AtomicU64>,
        cooldowns: Arc<Cooldowns>,
        reload: Weak<Table>,
    ) -> Self {
        let idle_deadline = tokio::time::Instant::now() + idle;
        Self {
            inner: Box::pin(inner),
            id,
            idle,
            idle_deadline,
            sleep: Box::pin(tokio::time::sleep_until(idle_deadline)),
            failed,
            cooldowns,
            reload,
            done: false,
        }
    }
}

impl<S, T> Stream for Watched<S>
where
    S: Stream<Item = Result<T, PluginError>>,
{
    type Item = Result<T, PluginError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        if tokio::time::Instant::now() >= this.idle_deadline {
            this.done = true;
            return Poll::Ready(Some(Err(PluginError::timeout("kernel deadline exceeded"))));
        }
        let polled =
            std::panic::catch_unwind(AssertUnwindSafe(|| this.inner.as_mut().poll_next(context)));
        match polled {
            Err(_panic) => {
                this.done = true;
                member_panicked(
                    this.id.clone(),
                    Arc::clone(&this.failed),
                    Arc::clone(&this.cooldowns),
                    Weak::clone(&this.reload),
                );
                Poll::Ready(Some(Err(PluginError::internal("member panicked"))))
            }
            Ok(Poll::Pending) => match this.sleep.as_mut().poll(context) {
                Poll::Ready(()) => {
                    this.done = true;
                    Poll::Ready(Some(Err(PluginError::timeout("kernel deadline exceeded"))))
                }
                Poll::Pending => Poll::Pending,
            },
            Ok(Poll::Ready(item)) => {
                this.idle_deadline = tokio::time::Instant::now() + this.idle;
                this.sleep.as_mut().reset(this.idle_deadline);
                Poll::Ready(item)
            }
        }
    }
}

fn map_completion(completion: ProviderCompletion) -> Completion {
    match completion {
        ProviderCompletion::Unary(response) => Completion::Unary(response),
        ProviderCompletion::Stream(stream) => {
            Completion::Stream(Box::pin(stream.map(|item| item.map_err(into_plugin))))
        }
        ProviderCompletion::Raw(stream) => Completion::Raw(BodyStream::from_stream(
            stream.map(|item| item.map_err(into_plugin)),
        )),
    }
}

fn map_count(outcome: ProviderCount) -> CountOutcome {
    match outcome {
        ProviderCount::Counted(tokens) => CountOutcome::Counted(tokens),
        ProviderCount::Unsupported => CountOutcome::Unsupported,
    }
}

pub(crate) fn into_plugin(error: ProviderError) -> PluginError {
    let retry_after = error.retry_after;
    let reset_after = error.reset_after;
    let credit = error.is_credit();
    let credit_soon = error.credit_soon();
    let cooldown = error.cools();
    let status = error.status;
    let credit_message = credit.then(|| error.message.clone());
    let mut converted = match error.kind {
        ProviderErrorKind::Connect => PluginError::connect(error.message),
        ProviderErrorKind::Timeout => PluginError::timeout(error.message),
        ProviderErrorKind::Transport => PluginError::transport(error.message),
        ProviderErrorKind::Interrupted => PluginError::interrupted(error.message),
        ProviderErrorKind::Status => {
            PluginError::status_error(error.status.unwrap_or(502), error.message)
        }
        ProviderErrorKind::Decode => PluginError::decode(error.message),
        ProviderErrorKind::Unsupported => PluginError::unsupported(error.message),
        ProviderErrorKind::Auth => PluginError::auth(error.message),
        ProviderErrorKind::Quota => match error.status {
            Some(status) => PluginError::quota_status(status, error.message),
            None => PluginError::quota(error.message),
        },
        ProviderErrorKind::Config => PluginError::config(error.message),
        ProviderErrorKind::Cancelled => PluginError::cancelled(error.message),
        ProviderErrorKind::Io => PluginError::io(error.message),
        ProviderErrorKind::Internal => PluginError::internal(error.message),
    };
    if let Some(message) = credit_message {
        converted = PluginError::credit_status(status.unwrap_or(402), message);
    }
    if !cooldown {
        converted = converted.without_cooldown();
    }
    if let Some(after) = retry_after {
        converted = converted.with_retry_after(after);
    }
    if let Some(after) = reset_after {
        converted = converted.with_reset_after(after);
    }
    if credit_soon {
        converted = converted.with_credit_soon();
    }
    converted
}

pub(crate) fn member_id(provider_id: &str) -> MemberId {
    let kebab = kebab_of(provider_id);
    let preset =
        PresetId::try_from(kebab.as_str()).unwrap_or_else(|_| PresetId::from_static("provider"));
    MemberId::from_preset(preset, MemberKey::from_static("default"))
}

fn kebab_of(value: &str) -> String {
    let mut out = String::new();
    let mut hyphen = true;
    for byte in value.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            hyphen = false;
            out.push(byte as char);
        } else if byte.is_ascii_uppercase() {
            hyphen = false;
            out.push(byte.to_ascii_lowercase() as char);
        } else if !hyphen {
            hyphen = true;
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "provider".to_owned()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{BuiltProvider, PlannedHop, open};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use hofund_plugin_api::{RawAnswer, RawBody, Request};
    use hofund_provider::{Completion, Provider, ProviderError};
    use hofund_router::{CooldownPolicy, Cooldowns, FailoverPolicy};

    fn cooldowns() -> Arc<Cooldowns> {
        Arc::new(Cooldowns::with_clock(
            CooldownPolicy::default(),
            Arc::new(|| tokio::time::Instant::now().into_std()),
        ))
    }

    #[test]
    fn provider_errors_round_trip_with_kinds_status_and_retry_hints() {
        for error in [
            ProviderError::connect("refused"),
            ProviderError::timeout("slow"),
            ProviderError::transport("reset"),
            ProviderError::interrupted("cut off"),
            ProviderError::decode("invalid"),
            ProviderError::unsupported("no count"),
            ProviderError::auth("denied"),
            ProviderError::quota("empty"),
            ProviderError::config("bad"),
            ProviderError::cancelled("client left"),
            ProviderError::io("disk"),
            ProviderError::internal("bug"),
            ProviderError::status(429, "slow"),
        ] {
            let error = error
                .with_retry_after(Duration::from_secs(120))
                .with_reset_after(Duration::from_secs(240))
                .with_credit_soon();
            let kind = error.kind;
            let message = error.message.clone();
            let status = error.status;
            let plugin = super::into_plugin(error);
            assert_eq!(plugin.retry_after(), Some(Duration::from_secs(120)));
            assert_eq!(plugin.reset_after(), Some(Duration::from_secs(240)));
            assert!(plugin.credit_soon());
            let restored = crate::adapt::provider_error(plugin);
            assert_eq!(restored.kind, kind);
            assert_eq!(restored.message, message);
            assert_eq!(restored.status, status);
            assert_eq!(restored.retry_after(), Some(Duration::from_secs(120)));
            assert_eq!(restored.reset_after(), Some(Duration::from_secs(240)));
            assert!(restored.credit_soon());
        }
    }

    struct Boom {
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for Boom {
        fn id(&self) -> &str {
            "boom"
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            panic!("member panicked");
        }
    }

    #[tokio::test]
    async fn a_panic_drops_the_member_from_the_next_snapshot() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut built = BTreeMap::new();
        built.insert(
            "boom".to_owned(),
            BuiltProvider {
                adapter: Arc::new(Boom {
                    hits: Arc::clone(&hits),
                }),
                protocol: "openai-chat",
                timeout: Duration::from_secs(60),
                plugin_id: "boom".to_owned(),
                preset_id: String::new(),
                fingerprint: String::new(),
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "boom".to_owned(),
                subscription: None,
                member_id: None,
                wire_model: None,
                max_output_tokens: None,
                routing: "smart".to_owned(),
                weight: 1,
                free: false,
            }],
        );
        let snapshot =
            open(&built, routes, FailoverPolicy::default(), cooldowns()).expect("snapshot");
        let first = snapshot.load();
        let error = match first.dispatch(&Request::new("demo"), "openai-chat").await {
            Ok(_) => panic!("panic should fail the call"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "upstream `boom` failed: member panicked");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let next = snapshot.load();
        assert!(next.models().is_empty());
        let missing = match next.dispatch(&Request::new("demo"), "openai-chat").await {
            Ok(_) => panic!("the member should be gone"),
            Err(error) => error,
        };
        assert!(missing.to_string().contains("no upstream"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    struct SecondChunk {
        polls: u8,
    }

    impl futures_util::Stream for SecondChunk {
        type Item = Result<bytes::Bytes, hofund_provider::ProviderError>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let this = self.get_mut();
            this.polls += 1;
            if this.polls == 1 {
                std::task::Poll::Ready(Some(Ok(bytes::Bytes::from_static(b"data: hi\n\n"))))
            } else {
                panic!("second chunk");
            }
        }
    }

    struct Later {
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for Later {
        fn id(&self) -> &str {
            "later"
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(Completion::Raw(Box::pin(SecondChunk { polls: 0 })))
        }
    }

    #[tokio::test]
    async fn a_panic_on_the_second_chunk_drops_the_member() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut built = BTreeMap::new();
        built.insert(
            "later".to_owned(),
            BuiltProvider {
                adapter: Arc::new(Later {
                    hits: Arc::clone(&hits),
                }),
                protocol: "openai-chat",
                timeout: Duration::from_secs(60),
                plugin_id: "openai-compat".to_owned(),
                preset_id: "openai".to_owned(),
                fingerprint: String::new(),
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "later".to_owned(),
                subscription: None,
                member_id: None,
                wire_model: None,
                max_output_tokens: None,
                routing: "smart".to_owned(),
                weight: 1,
                free: false,
            }],
        );
        let snapshot =
            open(&built, routes, FailoverPolicy::default(), cooldowns()).expect("snapshot");
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("the first chunk is delivered");
        let hofund_plugin_api::Completion::Raw(mut body) = routed.completion else {
            panic!("raw body");
        };
        let first = body.next_chunk().await.expect("chunk").expect("ok");
        assert_eq!(&first[..], b"data: hi\n\n");
        let error = body.next_chunk().await.expect("second").expect_err("panic");
        assert_eq!(error.to_string(), "member panicked");
        assert!(snapshot.load().models().is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    struct Stall;

    impl futures_util::Stream for Stall {
        type Item = Result<bytes::Bytes, hofund_provider::ProviderError>;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Pending
        }
    }

    struct Quiet {
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for Quiet {
        fn id(&self) -> &str {
            "quiet"
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            Ok(Completion::Raw(Box::pin(Stall)))
        }
    }

    #[tokio::test]
    async fn a_quiet_body_hits_the_kernel_deadline_and_stays_routable() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut built = BTreeMap::new();
        built.insert(
            "quiet".to_owned(),
            BuiltProvider {
                adapter: Arc::new(Quiet {
                    hits: Arc::clone(&hits),
                }),
                protocol: "openai-chat",
                timeout: Duration::from_millis(40),
                plugin_id: "openai-compat".to_owned(),
                preset_id: String::new(),
                fingerprint: String::new(),
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "quiet".to_owned(),
                subscription: None,
                member_id: None,
                wire_model: None,
                max_output_tokens: None,
                routing: "smart".to_owned(),
                weight: 1,
                free: false,
            }],
        );
        let snapshot =
            open(&built, routes, FailoverPolicy::default(), cooldowns()).expect("snapshot");
        let error = match snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
        {
            Ok(_) => panic!("a quiet body should hit the deadline"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("kernel deadline exceeded"),
            "{error}"
        );
        assert_eq!(snapshot.load().models(), ["demo".to_owned()]);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    struct Pace {
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for Pace {
        fn id(&self) -> &str {
            "pace"
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let stream = futures_util::stream::unfold(0u8, |n| async move {
                if n >= 5 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                Some((
                    Ok::<bytes::Bytes, ProviderError>(bytes::Bytes::from_static(b"x")),
                    n + 1,
                ))
            });
            Ok(Completion::Raw(Box::pin(stream)))
        }
    }

    // Paused time keeps each 20ms gap inside the 30ms idle. Five gaps still
    // outlast the old twice-idle cutoff, including on a coarse Windows timer.
    #[tokio::test(start_paused = true)]
    async fn an_active_body_is_not_cut_off_at_twice_the_idle() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut built = BTreeMap::new();
        built.insert(
            "pace".to_owned(),
            BuiltProvider {
                adapter: Arc::new(Pace {
                    hits: Arc::clone(&hits),
                }),
                protocol: "openai-chat",
                timeout: Duration::from_millis(30),
                plugin_id: "openai-compat".to_owned(),
                preset_id: String::new(),
                fingerprint: String::new(),
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "pace".to_owned(),
                subscription: None,
                member_id: None,
                wire_model: None,
                max_output_tokens: None,
                routing: "smart".to_owned(),
                weight: 1,
                free: false,
            }],
        );
        let snapshot =
            open(&built, routes, FailoverPolicy::default(), cooldowns()).expect("snapshot");
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("dispatch");
        let hofund_plugin_api::Completion::Raw(mut body) = routed.completion else {
            panic!("raw body");
        };
        let mut count = 0;
        while let Some(chunk) = body.next_chunk().await {
            chunk.expect("an active stream stays inside the idle deadline");
            count += 1;
        }
        assert_eq!(count, 5);
        assert_eq!(snapshot.load().models(), ["demo".to_owned()]);
    }

    #[test]
    fn colliding_member_ids_fail_startup() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut built = BTreeMap::new();
        for id in ["My_Provider", "my-provider"] {
            built.insert(
                id.to_owned(),
                BuiltProvider {
                    adapter: Arc::new(Boom {
                        hits: Arc::clone(&hits),
                    }),
                    protocol: "openai-chat",
                    timeout: Duration::from_secs(60),
                    plugin_id: "openai-compat".to_owned(),
                    preset_id: String::new(),
                    fingerprint: String::new(),
                },
            );
        }
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![
                PlannedHop {
                    provider: "My_Provider".to_owned(),
                    subscription: None,
                    member_id: None,
                    wire_model: None,
                    max_output_tokens: None,
                    routing: "smart".to_owned(),
                    weight: 1,
                    free: false,
                },
                PlannedHop {
                    provider: "my-provider".to_owned(),
                    subscription: None,
                    member_id: None,
                    wire_model: None,
                    max_output_tokens: None,
                    routing: "smart".to_owned(),
                    weight: 1,
                    free: false,
                },
            ],
        );
        let error = match open(&built, routes, FailoverPolicy::default(), cooldowns()) {
            Ok(_) => panic!("duplicate member ids should fail startup"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("my-provider"), "{error}");
    }

    fn built_member(id: &str, adapter: Arc<dyn Provider>) -> BuiltProvider {
        BuiltProvider {
            adapter,
            protocol: "openai-chat",
            timeout: Duration::from_secs(60),
            plugin_id: id.to_owned(),
            preset_id: String::new(),
            fingerprint: String::new(),
        }
    }

    fn route(ids: &[&str]) -> BTreeMap<String, Vec<PlannedHop>> {
        BTreeMap::from([(
            "demo".to_owned(),
            ids.iter()
                .map(|id| PlannedHop {
                    provider: (*id).to_owned(),
                    subscription: None,
                    member_id: None,
                    wire_model: None,
                    max_output_tokens: None,
                    routing: "smart".to_owned(),
                    weight: 1,
                    free: false,
                })
                .collect(),
        )])
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_returns_when_its_panic_cooldown_expires() {
        let hits = Arc::new(AtomicUsize::new(0));
        let built = BTreeMap::from([(
            "boom".to_owned(),
            built_member(
                "boom",
                Arc::new(Boom {
                    hits: Arc::clone(&hits),
                }),
            ),
        )]);
        let snapshot = open(
            &built,
            route(&["boom"]),
            FailoverPolicy::default(),
            cooldowns(),
        )
        .expect("snapshot");
        let error = match snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
        {
            Ok(_) => panic!("panic should fail the call"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("member panicked"));
        assert!(snapshot.load().models().is_empty());

        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(snapshot.load().models(), ["demo".to_owned()]);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_panic_doubles_the_cooldown() {
        let hits = Arc::new(AtomicUsize::new(0));
        let built = BTreeMap::from([(
            "boom".to_owned(),
            built_member(
                "boom",
                Arc::new(Boom {
                    hits: Arc::clone(&hits),
                }),
            ),
        )]);
        let snapshot = open(
            &built,
            route(&["boom"]),
            FailoverPolicy::default(),
            cooldowns(),
        )
        .expect("snapshot");
        for _ in 0..2 {
            let result = snapshot
                .load()
                .dispatch(&Request::new("demo"), "openai-chat")
                .await;
            assert!(result.is_err());
            assert!(snapshot.load().models().is_empty());
            tokio::time::advance(Duration::from_secs(60)).await;
            tokio::task::yield_now().await;
        }
        assert!(snapshot.load().models().is_empty());
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(snapshot.load().models(), ["demo".to_owned()]);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    fn raw_ok() -> Completion {
        Completion::Raw(Box::pin(futures_util::stream::once(async {
            Ok(bytes::Bytes::from_static(b"ok"))
        })))
    }

    struct Scripted {
        id: &'static str,
        hits: Arc<AtomicUsize>,
        answers: std::sync::Mutex<std::collections::VecDeque<Result<Completion, ProviderError>>>,
    }

    #[async_trait]
    impl Provider for Scripted {
        fn id(&self) -> &str {
            self.id
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.answers
                .lock()
                .expect("answers")
                .pop_front()
                .expect("scripted answer")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_cools_a_member_across_republished_snapshots() {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let second_hits = Arc::new(AtomicUsize::new(0));
        let built = BTreeMap::from([
            (
                "first".to_owned(),
                built_member(
                    "first",
                    Arc::new(Scripted {
                        id: "first",
                        hits: Arc::clone(&first_hits),
                        answers: std::sync::Mutex::new(
                            [
                                Err(ProviderError::status(429, "slow")
                                    .with_retry_after(Duration::from_secs(120))),
                                Ok(raw_ok()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    }),
                ),
            ),
            (
                "second".to_owned(),
                built_member(
                    "second",
                    Arc::new(Scripted {
                        id: "second",
                        hits: Arc::clone(&second_hits),
                        answers: std::sync::Mutex::new(
                            [Ok(raw_ok()), Ok(raw_ok()), Ok(raw_ok())]
                                .into_iter()
                                .collect(),
                        ),
                    }),
                ),
            ),
        ]);
        let book = cooldowns();
        let (snapshot, table) = super::open_with_table(
            &built,
            route(&["first", "second"]),
            FailoverPolicy::default(),
            book,
        )
        .expect("snapshot");

        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("failover");
        assert_eq!(routed.upstream_id, "second");
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("cooling");
        assert_eq!(routed.upstream_id, "second");
        assert_eq!(first_hits.load(Ordering::SeqCst), 1);

        table.publish();
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("republished");
        assert_eq!(routed.upstream_id, "second");
        assert_eq!(first_hits.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(121)).await;
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("expired");
        assert_eq!(routed.upstream_id, "first");
        assert_eq!(first_hits.load(Ordering::SeqCst), 2);
        assert_eq!(second_hits.load(Ordering::SeqCst), 3);
    }

    struct Observing {
        host: hofund_plugin_api::HostContext,
        hits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for Observing {
        fn id(&self) -> &str {
            "observed"
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let observation = self.host.observe().observation().with_remaining(0);
            self.host
                .observe()
                .observe(observation)
                .map_err(crate::adapt::provider_error)?;
            Ok(raw_ok())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_observation_cools_only_its_bound_member() {
        let book = cooldowns();
        let root = tempfile::tempdir().expect("root");
        let host = hofund_plugin_host::runtime_host_with_observe(
            hofund_plugin_api::PluginId::from_static("observed"),
            "",
            root.path(),
            hofund_plugin_host::HttpLimits::default(),
            super::observe_backend("observed", Arc::clone(&book)),
        )
        .expect("host");
        let observed_hits = Arc::new(AtomicUsize::new(0));
        let backup_hits = Arc::new(AtomicUsize::new(0));
        let built = BTreeMap::from([
            (
                "observed".to_owned(),
                built_member(
                    "observed",
                    Arc::new(Observing {
                        host,
                        hits: Arc::clone(&observed_hits),
                    }),
                ),
            ),
            (
                "backup".to_owned(),
                built_member(
                    "backup",
                    Arc::new(Scripted {
                        id: "backup",
                        hits: Arc::clone(&backup_hits),
                        answers: std::sync::Mutex::new([Ok(raw_ok())].into_iter().collect()),
                    }),
                ),
            ),
        ]);
        let snapshot = open(
            &built,
            route(&["observed", "backup"]),
            FailoverPolicy::default(),
            Arc::clone(&book),
        )
        .expect("snapshot");

        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("observed");
        assert_eq!(routed.upstream_id, "observed");
        assert!(book.cooling(&super::member_id("observed")).is_some());
        assert!(book.cooling(&super::member_id("backup")).is_none());
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("backup");
        assert_eq!(routed.upstream_id, "backup");
        assert_eq!(observed_hits.load(Ordering::SeqCst), 1);
        assert_eq!(backup_hits.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    // An unsupported count does not contact upstream or clear a member's rest.
    #[tokio::test(start_paused = true)]
    async fn an_unsupported_count_does_not_clear_a_rate_limit_rest() {
        let first_hits = Arc::new(AtomicUsize::new(0));
        let second_hits = Arc::new(AtomicUsize::new(0));
        let built = BTreeMap::from([
            (
                "first".to_owned(),
                built_member(
                    "first",
                    Arc::new(Scripted {
                        id: "first",
                        hits: Arc::clone(&first_hits),
                        answers: std::sync::Mutex::new(
                            [
                                Err(ProviderError::status(429, "slow")
                                    .with_retry_after(Duration::from_secs(120))),
                                Ok(raw_ok()),
                            ]
                            .into_iter()
                            .collect(),
                        ),
                    }),
                ),
            ),
            (
                "second".to_owned(),
                built_member(
                    "second",
                    Arc::new(Scripted {
                        id: "second",
                        hits: Arc::clone(&second_hits),
                        answers: std::sync::Mutex::new(
                            [Ok(raw_ok()), Ok(raw_ok())].into_iter().collect(),
                        ),
                    }),
                ),
            ),
        ]);
        let (snapshot, _table) = super::open_with_table(
            &built,
            route(&["first", "second"]),
            FailoverPolicy::default(),
            cooldowns(),
        )
        .expect("snapshot");
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("failover");
        assert_eq!(routed.upstream_id, "second");
        // One count_tokens request, 1 second later; nothing reaches upstream.
        tokio::time::advance(Duration::from_secs(1)).await;
        let _ = snapshot
            .load()
            .count(&Request::new("demo"), "anthropic-messages")
            .await;
        let routed = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await
            .expect("second dispatch");
        assert_eq!(
            routed.upstream_id, "second",
            "count Unsupported cleared the Retry-After rest of `first`"
        );
    }

    struct Counts {
        answer: std::sync::Mutex<Option<Result<hofund_provider::CountOutcome, ProviderError>>>,
    }

    #[async_trait]
    impl Provider for Counts {
        fn id(&self) -> &str {
            "count"
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            Ok(raw_ok())
        }

        async fn count_input(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<hofund_provider::CountOutcome, ProviderError> {
            self.answer
                .lock()
                .expect("count answer")
                .take()
                .expect("one count")
        }
    }

    async fn run_count(
        answer: Result<hofund_provider::CountOutcome, ProviderError>,
        book: Arc<Cooldowns>,
    ) {
        let built = BTreeMap::from([(
            "count".to_owned(),
            built_member(
                "count",
                Arc::new(Counts {
                    answer: std::sync::Mutex::new(Some(answer)),
                }),
            ),
        )]);
        let snapshot =
            open(&built, route(&["count"]), FailoverPolicy::default(), book).expect("snapshot");
        let _ = snapshot
            .load()
            .count(&Request::new("demo"), "openai-chat")
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_counted_count_does_not_clear_a_rate_limit_rest() {
        let book = cooldowns();
        let id = super::member_id("count");
        let _ = book.record_failure(
            &id,
            &hofund_plugin_api::PluginError::status_error(429, "slow")
                .with_retry_after(Duration::from_secs(120)),
        );
        let before = book.cooling(&id);

        run_count(
            Ok(hofund_provider::CountOutcome::Counted(4)),
            Arc::clone(&book),
        )
        .await;

        assert_eq!(book.cooling(&id), before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_error_count_does_not_cool_a_ready_member() {
        let book = cooldowns();

        run_count(
            Err(ProviderError::status(503, "unavailable")),
            Arc::clone(&book),
        )
        .await;

        assert_eq!(book.cooling(&super::member_id("count")), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_count_cools_the_member() {
        let book = cooldowns();

        run_count(Err(ProviderError::status(429, "slow")), Arc::clone(&book)).await;

        let cooling = book.cooling(&super::member_id("count")).expect("rate rest");
        assert_eq!(cooling.cause(), hofund_router::CooldownCause::RateLimit);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_and_transport_count_failures_cool_the_member() {
        for error in [
            ProviderError::connect("refused"),
            ProviderError::transport("reset"),
        ] {
            let book = cooldowns();

            run_count(Err(error), Arc::clone(&book)).await;

            let cooling = book
                .cooling(&super::member_id("count"))
                .expect("transport rest");
            assert_eq!(cooling.cause(), hofund_router::CooldownCause::Failure);
        }
    }

    struct Forwards {
        answer: std::sync::Mutex<Option<Result<RawAnswer, ProviderError>>>,
        seen: std::sync::Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl Provider for Forwards {
        fn id(&self) -> &str {
            "forward"
        }

        async fn execute(
            &self,
            _request: Request,
            _inbound_protocol: &str,
        ) -> Result<Completion, ProviderError> {
            Ok(raw_ok())
        }

        async fn forward(&self, path: &str, body: RawBody) -> Result<RawAnswer, ProviderError> {
            self.seen.lock().expect("seen").push((
                path.to_owned(),
                String::from_utf8(body.bytes().to_vec()).expect("utf8"),
            ));
            self.answer
                .lock()
                .expect("forward answer")
                .take()
                .expect("one forward")
        }
    }

    async fn run_forward(
        answer: Result<RawAnswer, ProviderError>,
        book: Arc<Cooldowns>,
    ) -> (
        Result<hofund_router::Forwarded, hofund_router::RouterError>,
        Vec<(String, String)>,
    ) {
        let provider = Arc::new(Forwards {
            answer: std::sync::Mutex::new(Some(answer)),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let built = BTreeMap::from([(
            "forward".to_owned(),
            built_member("forward", Arc::clone(&provider) as Arc<dyn Provider>),
        )]);
        let snapshot =
            open(&built, route(&["forward"]), FailoverPolicy::default(), book).expect("snapshot");
        let call = hofund_plugin_api::Forward::new("demo", "/embeddings", |model| {
            Ok(RawBody::new("application/json", format!("m={model}")))
        })
        .expect("call");
        let result = snapshot.load().forward(&call).await;
        let seen = provider.seen.lock().expect("seen").clone();
        (result, seen)
    }

    #[tokio::test(start_paused = true)]
    async fn a_forward_reaches_the_provider_and_does_not_clear_a_rest() {
        let book = cooldowns();
        let id = super::member_id("forward");
        let _ = book.record_failure(
            &id,
            &hofund_plugin_api::PluginError::status_error(429, "slow")
                .with_retry_after(Duration::from_secs(120)),
        );
        let before = book.cooling(&id);

        let (result, seen) = run_forward(
            Ok(RawAnswer::new(200, None, b"{}".to_vec())),
            Arc::clone(&book),
        )
        .await;

        let forwarded = result.expect("forwarded");
        assert_eq!(forwarded.upstream_id, "forward");
        assert_eq!(forwarded.answer.body().as_ref(), b"{}");
        assert_eq!(seen, [("/embeddings".to_owned(), "m=demo".to_owned())]);
        assert_eq!(book.cooling(&id), before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forward_that_is_not_served_or_fails_on_the_server_does_not_cool() {
        for error in [
            ProviderError::unsupported("no embeddings"),
            ProviderError::status(404, "not found"),
            ProviderError::status(503, "unavailable"),
        ] {
            let book = cooldowns();

            let (result, _) = run_forward(Err(error), Arc::clone(&book)).await;

            assert!(result.is_err());
            assert_eq!(book.cooling(&super::member_id("forward")), None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_or_unreachable_forward_cools_the_member() {
        for (error, cause) in [
            (
                ProviderError::status(429, "slow"),
                hofund_router::CooldownCause::RateLimit,
            ),
            (
                ProviderError::connect("refused"),
                hofund_router::CooldownCause::Failure,
            ),
        ] {
            let book = cooldowns();

            let (result, _) = run_forward(Err(error), Arc::clone(&book)).await;

            assert!(result.is_err());
            let cooling = book.cooling(&super::member_id("forward")).expect("rest");
            assert_eq!(cooling.cause(), cause);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_plugin_member_error_does_not_cool_the_member() {
        let hits = Arc::new(AtomicUsize::new(0));
        let built = BTreeMap::from([(
            "cancelled".to_owned(),
            built_member(
                "cancelled",
                Arc::new(Scripted {
                    id: "cancelled",
                    hits,
                    answers: std::sync::Mutex::new(
                        [Err(ProviderError::cancelled("client left"))]
                            .into_iter()
                            .collect(),
                    ),
                }),
            ),
        )]);
        let book = cooldowns();
        let snapshot = open(
            &built,
            route(&["cancelled"]),
            FailoverPolicy::default(),
            Arc::clone(&book),
        )
        .expect("snapshot");

        let result = snapshot
            .load()
            .dispatch(&Request::new("demo"), "openai-chat")
            .await;

        assert!(result.is_err());
        assert_eq!(book.cooling(&super::member_id("cancelled")), None);
    }

    #[tokio::test]
    async fn weighted_rounds_survive_republished_snapshots() {
        let built = BTreeMap::from([
            (
                "first".to_owned(),
                built_member(
                    "first",
                    Arc::new(Scripted {
                        id: "first",
                        hits: Arc::new(AtomicUsize::new(0)),
                        answers: std::sync::Mutex::new(
                            [Ok(raw_ok()), Ok(raw_ok()), Ok(raw_ok())]
                                .into_iter()
                                .collect(),
                        ),
                    }),
                ),
            ),
            (
                "second".to_owned(),
                built_member(
                    "second",
                    Arc::new(Scripted {
                        id: "second",
                        hits: Arc::new(AtomicUsize::new(0)),
                        answers: std::sync::Mutex::new([Ok(raw_ok())].into_iter().collect()),
                    }),
                ),
            ),
        ]);
        let mut routes = route(&["first", "second"]);
        for hop in routes.get_mut("demo").expect("route") {
            hop.routing = "weight".to_owned();
            hop.weight = if hop.provider == "first" { 3 } else { 1 };
        }
        let (snapshot, table) =
            super::open_with_table(&built, routes, FailoverPolicy::default(), cooldowns())
                .expect("snapshot");

        for expected in ["first", "first", "second", "first"] {
            table.publish();
            let routed = snapshot
                .load()
                .dispatch(&Request::new("demo"), "openai-chat")
                .await
                .expect("dispatch");
            assert_eq!(routed.upstream_id, expected);
        }
    }
}
