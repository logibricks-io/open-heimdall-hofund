//! Direct-provider patience with fake members and paused time.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_ir::{Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router};

struct Fake {
    id: MemberId,
    hits: Arc<AtomicUsize>,
    answers: Mutex<VecDeque<Result<Completion, PluginError>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

fn answer() -> Completion {
    let mut message = Message::new(Role::Assistant);
    message.content.push(hofund_ir::ContentPart::text("ok"));
    Completion::Unary(Box::new(Response::new(message)))
}

fn make_router(
    answers: Vec<Result<Completion, PluginError>>,
    patience: Option<Duration>,
) -> (Router, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::from_static("primary"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(&hits),
        answers: Mutex::new(answers.into_iter().collect()),
    });
    let hop = Hop {
        member,
        wire_model: None,
        max_output_tokens: None,
        context_window: None,
        patience,
        weight: 1,
        free: false,
        cold_after: hofund_router::affinity::CACHE_COLD,
        nearly_spent: false,
        fallback: false,
        effort: String::new(),
        units: Vec::new(),
    };
    let router = Router::from_hops(
        BTreeMap::from([("demo".to_owned(), vec![hop])]),
        FailoverPolicy::default(),
    );
    (router, hits)
}

struct Sleep;

impl hofund_router::WaitHook for Sleep {
    fn wait(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> std::time::Instant {
        tokio::time::Instant::now().into_std()
    }
}

struct RecordingSleep(Arc<Mutex<Vec<Duration>>>);

impl hofund_router::WaitHook for RecordingSleep {
    fn wait(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.0.lock().expect("wait record").push(duration);
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> std::time::Instant {
        tokio::time::Instant::now().into_std()
    }
}

async fn dispatch_recording(
    router: &Router,
    waits: Arc<Mutex<Vec<Duration>>>,
) -> Result<hofund_router::Routed, hofund_router::RouterError> {
    router
        .dispatch_with_wait(
            &Request::new("primary/demo"),
            "test",
            None,
            Some(Arc::new(RecordingSleep(waits))),
        )
        .await
}

async fn dispatch(
    router: &Router,
    model: &str,
) -> Result<hofund_router::Routed, hofund_router::RouterError> {
    router
        .dispatch_with_wait(&Request::new(model), "test", None, Some(Arc::new(Sleep)))
        .await
}

#[tokio::test(start_paused = true)]
async fn b6a_single_provider_retries_busy_failures_with_backoff() {
    for status in [408, 500, 502, 503, 504, 529, 429] {
        let (router, hits) = make_router(
            vec![Err(PluginError::status_error(status, "busy")), Ok(answer())],
            Some(Duration::from_secs(60)),
        );
        let started = tokio::time::Instant::now();
        let routed = dispatch(&router, "primary/demo")
            .await
            .expect("busy failure is retried");
        assert_eq!(routed.upstream_id, "primary", "{status}");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "{status}");
        assert_eq!(started.elapsed(), Duration::from_secs(2), "{status}");
    }

    let (router, hits) = make_router(
        vec![
            Err(PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(5))),
            Ok(answer()),
        ],
        Some(Duration::from_secs(60)),
    );
    let started = tokio::time::Instant::now();
    dispatch(&router, "primary/demo")
        .await
        .expect("retry hint is honored");
    assert_eq!(started.elapsed(), Duration::from_secs(5));
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    // A route pool waits by its own patience (b11), not the provider's:
    // the provider's is off here and the pool still waits its default.
    let (router, hits) = make_router(
        vec![Err(PluginError::status_error(503, "busy")), Ok(answer())],
        None,
    );
    let started = tokio::time::Instant::now();
    dispatch(&router, "demo")
        .await
        .expect("the pool's patience waits");
    assert_eq!(started.elapsed(), Duration::from_secs(2));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let (router, hits) = make_router(
        vec![Err(PluginError::status_error(503, "busy")), Ok(answer())],
        None,
    );
    dispatch(&router, "primary/demo")
        .await
        .err()
        .expect("the provider's patience is off for a direct request");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn b6d_single_provider_does_not_retry_terminal_failures() {
    let cases = vec![
        PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(3600)),
        PluginError::quota_status(429, "plan exhausted"),
        PluginError::credit_status(429, "credit exhausted"),
        PluginError::status_error(401, "bad key"),
        PluginError::status_error(403, "forbidden"),
        PluginError::status_error(502, "response protection is unavailable"),
        PluginError::status_error(
            429,
            r#"{"error":{"code":"user_model_rate_limited","message":"slow"}}"#,
        ),
    ];
    for error in cases {
        let (router, hits) = make_router(vec![Err(error)], Some(Duration::from_secs(60)));
        let started = tokio::time::Instant::now();
        dispatch(&router, "primary/demo")
            .await
            .err()
            .expect("terminal failure");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }
}

#[tokio::test(start_paused = true)]
async fn b6g_pauses_double_to_thirty_seconds_and_stop_at_the_budget() {
    let answers = (0..20)
        .map(|index| Err(PluginError::status_error(503, format!("busy-{index}"))))
        .collect();
    let (router, hits) = make_router(answers, Some(Duration::from_secs(60)));
    let waits = Arc::new(Mutex::new(Vec::new()));
    let started = tokio::time::Instant::now();
    let error = dispatch_recording(&router, Arc::clone(&waits))
        .await
        .err()
        .expect("the budget is exhausted");

    assert_eq!(started.elapsed(), Duration::from_secs(60));
    assert_eq!(hits.load(Ordering::SeqCst), 6);
    assert_eq!(
        waits.lock().expect("wait record").as_slice(),
        [2, 4, 8, 16, 30].map(Duration::from_secs)
    );
    let hofund_router::RouterError::Upstream { error, .. } = error else {
        panic!("expected an upstream error");
    };
    assert_eq!(error.status(), Some(503));
    assert_eq!(error.message(), "busy-5");
}

#[tokio::test(start_paused = true)]
async fn b6h_patience_off_and_240_second_cap() {
    let (router, hits) = make_router(vec![Err(PluginError::status_error(503, "busy"))], None);
    let started = tokio::time::Instant::now();
    dispatch(&router, "primary/demo")
        .await
        .err()
        .expect("patience is off");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn b6i_twelve_round_backstop() {
    let answers = (0..20)
        .map(|_| Err(PluginError::status_error(503, "busy")))
        .collect();
    let (router, hits) = make_router(answers, Some(Duration::from_secs(240)));
    let waits = Arc::new(Mutex::new(Vec::new()));
    let started = tokio::time::Instant::now();
    dispatch_recording(&router, Arc::clone(&waits))
        .await
        .err()
        .expect("the 240-second budget is exhausted");

    assert_eq!(started.elapsed(), Duration::from_secs(240));
    assert_eq!(hits.load(Ordering::SeqCst), 12);
    assert_eq!(waits.lock().expect("wait record").len(), 11);
}
