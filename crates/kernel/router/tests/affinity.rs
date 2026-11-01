//! Dispatch integration for conversation affinity.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};
use hofund_ir::{ContentPart, Message, Response, Role, Usage};
use hofund_plugin_api::{AccountId, MemberId, MemberKey, PluginError, PluginId, PresetId};
use hofund_router::affinity::{AffinityState, CacheUsage, Conversation, Stays};
use hofund_router::quota::{QuotaStore, WindowInfo};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router};

struct Fake {
    id: MemberId,
    route_id: &'static str,
    hits: Arc<AtomicUsize>,
    answers: Mutex<VecDeque<Result<Completion, PluginError>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(
        &self,
        _request: hofund_router::Request,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

fn fake(
    id: &'static str,
    hits: &Arc<AtomicUsize>,
    answers: Vec<Result<Completion, PluginError>>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id: id,
        hits: Arc::clone(hits),
        answers: Mutex::new(answers.into_iter().collect()),
    })
}

fn fake_account(
    id: &'static str,
    hits: &Arc<AtomicUsize>,
    answers: Vec<Result<Completion, PluginError>>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: MemberId::from_parts(
            PluginId::try_from(id).expect("plugin"),
            AccountId::try_from("acct").expect("account"),
        ),
        route_id: id,
        hits: Arc::clone(hits),
        answers: Mutex::new(answers.into_iter().collect()),
    })
}

fn answer() -> Result<Completion, PluginError> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    let mut response = Response::new(message);
    response.usage = Some(Usage {
        input_tokens: Some(3000),
        cache_write_tokens: Some(3000),
        ..Usage::default()
    });
    Ok(Completion::Unary(Box::new(response)))
}

fn router(
    first: Arc<dyn Member>,
    second: Arc<dyn Member>,
    state: AffinityState,
    stays: Stays,
) -> Router {
    let hops = vec![Hop::new(first), Hop::new(second)];
    let route = Route::new(hops).with_routing("rotate").with_stays(stays);
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
    .with_affinity(state)
}

fn conversation() -> Conversation {
    Conversation::new("demo|session", 1, false)
}

#[tokio::test]
async fn c2d_dispatch_records_usage_and_keeps_warm_answerer() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let first = fake("first", &first_hits, vec![answer(), answer()]);
    let second = fake("second", &second_hits, vec![answer()]);
    let router = router(first, second, AffinityState::in_memory(), Stays::Auto);

    for _ in 0..2 {
        let routed = router
            .dispatch_with_context(
                &hofund_router::Request::new("demo"),
                "test",
                Some(&conversation()),
                None,
            )
            .await
            .expect("dispatch");
        assert_eq!(routed.upstream_id, "first");
    }
    assert_eq!(first_hits.load(Ordering::SeqCst), 2);
    assert_eq!(second_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn c3c_strict_dispatch_passes_on_error_without_failover() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let first = fake("first", &first_hits, vec![answer()]);
    let second = fake(
        "second",
        &second_hits,
        vec![Err(PluginError::status_error(429, "out"))],
    );
    let state = AffinityState::in_memory();
    state.answered(
        "demo|session",
        "preset:second:row",
        "demo",
        1,
        CacheUsage {
            input: Some(3000),
            read: 0,
            write: 3000,
            write_hour: 0,
        },
    );
    let router = router(first, second, state, Stays::Strict);
    let error = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&conversation()),
            None,
        )
        .await;
    let Err(error) = error else {
        panic!("strict failure should reach the caller");
    };
    assert!(error.to_string().contains("second"));
    assert_eq!(first_hits.load(Ordering::SeqCst), 0);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn c2f_nearly_spent_stuck_account_is_bypassed() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let first = fake("first", &first_hits, vec![answer()]);
    let second = fake_account("second", &second_hits, vec![answer()]);
    let state = AffinityState::in_memory();
    state.answered(
        "demo|session",
        "account:second:acct",
        "demo",
        1,
        CacheUsage {
            input: Some(3000),
            read: 0,
            write: 3000,
            write_hour: 0,
        },
    );
    let quotas = Arc::new(QuotaStore::in_memory());
    quotas.record_success(
        Quota {
            provider: "second".to_owned(),
            kind: QuotaKind::Subscription,
            user: "acct".to_owned(),
            ..Quota::default()
        },
        vec![WindowInfo::new(QuotaSpan {
            name: "week".to_owned(),
            used: 99.0,
            remaining: 1.0,
            ..QuotaSpan::default()
        })],
        std::time::SystemTime::now(),
    );
    let router = router(first, second, state, Stays::Auto).with_quotas(quotas);
    let routed = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&conversation()),
            None,
        )
        .await
        .expect("ready member answers");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 0);
}
