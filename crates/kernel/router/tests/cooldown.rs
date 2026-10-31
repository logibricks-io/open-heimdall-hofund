//! Cooldown-aware member ordering across dispatch, count, and snapshots.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{
    CountOutcome, MemberId, MemberKey, PluginError, PluginId, PresetId, QuotaObservation,
};
use hofund_router::strategy::{Strategy, StrategyState};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router,
};

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<&'static str>>>,
    behavior: Behavior,
}

enum Behavior {
    Fail,
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
