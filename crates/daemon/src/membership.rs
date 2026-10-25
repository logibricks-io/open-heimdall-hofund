//! Adapts each configured provider into a [`Member`], then into a tower service.
//!
//! The service is wrapped in the stage-1 layers: a kernel deadline, panic
//! containment, and the router's span (the span is entered by the router,
//! which knows the attempt). A panic marks the member failed and publishes a
//! snapshot that leaves it out. The body deadline is idle time since the last
//! chunk. A stream that keeps sending is not cut off. There is no cooldown
//! timer; that is pull request K.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{FutureExt, Stream, StreamExt};
use hofund_plugin_api::{
    BodyStream, Completion, CountOutcome, MemberId, MemberKey, PluginError, PresetId, Request,
};
use hofund_provider::{
    Completion as ProviderCompletion, CountOutcome as ProviderCount, Provider, ProviderError,
    ProviderErrorKind,
};
use hofund_router::{FailoverPolicy, Hop, Member, Router, Snapshot};
use tower::Service;

use crate::DaemonError;

pub(crate) struct BuiltProvider {
    pub adapter: Arc<dyn Provider>,
    pub protocol: &'static str,
    pub timeout: Duration,
    /// Catalog or implementation id recorded on the member span.
    pub plugin_id: String,
    /// Preset id from configuration. Empty when the provider has no preset.
    pub preset_id: String,
}

struct Table {
    hops: Mutex<BTreeMap<String, Vec<HopSpec>>>,
    policy: FailoverPolicy,
    snapshot: Snapshot,
}

struct HopSpec {
    member: Arc<LiveMember>,
    wire_model: Option<String>,
    max_output_tokens: Option<u64>,
}

struct LiveMember {
    id: MemberId,
    route_id: String,
    plugin_id: String,
    preset_id: String,
    protocol: &'static str,
    failed: Arc<std::sync::atomic::AtomicBool>,
    stack: Guard<ProviderCall>,
}

#[derive(Clone)]
struct ProviderCall {
    provider: Arc<dyn Provider>,
}

enum Job {
    Execute(Request, String),
    Count(Request, String),
}

enum Out {
    Execute(Completion),
    Count(CountOutcome),
}

#[derive(Clone)]
struct Guard<S> {
    inner: S,
    timeout: Duration,
    failed: Arc<std::sync::atomic::AtomicBool>,
    reload: Weak<Table>,
}

pub(crate) fn open(
    built: &BTreeMap<String, BuiltProvider>,
    routes: BTreeMap<String, Vec<PlannedHop>>,
    policy: FailoverPolicy,
) -> Result<Snapshot, DaemonError> {
    let snapshot = Snapshot::new(Router::from_hops(BTreeMap::new(), policy.clone()));
    let table = Arc::new(Table {
        hops: Mutex::new(BTreeMap::new()),
        policy,
        snapshot,
    });
    let mut stored = BTreeMap::new();
    let mut seen_ids: BTreeMap<String, String> = BTreeMap::new();
    for (model, planned) in routes {
        let mut specs = Vec::new();
        for hop in planned {
            let label = member_id(&hop.provider).to_string();
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
            specs.push(HopSpec {
                member: Arc::new(LiveMember::new(
                    built,
                    &hop.provider,
                    Arc::downgrade(&table),
                )),
                wire_model: hop.wire_model,
                max_output_tokens: hop.max_output_tokens,
            });
        }
        stored.insert(model, specs);
    }
    *table.hops.lock().expect("membership") = stored;
    table.publish();
    let snapshot = table.snapshot.clone();
    let owner = Arc::clone(&table) as Arc<dyn std::any::Any + Send + Sync>;
    Ok(snapshot.hold(owner))
}

pub(crate) struct PlannedHop {
    pub provider: String,
    pub wire_model: Option<String>,
    pub max_output_tokens: Option<u64>,
}

impl Table {
    fn publish(&self) {
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
                    })
                    .collect();
                (model.clone(), list)
            })
            .collect();
        let policy = self.policy.clone();
        self.snapshot.store(Router::from_hops(routes, policy));
    }
}

impl LiveMember {
    fn new(built: &BuiltProvider, route_id: &str, reload: Weak<Table>) -> Self {
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stack = Guard {
            inner: ProviderCall {
                provider: Arc::clone(&built.adapter),
            },
            timeout: built.timeout,
            failed: Arc::clone(&failed),
            reload,
        };
        Self {
            id: member_id(route_id),
            route_id: route_id.to_owned(),
            plugin_id: built.plugin_id.clone(),
            preset_id: built.preset_id.clone(),
            protocol: built.protocol,
            failed,
            stack,
        }
    }

    fn failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Acquire)
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

    async fn execute(&self, request: Request, inbound: &str) -> Result<Completion, PluginError> {
        let mut stack = self.stack.clone();
        match stack.call(Job::Execute(request, inbound.to_owned())).await {
            Ok(Out::Execute(completion)) => Ok(completion),
            Ok(Out::Count(_)) => Err(PluginError::internal("member returned a count")),
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
        let timeout = self.timeout;
        let failed = Arc::clone(&self.failed);
        let reload = Weak::clone(&self.reload);
        Box::pin(async move {
            let fut = AssertUnwindSafe(inner.call(job));
            match tokio::time::timeout(timeout, fut.catch_unwind()).await {
                Ok(Ok(Ok(Out::Execute(completion)))) => Ok(Out::Execute(watch_completion(
                    completion,
                    timeout,
                    Arc::clone(&failed),
                    Weak::clone(&reload),
                ))),
                Ok(Ok(result)) => result,
                Ok(Err(_panic)) => {
                    failed.store(true, std::sync::atomic::Ordering::Release);
                    if let Some(table) = reload.upgrade() {
                        table.publish();
                    }
                    Err(PluginError::internal("member panicked"))
                }
                Err(_elapsed) => Err(PluginError::timeout("kernel deadline exceeded")),
            }
        })
    }
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
                Job::Execute(request, inbound) => {
                    let completion = provider
                        .execute(request, &inbound)
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
            }
        })
    }
}

fn watch_completion(
    completion: Completion,
    idle: Duration,
    failed: Arc<std::sync::atomic::AtomicBool>,
    reload: Weak<Table>,
) -> Completion {
    match completion {
        Completion::Stream(stream) => {
            Completion::Stream(Box::pin(Watched::new(stream, idle, failed, reload)))
        }
        Completion::Raw(body) => Completion::Raw(BodyStream::from_stream(Watched::new(
            body_to_stream(body),
            idle,
            failed,
            reload,
        ))),
        other => other,
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
    idle: Duration,
    idle_deadline: tokio::time::Instant,
    sleep: Pin<Box<tokio::time::Sleep>>,
    failed: Arc<std::sync::atomic::AtomicBool>,
    reload: Weak<Table>,
    done: bool,
}

impl<S> Watched<S> {
    fn new(
        inner: S,
        idle: Duration,
        failed: Arc<std::sync::atomic::AtomicBool>,
        reload: Weak<Table>,
    ) -> Self {
        let idle_deadline = tokio::time::Instant::now() + idle;
        Self {
            inner: Box::pin(inner),
            idle,
            idle_deadline,
            sleep: Box::pin(tokio::time::sleep_until(idle_deadline)),
            failed,
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
                this.failed
                    .store(true, std::sync::atomic::Ordering::Release);
                if let Some(table) = this.reload.upgrade() {
                    table.publish();
                }
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

fn into_plugin(error: ProviderError) -> PluginError {
    match error.kind {
        ProviderErrorKind::Connect => PluginError::connect(error.message),
        ProviderErrorKind::Timeout => PluginError::timeout(error.message),
        ProviderErrorKind::Status => {
            PluginError::status_error(error.status.unwrap_or(502), error.message)
        }
        ProviderErrorKind::Decode => PluginError::decode(error.message),
    }
}

fn member_id(provider_id: &str) -> MemberId {
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
    use hofund_plugin_api::Request;
    use hofund_provider::{Completion, Provider, ProviderError};
    use hofund_router::FailoverPolicy;

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
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "boom".to_owned(),
                wire_model: None,
                max_output_tokens: None,
            }],
        );
        let snapshot = open(&built, routes, FailoverPolicy::default()).expect("snapshot");
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
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "later".to_owned(),
                wire_model: None,
                max_output_tokens: None,
            }],
        );
        let snapshot = open(&built, routes, FailoverPolicy::default()).expect("snapshot");
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
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "quiet".to_owned(),
                wire_model: None,
                max_output_tokens: None,
            }],
        );
        let snapshot = open(&built, routes, FailoverPolicy::default()).expect("snapshot");
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
            },
        );
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![PlannedHop {
                provider: "pace".to_owned(),
                wire_model: None,
                max_output_tokens: None,
            }],
        );
        let snapshot = open(&built, routes, FailoverPolicy::default()).expect("snapshot");
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
                },
            );
        }
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![
                PlannedHop {
                    provider: "My_Provider".to_owned(),
                    wire_model: None,
                    max_output_tokens: None,
                },
                PlannedHop {
                    provider: "my-provider".to_owned(),
                    wire_model: None,
                    max_output_tokens: None,
                },
            ],
        );
        let error = match open(&built, routes, FailoverPolicy::default()) {
            Ok(_) => panic!("duplicate member ids should fail startup"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("my-provider"), "{error}");
    }
}
