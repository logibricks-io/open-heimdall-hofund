//! Cooldown-aware member ordering across dispatch, count, and snapshots.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role, estimate_input_tokens};
use hofund_plugin_api::{
    CountOutcome, MemberId, MemberKey, PluginError, PluginId, PresetId, QuotaObservation,
    QuotaWindow,
};
use hofund_router::strategy::{Strategy, StrategyState};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router, RouterError,
    WaitHook,
};

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<&'static str>>>,
    behavior: Behavior,
}

enum Behavior {
    Fail,
    RefuseCredit,
    RefuseQuota,
    Unary,
    MissingCount,
    Count(u64),
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.seen.lock().expect("seen").push(self.id);
        match self.behavior {
            Behavior::Fail => Err(PluginError::status_error(503, "down")),
            Behavior::RefuseCredit => Err(PluginError::credit_status(402, "allowance exhausted")
                .with_reset_after(Duration::from_secs(6 * 60 * 60))),
            Behavior::RefuseQuota => Err(PluginError::quota_status(429, "allowance exhausted")
                .with_reset_after(Duration::from_secs(6 * 60 * 60))),
            _ => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(ContentPart::text("ok"));
                Ok(Completion::Unary(Box::new(Response::new(message))))
            }
        }
    }

    async fn count_input(
        &self,
        _request: Request,
        _inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.seen.lock().expect("seen").push(self.id);
        match self.behavior {
            Behavior::Fail => Err(PluginError::status_error(503, "down")),
            Behavior::Count(tokens) => Ok(CountOutcome::Counted(tokens)),
            _ => Ok(CountOutcome::Unsupported),
        }
    }
}

fn fixture() -> (Arc<Cooldowns>, Arc<Mutex<Vec<&'static str>>>) {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let book = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    (book, Arc::new(Mutex::new(Vec::new())))
}

fn fake(
    id: &'static str,
    seen: &Arc<Mutex<Vec<&'static str>>>,
    behavior: Behavior,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id,
        member_id: MemberId::from_preset(PresetId::from_static(id), MemberKey::from_static("row")),
        seen: Arc::clone(seen),
        behavior,
    })
}

fn member_id(id: &'static str) -> MemberId {
    MemberId::from_preset(PresetId::from_static(id), MemberKey::from_static("row"))
}

fn router(members: Vec<Arc<dyn Member>>, book: &Arc<Cooldowns>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), members);
    Router::new(routes, FailoverPolicy::default()).with_cooldowns(Arc::clone(book))
}

fn cool(book: &Cooldowns, id: &'static str, seconds: u64) {
    let observed = QuotaObservation::from_stored(PluginId::from_static("router"))
        .with_remaining(0)
        .with_retry_after(Duration::from_secs(seconds));
    let _ = book.observe(&member_id(id), &observed);
}

#[tokio::test]
async fn dispatch_tries_ready_members_before_one_cooling_probe() {
    let (book, seen) = fixture();
    cool(&book, "first", 300);
    cool(&book, "third", 60);
    let router = router(
        vec![
            fake("first", &seen, Behavior::Unary),
            fake("second", &seen, Behavior::Fail),
            fake("third", &seen, Behavior::Unary),
        ],
        &book,
    );
    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "third");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second", "third"]);
}

#[tokio::test]
async fn count_uses_the_same_cooldown_ordering() {
    let (book, seen) = fixture();
    cool(&book, "first", 300);
    cool(&book, "third", 60);
    let router = router(
        vec![
            fake("first", &seen, Behavior::Count(1)),
            fake("second", &seen, Behavior::MissingCount),
            fake("third", &seen, Behavior::Count(3)),
        ],
        &book,
    );
    let count = router
        .count(&Request::new("demo"), "inbound")
        .await
        .expect("count");
    assert_eq!(count.tokens, 3);
    assert_eq!(count.upstream_id, "third");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second", "third"]);
}

#[tokio::test]
async fn a_shared_book_survives_fresh_members_and_routers() {
    let (book, seen) = fixture();
    let before = router(vec![fake("first", &seen, Behavior::Unary)], &book);
    before
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("before");
    assert!(Arc::ptr_eq(before.cooldowns().expect("book"), &book));
    book.record_failure(&member_id("first"), &PluginError::connect("down"));
    seen.lock().expect("seen").clear();

    let after = router(
        vec![
            fake("first", &seen, Behavior::Unary),
            fake("second", &seen, Behavior::Unary),
        ],
        &book,
    );
    let routed = after
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("after");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);
}

#[tokio::test]
async fn ready_members_keep_order_before_the_cooling_probe() {
    let (book, seen) = fixture();
    cool(&book, "first", 300);
    cool(&book, "third", 60);
    let router = router(
        vec![
            fake("first", &seen, Behavior::Unary),
            fake("second", &seen, Behavior::Fail),
            fake("third", &seen, Behavior::Unary),
            fake("fourth", &seen, Behavior::Unary),
        ],
        &book,
    );
    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "fourth");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second", "fourth"]);
}

#[tokio::test]
async fn equal_cooldowns_keep_the_first_configured_probe() {
    let (book, seen) = fixture();
    for id in ["first", "second", "third"] {
        cool(&book, id, 60);
    }
    let router = router(
        vec![
            fake("first", &seen, Behavior::Unary),
            fake("second", &seen, Behavior::Unary),
            fake("third", &seen, Behavior::Unary),
        ],
        &book,
    );
    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["first"]);
}

#[tokio::test]
async fn a_lone_cooling_member_is_still_tried() {
    let (book, seen) = fixture();
    cool(&book, "first", 60);
    let router = router(vec![fake("first", &seen, Behavior::Unary)], &book);
    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["first"]);
}

#[tokio::test]
async fn all_cooling_members_reduce_to_the_soonest_probe() {
    let (book, seen) = fixture();
    cool(&book, "first", 300);
    cool(&book, "second", 60);
    cool(&book, "third", 120);
    let router = router(
        vec![
            fake("first", &seen, Behavior::Unary),
            fake("second", &seen, Behavior::Unary),
            fake("third", &seen, Behavior::Unary),
        ],
        &book,
    );
    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);
}

#[tokio::test]
async fn b5e_model_rests_order_each_member_model_pair_independently() {
    let (book, seen) = fixture();
    for (id, seconds) in [("first", 300), ("third", 60)] {
        let error = PluginError::status_error(
            429,
            "All credentials for model model-a are cooling down via provider upstream",
        )
        .with_retry_after(Duration::from_secs(seconds));
        book.record_failure_for_model(&member_id(id), "model-a", &error)
            .expect("model rest");
    }
    let members = vec![
        fake("first", &seen, Behavior::Unary),
        fake("second", &seen, Behavior::Fail),
        fake("third", &seen, Behavior::Unary),
    ];
    let mut routes = BTreeMap::new();
    routes.insert("model-a".to_owned(), members.clone());
    routes.insert("model-b".to_owned(), members);
    let router = Router::new(routes, FailoverPolicy::default()).with_cooldowns(Arc::clone(&book));

    let routed = router
        .dispatch(&Request::new("model-a"), "inbound")
        .await
        .expect("model-a");
    assert_eq!(routed.upstream_id, "third");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second", "third"]);

    seen.lock().expect("seen").clear();
    let routed = router
        .dispatch(&Request::new("model-b"), "inbound")
        .await
        .expect("model-b");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["first"]);
}

#[tokio::test]
async fn a8_ready_members_follow_strategy_then_one_soonest_probe() {
    let (book, seen) = fixture();
    cool(&book, "third", 60);
    let mut hops = vec![
        Hop::new(fake("first", &seen, Behavior::Fail)),
        Hop::new(fake("second", &seen, Behavior::Unary)),
        Hop::new(fake("third", &seen, Behavior::Unary)),
    ];
    hops[0].weight = 3;
    hops[1].weight = 1;
    hops[2].weight = 4;
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops).with_routing("weight"))]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(book);

    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["first", "second"]);
}

#[tokio::test]
async fn a8_route_strategy_sees_weight_free_and_rest_of_each_member() {
    let (book, seen) = fixture();
    cool(&book, "second", 60);
    let state = Arc::new(StrategyState::with_builtins());
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let _probe = state.registry().register(
        "probe",
        Strategy::new(move |pool, seats, _| {
            sink.lock().expect("captured").push((
                pool.to_owned(),
                seats
                    .iter()
                    .map(|seat| (seat.weight, seat.free, seat.resting))
                    .collect::<Vec<_>>(),
            ));
            vec![2, 0, 1]
        }),
    );
    let mut hops = vec![
        Hop::new(fake("first", &seen, Behavior::Unary)),
        Hop::new(fake("second", &seen, Behavior::Unary)),
        Hop::new(fake("third", &seen, Behavior::Unary)),
    ];
    hops[0].weight = 3;
    hops[2].free = true;
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops).with_routing("probe"))]),
        FailoverPolicy::default(),
    )
    .with_strategies(Arc::clone(&state))
    .with_cooldowns(book);

    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "third");
    assert_eq!(
        captured.lock().expect("captured").as_slice(),
        [(
            "demo".to_owned(),
            vec![(3, false, false), (1, false, true), (1, true, false)]
        )]
    );
}

#[tokio::test]
async fn b5e_routing_reads_the_rest_of_the_hop_wire_model() {
    let (book, seen) = fixture();
    let error = PluginError::status_error(
        429,
        "All credentials for model wire-a are cooling down via provider upstream",
    )
    .with_retry_after(Duration::from_secs(300));
    book.record_failure_for_model(&member_id("first"), "wire-a", &error)
        .expect("model rest");
    let mut first = Hop::new(fake("first", &seen, Behavior::Unary));
    first.wire_model = Some("wire-a".to_owned());
    let hops = vec![first, Hop::new(fake("second", &seen, Behavior::Unary))];
    let router = Router::from_routes(
        BTreeMap::from([("alias".to_owned(), Route::new(hops))]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(book);

    let routed = router
        .dispatch(&Request::new("alias"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);
}

#[tokio::test]
async fn d251_3_renewal_lets_a_lone_member_ask_once() {
    for credit in [false, true] {
        let (book, seen) = fixture();
        let id = member_id("first");
        book.configure_credit_window(&id);
        let reset = SystemTime::now() + Duration::from_secs(3 * 60 * 60);
        let reading =
            |used| QuotaWindow::new("window", used, Some(10), Some(reset)).expect("window");
        let refusal = || {
            if credit {
                PluginError::credit_status(402, "allowance exhausted")
            } else {
                PluginError::quota_status(429, "allowance exhausted")
            }
            .with_reset_after(Duration::from_secs(6 * 60 * 60))
        };
        book.observe_credit_window(&id, &[reading(10)]);
        book.record_failure(&id, &refusal())
            .expect("vendor-timed rest");
        let behavior = if credit {
            Behavior::RefuseCredit
        } else {
            Behavior::RefuseQuota
        };
        let router = router(vec![fake("first", &seen, behavior)], &book);
        let status = if credit { 402 } else { 429 };

        let held = router.dispatch(&Request::new("demo"), "inbound").await;
        assert!(seen.lock().expect("seen").is_empty());
        let Err(RouterError::Upstream { error, .. }) = held else {
            panic!("expected the stored refusal");
        };
        assert_eq!(error.status(), Some(status));
        assert_eq!(error.message(), "allowance exhausted");
        assert_eq!(error.is_credit(), credit);

        book.observe_credit_window(&id, &[reading(8)]);
        let probed = router.dispatch(&Request::new("demo"), "inbound").await;
        assert_eq!(seen.lock().expect("seen").as_slice(), ["first"]);
        let Err(RouterError::Upstream { error, .. }) = probed else {
            panic!("expected the upstream refusal");
        };
        assert_eq!(error.status(), Some(status));
        book.record_failure(&id, &refusal())
            .expect("same-time refusal");

        seen.lock().expect("seen").clear();
        book.observe_credit_window(&id, &[reading(10)]);
        book.observe_credit_window(&id, &[reading(8)]);
        let held_again = router.dispatch(&Request::new("demo"), "inbound").await;
        assert!(seen.lock().expect("seen").is_empty());
        let Err(RouterError::Upstream { error, .. }) = held_again else {
            panic!("expected the held refusal");
        };
        assert_eq!(error.status(), Some(status));
        assert_eq!(error.is_credit(), credit);
    }
}

#[tokio::test]
async fn d251_7_ready_sibling_answers_before_the_renewal_probe() {
    let (book, seen) = fixture();
    let id = member_id("first");
    book.configure_credit_window(&id);
    let reset = SystemTime::now() + Duration::from_secs(3 * 60 * 60);
    let reading = |used| QuotaWindow::new("window", used, Some(10), Some(reset)).expect("window");
    let refusal = PluginError::quota_status(429, "allowance exhausted")
        .with_reset_after(Duration::from_secs(6 * 60 * 60));
    book.observe_credit_window(&id, &[reading(10)]);
    book.record_failure(&id, &refusal)
        .expect("vendor-timed rest");
    book.observe_credit_window(&id, &[reading(8)]);
    assert!(book.cooling(&id).expect("renewed rest").renewal_probe());
    let router = router(
        vec![
            fake("first", &seen, Behavior::RefuseQuota),
            fake("second", &seen, Behavior::Unary),
        ],
        &book,
    );

    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("ready sibling");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);
}

#[tokio::test]
async fn d24_a_model_with_no_provider_rests_alone_and_its_seat_is_passed_over() {
    let (book, seen) = fixture();
    let error = PluginError::status_error(
        400,
        r#"{"error":{"message":"Model id : lab/demo-flash , has no provider supported"}}"#,
    );
    book.record_failure_for_model(&member_id("first"), "wire-a", &error)
        .expect("model rest");
    assert!(
        book.cooling(&member_id("first")).is_none(),
        "the member does not rest"
    );
    assert!(
        book.cooling_for_model(&member_id("first"), "wire-b")
            .is_none()
    );
    let mut first = Hop::new(fake("first", &seen, Behavior::Unary));
    first.wire_model = Some("wire-a".to_owned());
    let hops = vec![first, Hop::new(fake("second", &seen, Behavior::Unary))];
    let router = Router::from_routes(
        BTreeMap::from([("alias".to_owned(), Route::new(hops))]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(book);

    let routed = router
        .dispatch(&Request::new("alias"), "inbound")
        .await
        .expect("dispatch");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);
}

struct FrozenClock(Arc<Mutex<Instant>>);

impl WaitHook for FrozenClock {
    fn wait(
        &self,
        _duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::ready(()))
    }

    fn now(&self) -> Instant {
        *self.0.lock().expect("clock")
    }
}

struct FrozenBook {
    book: Arc<Cooldowns>,
    clock: Arc<Mutex<Instant>>,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

fn frozen_book() -> FrozenBook {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let book = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    FrozenBook {
        book,
        clock: now,
        seen: Arc::new(Mutex::new(Vec::new())),
    }
}

#[tokio::test]
async fn d260_vendor_timed_quota_rests_the_model_and_still_asks_the_rest() {
    let FrozenBook { book, clock, seen } = frozen_book();
    let notice = "Third-party quota reached. Resets in 3h22m19s.";
    let error = PluginError::quota_status(429, notice)
        .with_reset_after(Duration::from_secs(12_139))
        .with_model_scope();
    book.record_failure_for_model(&member_id("first"), "pool-a", &error)
        .expect("rest");
    let cooling = book
        .cooling_for_model(&member_id("first"), "pool-a")
        .expect("cooling");
    assert!(cooling.vendor_timed());
    assert!(
        book.cooling_for_model(&member_id("first"), "pool-b")
            .is_none()
    );

    let lone = Router::new(
        BTreeMap::from([(
            "pool-a".to_owned(),
            vec![fake("first", &seen, Behavior::Unary)],
        )]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(Arc::clone(&book));
    let refused = lone
        .dispatch_with_wait(
            &Request::new("pool-a"),
            "inbound",
            None,
            Some(Arc::new(FrozenClock(Arc::clone(&clock)))),
        )
        .await;
    assert!(seen.lock().expect("seen").is_empty());
    let Err(RouterError::Upstream { error, .. }) = refused else {
        panic!("expected the stored refusal");
    };
    assert_eq!(error.status(), Some(429));
    assert!(error.message().contains(notice));
    let written = Duration::from_secs(12_139);
    assert_eq!(error.retry_after(), Some(written));
    assert_eq!(error.reset_after(), Some(written));

    let members = vec![
        fake("first", &seen, Behavior::Unary),
        fake("second", &seen, Behavior::Unary),
    ];
    let router = Router::new(
        BTreeMap::from([
            ("pool-a".to_owned(), members.clone()),
            ("pool-b".to_owned(), members),
        ]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(Arc::clone(&book));

    seen.lock().expect("seen").clear();
    let routed = router
        .dispatch(&Request::new("pool-a"), "inbound")
        .await
        .expect("other account");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);

    seen.lock().expect("seen").clear();
    let routed = router
        .dispatch(&Request::new("pool-b"), "inbound")
        .await
        .expect("sibling model");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["first"]);
}

#[tokio::test]
async fn d260_a_guessed_quota_rest_is_still_probed() {
    let (book, seen) = fixture();
    let error = PluginError::quota_status(429, "Quota exceeded").with_model_scope();
    book.record_failure_for_model(&member_id("first"), "demo", &error)
        .expect("rest");
    let cooling = book
        .cooling_for_model(&member_id("first"), "demo")
        .expect("cooling");
    assert!(!cooling.vendor_timed());
    let router = router(vec![fake("first", &seen, Behavior::Unary)], &book);
    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("probe");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["first"]);
}

#[tokio::test]
async fn d260_count_skips_a_vendor_timed_quota_rest() {
    let (book, seen) = fixture();
    let error = PluginError::quota_status(429, "Third-party quota reached. Resets in 3h22m19s.")
        .with_reset_after(Duration::from_secs(12_139))
        .with_model_scope();
    book.record_failure_for_model(&member_id("first"), "demo", &error)
        .expect("rest");
    let request = Request::new("demo");

    let lone = router(vec![fake("first", &seen, Behavior::Count(9))], &book);
    let count = lone.count(&request, "inbound").await.expect("estimate");
    assert!(seen.lock().expect("seen").is_empty());
    assert!(count.estimated);
    assert_eq!(count.tokens, estimate_input_tokens(&request));
    assert!(count.upstream_id.is_empty());

    seen.lock().expect("seen").clear();
    let mixed = router(
        vec![
            fake("first", &seen, Behavior::Count(9)),
            fake("second", &seen, Behavior::MissingCount),
        ],
        &book,
    );
    let count = mixed
        .count(&request, "inbound")
        .await
        .expect("no count left");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);
    assert!(count.estimated);
    assert_eq!(count.tokens, estimate_input_tokens(&request));

    seen.lock().expect("seen").clear();
    let next = router(
        vec![
            fake("first", &seen, Behavior::Count(9)),
            fake("second", &seen, Behavior::Count(4)),
        ],
        &book,
    );
    let count = next.count(&request, "inbound").await.expect("next");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["second"]);
    assert!(!count.estimated);
    assert_eq!(count.tokens, 4);
    assert_eq!(count.upstream_id, "second");

    seen.lock().expect("seen").clear();
    let sibling = Router::new(
        BTreeMap::from([(
            "pool-b".to_owned(),
            vec![fake("first", &seen, Behavior::Count(7))],
        )]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(Arc::clone(&book));
    let count = sibling
        .count(&Request::new("pool-b"), "inbound")
        .await
        .expect("sibling");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["first"]);
    assert!(!count.estimated);
    assert_eq!(count.tokens, 7);
}
