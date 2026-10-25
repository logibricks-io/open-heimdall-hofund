//! Adapts each configured provider into a [`Member`], then into a tower service.
//!
//! The service is wrapped in the stage-1 layers: a kernel deadline, panic
//! containment, and the router's span (the span is entered by the router,
//! which knows the attempt). A panic marks the member failed and publishes a
//! snapshot that leaves it out. There is no cooldown timer; that is pull
//! request K.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{FutureExt, StreamExt};
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
    for (model, planned) in routes {
        let mut specs = Vec::new();
        for hop in planned {
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
        drop(hops);
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
}
