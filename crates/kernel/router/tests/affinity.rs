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
    let route = Route::new(hops).with_routing("smart").with_stays(stays);
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
            kind: QuotaKind::default(),
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

struct Recording {
    id: MemberId,
    models: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Member for Recording {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "second"
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(
        &self,
        request: hofund_router::Request,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.models.lock().expect("models").push(request.model);
        answer()
    }
}

#[tokio::test]
async fn c3d_strict_dispatch_asks_the_answerer_first_even_while_its_model_rests() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let first = fake("first", &first_hits, vec![answer()]);
    let models = Arc::new(Mutex::new(Vec::new()));
    let second_id = MemberId::from_preset(
        PresetId::try_from("second").expect("preset"),
        MemberKey::from_static("row"),
    );
    let second: Arc<dyn Member> = Arc::new(Recording {
        id: second_id.clone(),
        models: Arc::clone(&models),
    });
    let cooldowns = Arc::new(hofund_router::Cooldowns::new(
        hofund_router::CooldownPolicy::default(),
    ));
    cooldowns
        .record_failure_for_model(
            &second_id,
            "model-a",
            &PluginError::status_error(
                429,
                "All credentials for the requested model are cooling down",
            )
            .with_retry_after(std::time::Duration::from_secs(120)),
        )
        .expect("model rest");
    assert!(cooldowns.cooling_for_model(&second_id, "model-a").is_some());
    assert!(cooldowns.cooling_for_model(&second_id, "model-b").is_none());
    let state = AffinityState::in_memory();
    state.answered(
        "demo|session",
        "preset:second:row",
        "model-a",
        1,
        CacheUsage {
            input: Some(3000),
            read: 0,
            write: 3000,
            write_hour: 0,
        },
    );
    let mut resting = Hop::new(Arc::clone(&second));
    resting.wire_model = Some("model-a".to_owned());
    let mut ready = Hop::new(second);
    ready.wire_model = Some("model-b".to_owned());
    let route = Route::new(vec![Hop::new(first), ready, resting])
        .with_routing("order")
        .with_stays(Stays::Strict);
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
    .with_affinity(state)
    .with_cooldowns(cooldowns);
    router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&conversation()),
            None,
        )
        .await
        .expect("strict answerer answers");
    assert_eq!(models.lock().expect("models").as_slice(), ["model-a"]);
    assert_eq!(first_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d25c_dispatch_records_a_stand_in_and_returns_after_recovery() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let first = fake("first", &first_hits, vec![answer()]);
    let second = fake("second", &second_hits, vec![answer()]);
    let cooldowns = Arc::new(hofund_router::Cooldowns::new(
        hofund_router::CooldownPolicy::default(),
    ));
    cooldowns
        .record_failure_for_model(
            first.id(),
            "demo",
            &PluginError::status_error(429, "out")
                .with_retry_after(std::time::Duration::from_secs(120)),
        )
        .expect("rest");
    let state = AffinityState::in_memory();
    let route = Route::new(vec![Hop::new(Arc::clone(&first)), Hop::new(second)])
        .with_routing("smart")
        .with_stays(Stays::Auto);
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
    .with_affinity(state.clone())
    .with_cooldowns(cooldowns.clone());

    let routed = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&conversation()),
            None,
        )
        .await
        .expect("stand-in answers");
    assert_eq!(routed.upstream_id, "second");
    assert!(state.stick("demo|session").expect("stick").stood_in);

    cooldowns.record_success(first.id());
    let next = Conversation::new("demo|session", 2, false);
    let routed = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&next),
            None,
        )
        .await
        .expect("recovered member answers");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn c6g_a_failed_opening_no_longer_counts_as_held() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let first = fake(
        "first",
        &first_hits,
        vec![Err(PluginError::status_error(500, "down")), answer()],
    );
    let second = fake(
        "second",
        &second_hits,
        vec![Err(PluginError::status_error(500, "down")), answer()],
    );
    let router = router(first, second, AffinityState::in_memory(), Stays::Session);
    let opening = Conversation::new("demo|a", 1, false);
    assert!(
        router
            .dispatch_with_context(
                &hofund_router::Request::new("demo"),
                "test",
                Some(&opening),
                None
            )
            .await
            .is_err()
    );
    let next = Conversation::new("demo|b", 1, false);
    let routed = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&next),
            None,
        )
        .await
        .expect("answers");
    assert_eq!(routed.upstream_id, "first");
}

#[tokio::test]
async fn c6h_a_broken_stream_no_longer_counts_as_held() {
    use futures_util::StreamExt;
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let broken = Ok(Completion::Stream(Box::pin(futures_util::stream::iter(
        vec![
            Ok(hofund_ir::StreamEvent::TextDelta(hofund_ir::TextDelta {
                index: 0,
                text: "hi".to_owned(),
                extensions: hofund_ir::Extensions::new(),
            })),
            Err(PluginError::status_error(500, "cut")),
        ],
    ))));
    let first = fake("first", &first_hits, vec![broken, answer()]);
    let second = fake("second", &second_hits, vec![answer()]);
    let router = router(first, second, AffinityState::in_memory(), Stays::Session);
    let opening = Conversation::new("demo|a", 1, false);
    let routed = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&opening),
            None,
        )
        .await
        .expect("stream starts");
    let Completion::Stream(mut stream) = routed.completion else {
        panic!("stream");
    };
    while stream.next().await.is_some() {}
    drop(stream);
    let next = Conversation::new("demo|b", 1, false);
    let routed = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&next),
            None,
        )
        .await
        .expect("answers");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(second_hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d26_5_dispatch_reserve_takes_over_and_gives_back() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let reserve_hits = Arc::new(AtomicUsize::new(0));
    let failure = || Err(PluginError::status_error(429, "out"));
    let first = fake("first", &first_hits, vec![failure(), answer()]);
    let second = fake("second", &second_hits, vec![failure(), answer()]);
    let reserve = fake("reserve", &reserve_hits, vec![answer()]);
    let at_once = [
        ("preset:first:row", 0),
        ("preset:second:row", 1),
        ("preset:reserve:row", 2),
    ]
    .into_iter()
    .map(|(id, rank)| {
        (
            id.to_owned(),
            hofund_router::at_once::AtOnceMember {
                group: "provider".to_owned(),
                limit: 2,
                rank,
            },
        )
    })
    .collect();
    let route = Route::new(vec![Hop::new(first), Hop::new(second), Hop::new(reserve)])
        .with_at_once(at_once)
        .with_routing("order")
        .with_stays(Stays::Session);
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
    .with_affinity(AffinityState::in_memory());
    assert_eq!(
        router.route_reserves("demo"),
        [("preset:reserve:row".to_owned(), 2)]
    );

    let first_answer = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&conversation()),
            None,
        )
        .await
        .expect("reserve answers after kept members fail");
    assert_eq!(first_answer.upstream_id, "reserve");
    assert_eq!(first_answer.affinity_reason.as_deref(), Some("first"));
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    assert_eq!(reserve_hits.load(Ordering::SeqCst), 1);

    let next = Conversation::new("demo|session", 2, false);
    let second_answer = router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&next),
            None,
        )
        .await
        .expect("kept member takes the conversation back");
    assert_eq!(second_answer.upstream_id, "first");
    assert_eq!(second_answer.affinity_reason.as_deref(), Some("reserve"));
    assert_eq!(first_hits.load(Ordering::SeqCst), 2);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    assert_eq!(reserve_hits.load(Ordering::SeqCst), 1);
}
