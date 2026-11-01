//! Routing groups, nested units, sinking, and weighted affinity accounting.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Response, Role};
use hofund_plugin_api::{
    MemberId, MemberKey, ModelSpec, PluginError, PluginId, PresetId, QuotaObservation,
};
use hofund_router::affinity::{AffinityState, CacheUsage, Conversation, Stays};
use hofund_router::strategy::{Registry, Strategy, StrategyState};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router, Unit,
};

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<&'static str>>>,
    fail: bool,
    models: Option<Arc<Mutex<Vec<ModelSpec>>>>,
    model_reads: Arc<AtomicUsize>,
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

    async fn execute(
        &self,
        _request: hofund_router::Request,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.seen.lock().expect("seen").push(self.id);
        if self.fail {
            return Err(PluginError::status_error(500, "down"));
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }

    async fn costs_nothing(&self, model: &str) -> Option<bool> {
        let models = self.models.as_ref()?;
        self.model_reads.fetch_add(1, Ordering::SeqCst);
        models
            .lock()
            .expect("models")
            .iter()
            .find(|listed| listed.id().eq_ignore_ascii_case(model))
            .map(ModelSpec::costs_nothing)
    }
}

fn member_id(id: &str) -> MemberId {
    MemberId::from_preset(
        PresetId::try_from(id).expect("preset"),
        MemberKey::from_static("row"),
    )
}

fn unit(key: &str, routing: &str, weight: u64, sink: bool) -> Unit {
    Unit {
        key: key.to_owned(),
        pool: key.to_owned(),
        routing: routing.to_owned(),
        weight,
        sink,
        fallback: false,
    }
}

fn direct(id: &str, weight: u64) -> Vec<Unit> {
    vec![unit(&format!("provider/{id}"), "smart", weight, false)]
}

fn hop(
    id: &'static str,
    units: Vec<Unit>,
    weight: u64,
    seen: &Arc<Mutex<Vec<&'static str>>>,
) -> Hop {
    hop_failing(id, units, weight, seen, false)
}

fn hop_failing(
    id: &'static str,
    units: Vec<Unit>,
    weight: u64,
    seen: &Arc<Mutex<Vec<&'static str>>>,
    fail: bool,
) -> Hop {
    let mut hop = Hop::new(Arc::new(Fake {
        id,
        member_id: member_id(id),
        seen: Arc::clone(seen),
        fail,
        models: None,
        model_reads: Arc::new(AtomicUsize::new(0)),
    }));
    hop.units = units;
    hop.weight = weight;
    hop
}

fn router(route: Route) -> Router {
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
}

async fn dispatch(router: &Router) {
    router
        .dispatch(&hofund_router::Request::new("demo"), "test")
        .await
        .expect("dispatch");
}

#[tokio::test]
async fn d1_direct_provider_request_uses_the_whole_fallback_group() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut primary = hop_failing("plan", direct("plan", 1), 1, &seen, true);
    primary.wire_model = Some("m".to_owned());
    let group = || {
        let mut unit = unit("group/backup", "order", 1, false);
        unit.fallback = true;
        unit
    };
    let fallback = |id| {
        let mut units = direct(id, 1);
        units[0].fallback = true;
        units
    };
    let mut second = hop_failing("b", vec![group(), fallback("b")[0].clone()], 1, &seen, true);
    second.wire_model = Some("x".to_owned());
    let mut third = hop("a", vec![group(), fallback("a")[0].clone()], 1, &seen);
    third.wire_model = Some("x".to_owned());
    let router = router(Route::new(vec![primary, second, third]).with_routing("order"));

    router
        .dispatch(&hofund_router::Request::new("plan/m"), "test")
        .await
        .expect("fallback group dispatch");
    assert_eq!(*seen.lock().expect("seen"), ["plan", "b", "a"]);
}

#[tokio::test]
async fn d2_manual_member_order_is_used_for_pick_and_failover() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let route = Route::new(vec![
        hop_failing("b", direct("b", 1), 1, &seen, true),
        hop("a", direct("a", 1), 1, &seen),
    ])
    .with_routing("order");
    dispatch(&router(route)).await;
    assert_eq!(*seen.lock().expect("seen"), ["b", "a"]);
}

#[tokio::test]
async fn d3_rate_limit_sinks_until_manual_clear_but_quota_does_not_sink() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let route = Route::new(vec![
        hop("a", direct("a", 1), 1, &seen),
        hop("b", direct("b", 1), 1, &seen),
        hop("c", direct("c", 1), 1, &seen),
    ])
    .with_routing("order")
    .with_sink(true);
    let router = router(route).with_cooldowns(Arc::clone(&cooldowns));

    cooldowns.record_failure_for_model(
        &member_id("a"),
        "demo",
        &PluginError::status_error(429, "slow"),
    );
    *now.lock().expect("clock") += Duration::from_secs(61);
    cooldowns.record_success_for_model(&member_id("a"), "demo");
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b"]);

    cooldowns.record_failure_for_model(
        &member_id("b"),
        "demo",
        &PluginError::status_error(429, "slow"),
    );
    *now.lock().expect("clock") += Duration::from_secs(61);
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b", "c"]);

    cooldowns.record_success(&member_id("a"));
    cooldowns.record_success(&member_id("b"));
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b", "c", "a"]);

    cooldowns.record_failure_for_model(&member_id("a"), "demo", &PluginError::quota("used up"));
    *now.lock().expect("clock") += Duration::from_secs(15 * 61);
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b", "c", "a", "a"]);

    cooldowns.observe(
        &member_id("a"),
        &QuotaObservation::from_stored(PluginId::from_static("a"))
            .with_remaining(0)
            .with_limit(10),
    );
    *now.lock().expect("clock") += Duration::from_secs(15 * 61);
    cooldowns.record_failure_for_model(
        &member_id("a"),
        "demo",
        &PluginError::status_error(429, "slow"),
    );
    *now.lock().expect("clock") += Duration::from_secs(61);
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b", "c", "a", "a", "a"]);
}

#[tokio::test]
async fn d3_nested_group_sinks_inside_its_own_order() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inner = || unit("group/inner", "order", 1, true);
    let route = Route::new(vec![
        hop("a", vec![inner(), direct("a", 1)[0].clone()], 1, &seen),
        hop("b", vec![inner(), direct("b", 1)[0].clone()], 1, &seen),
        hop("x", direct("x", 1), 1, &seen),
    ])
    .with_routing("order");
    let router = router(route).with_cooldowns(Arc::clone(&cooldowns));
    cooldowns.record_failure_for_model(
        &member_id("a"),
        "demo",
        &PluginError::status_error(429, "slow"),
    );
    *now.lock().expect("clock") += Duration::from_secs(61);

    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b"]);
}

#[tokio::test]
async fn d3_rotate_group_does_not_reorder_by_sink() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inner = || unit("group/inner", "rotate", 1, true);
    let route = Route::new(vec![
        hop("a", vec![inner(), direct("a", 1)[0].clone()], 1, &seen),
        hop("b", vec![inner(), direct("b", 1)[0].clone()], 1, &seen),
    ])
    .with_routing("order");
    let router = router(route).with_cooldowns(Arc::clone(&cooldowns));
    cooldowns.record_failure_for_model(
        &member_id("a"),
        "demo",
        &PluginError::status_error(429, "slow"),
    );
    *now.lock().expect("clock") += Duration::from_secs(61);

    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["a"]);
}

#[tokio::test]
async fn d3_auth_failure_does_not_sink() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let route = Route::new(vec![
        hop("a", direct("a", 1), 1, &seen),
        hop("b", direct("b", 1), 1, &seen),
    ])
    .with_routing("order")
    .with_sink(true);
    let router = router(route).with_cooldowns(Arc::clone(&cooldowns));
    cooldowns.record_failure(
        &member_id("a"),
        &PluginError::status_error(401, "unauthorized"),
    );
    assert!(
        cooldowns
            .cooling_for_model(&member_id("a"), "demo")
            .is_some()
    );
    assert_eq!(cooldowns.sunk_at(&member_id("a"), "demo"), None);
    *now.lock().expect("clock") += Duration::from_secs(61);

    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["a"]);
}

#[tokio::test]
async fn d4_unserved_routing_uses_listed_order_when_registration_is_removed() {
    let registry = Registry::default();
    let strategies = Arc::new(StrategyState::new(registry.clone()));
    let registration = registry.register("rotate", Strategy::new(|_, _, _| vec![1, 0]).exact());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let route = Route::new(vec![
        hop("a", direct("a", 1), 1, &seen),
        hop("b", direct("b", 1), 1, &seen),
    ])
    .with_routing("rotate");
    let router = router(route).with_strategies(Arc::clone(&strategies));
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b"]);

    drop(registration);
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b", "a"]);
}

#[tokio::test]
async fn d5_subscription_is_one_weighted_unit_with_inner_account_weights() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let subscription = || unit("subscription/plan", "weight", 1, false);
    let route = Route::new(vec![
        hop("p", direct("p", 3), 1, &seen),
        hop("a", vec![subscription()], 3, &seen),
        hop("b", vec![subscription()], 1, &seen),
    ])
    .with_routing("weight");
    let router = router(route);
    for _ in 0..8 {
        dispatch(&router).await;
    }
    assert_eq!(
        *seen.lock().expect("seen"),
        ["p", "p", "b", "p", "p", "p", "b", "p"]
    );
}

#[tokio::test]
async fn d5_nested_group_is_one_unit_and_keeps_its_own_routing() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inner = || unit("group/inner", "rotate", 1, false);
    let route = Route::new(vec![
        hop("p", direct("p", 3), 1, &seen),
        hop("a", vec![inner(), direct("a", 1)[0].clone()], 1, &seen),
        hop("b", vec![inner(), direct("b", 1)[0].clone()], 1, &seen),
    ])
    .with_routing("weight");
    let router = router(route);
    for _ in 0..8 {
        dispatch(&router).await;
    }
    assert_eq!(
        *seen.lock().expect("seen"),
        ["p", "p", "a", "p", "p", "p", "a", "p"]
    );
}

#[tokio::test]
async fn d5_affinity_moves_the_weighted_round_to_the_kept_member() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let route = Route::new(vec![
        hop("a", direct("a", 1), 1, &seen),
        hop("b", direct("b", 1), 1, &seen),
    ])
    .with_routing("weight")
    .with_stays(Stays::Session);
    let affinity = AffinityState::in_memory();
    affinity.answered(
        "demo|kept",
        &member_id("b").to_string(),
        "demo",
        1,
        CacheUsage {
            input: Some(3000),
            read: 0,
            write: 3000,
            write_hour: 0,
        },
    );
    let router = router(route).with_affinity(affinity);
    router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(&Conversation::new("demo|kept", 2, false)),
            None,
        )
        .await
        .expect("kept dispatch");
    dispatch(&router).await;
    assert_eq!(*seen.lock().expect("seen"), ["b", "a"]);
}

fn listed(free: bool, zero: bool) -> ModelSpec {
    let model = ModelSpec::new("demo", None).expect("model");
    let model = if zero {
        model.with_prices(0, 0, 0)
    } else {
        model.with_prices(1, 2, 0)
    };
    model.with_free(free)
}

fn free_hop(
    id: &'static str,
    models: &Arc<Mutex<Vec<ModelSpec>>>,
    reads: &Arc<AtomicUsize>,
    seen: &Arc<Mutex<Vec<&'static str>>>,
) -> Hop {
    let mut hop = Hop::new(Arc::new(Fake {
        id,
        member_id: member_id(id),
        seen: Arc::clone(seen),
        fail: false,
        models: Some(Arc::clone(models)),
        model_reads: Arc::clone(reads),
    }));
    hop.units = direct(id, 1);
    hop
}

#[tokio::test]
async fn d6_subscription_members_use_model_list_free_and_zero_price_flags() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let paid_models = Arc::new(Mutex::new(vec![listed(false, false)]));
    let zero_models = Arc::new(Mutex::new(vec![listed(false, true)]));
    let free_models = Arc::new(Mutex::new(vec![listed(true, false)]));
    let paid_reads = Arc::new(AtomicUsize::new(0));
    let zero_reads = Arc::new(AtomicUsize::new(0));
    let free_reads = Arc::new(AtomicUsize::new(0));
    let paid = free_hop("paid", &paid_models, &paid_reads, &seen);
    let zero = free_hop("zero", &zero_models, &zero_reads, &seen);
    let free = free_hop("free", &free_models, &free_reads, &seen);

    dispatch(&router(
        Route::new(vec![paid.clone(), zero.clone(), free.clone()]).with_routing("order"),
    ))
    .await;
    assert_eq!(paid_reads.load(Ordering::SeqCst), 0);
    assert_eq!(zero_reads.load(Ordering::SeqCst), 0);
    assert_eq!(free_reads.load(Ordering::SeqCst), 0);

    dispatch(&router(
        Route::new(vec![paid, zero, free]).with_routing("free"),
    ))
    .await;
    assert_eq!(*seen.lock().expect("seen"), ["paid", "zero"]);
    assert_eq!(paid_reads.load(Ordering::SeqCst), 1);
    assert_eq!(zero_reads.load(Ordering::SeqCst), 1);
    assert_eq!(free_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn d6_model_list_refresh_reorders_free_route_per_request() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let paid_models = Arc::new(Mutex::new(vec![listed(false, false)]));
    let changed_models = Arc::new(Mutex::new(vec![listed(false, false)]));
    let paid_reads = Arc::new(AtomicUsize::new(0));
    let changed_reads = Arc::new(AtomicUsize::new(0));
    let route = Route::new(vec![
        free_hop("paid", &paid_models, &paid_reads, &seen),
        free_hop("changed", &changed_models, &changed_reads, &seen),
    ])
    .with_routing("free");
    let router = router(route);

    dispatch(&router).await;
    *changed_models.lock().expect("models") = vec![listed(true, false)];
    dispatch(&router).await;
    *changed_models.lock().expect("models") = vec![listed(false, true)];
    dispatch(&router).await;

    assert_eq!(*seen.lock().expect("seen"), ["paid", "changed", "changed"]);
    assert_eq!(paid_reads.load(Ordering::SeqCst), 3);
    assert_eq!(changed_reads.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn d6_nested_group_free_flags_refresh_per_request() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let paid_models = Arc::new(Mutex::new(vec![listed(false, false)]));
    let changed_models = Arc::new(Mutex::new(vec![listed(false, false)]));
    let paid_reads = Arc::new(AtomicUsize::new(0));
    let changed_reads = Arc::new(AtomicUsize::new(0));
    let inner = || unit("group/free", "free", 1, false);
    let mut paid = free_hop("paid", &paid_models, &paid_reads, &seen);
    paid.units = vec![inner(), direct("paid", 1)[0].clone()];
    let mut changed = free_hop("changed", &changed_models, &changed_reads, &seen);
    changed.units = vec![inner(), direct("changed", 1)[0].clone()];
    let route = Route::new(vec![paid, changed]).with_routing("order");
    let router = router(route);

    dispatch(&router).await;
    *changed_models.lock().expect("models") = vec![listed(true, false)];
    dispatch(&router).await;
    *changed_models.lock().expect("models") = vec![listed(false, true)];
    dispatch(&router).await;

    assert_eq!(*seen.lock().expect("seen"), ["paid", "changed", "changed"]);
    assert_eq!(paid_reads.load(Ordering::SeqCst), 3);
    assert_eq!(changed_reads.load(Ordering::SeqCst), 3);
}
