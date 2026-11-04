//! Per-account credits modes (D-42) at dispatch time.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{AccountId, MemberId, PluginError, PluginId};
use hofund_router::credits_mode::{CreditsMode, CreditsModeStore, apply_to_seat};
use hofund_router::quota::{QuotaStore, WindowInfo};
use hofund_router::strategy::{Measurements, Needs, Seat};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router};

struct Fake {
    id: MemberId,
    route_id: &'static str,
    order: Arc<Mutex<Vec<&'static str>>>,
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
        self.order.lock().expect("order").push(self.route_id);
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

struct ModeMeasurements {
    quotas: Arc<QuotaStore>,
    modes: Arc<CreditsModeStore>,
}

impl Measurements for ModeMeasurements {
    fn weigh(
        &self,
        seats: &mut [Seat],
        wire_models: &[String],
        _usage_model: &str,
        _needs: Needs,
        now: SystemTime,
    ) {
        let allowances = self.quotas.allowances(now);
        for (seat, model) in seats.iter_mut().zip(wire_models) {
            let mode = self.modes.mode_for_member(&seat.key);
            seat.credits_mode = mode;
            if let Some(allowance) = allowances.iter().find(|entry| entry.member == seat.key) {
                apply_to_seat(seat, allowance, model, now, mode);
            }
        }
    }
}

fn account(plugin: &str, user: &str) -> MemberId {
    MemberId::from_parts(
        PluginId::try_from(plugin).expect("plugin"),
        AccountId::try_from(user).expect("account"),
    )
}

fn fake(
    plugin: &'static str,
    user: &'static str,
    order: &Arc<Mutex<Vec<&'static str>>>,
    answers: Vec<Result<Completion, PluginError>>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: account(plugin, user),
        route_id: user,
        order: Arc::clone(order),
        answers: Mutex::new(answers.into_iter().collect()),
    })
}

fn ok() -> Result<Completion, PluginError> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Ok(Completion::Unary(Box::new(Response::new(message))))
}

fn now() -> SystemTime {
    // Far enough ahead that recorded windows are not expired against wall clocks
    // used inside Router::reorder_last_on_credits.
    SystemTime::now()
}

fn on_credits_windows(used_blocking: f64) -> Vec<WindowInfo> {
    vec![
        WindowInfo::new(QuotaSpan {
            name: "day".to_owned(),
            used: used_blocking,
            remaining: (100.0 - used_blocking).max(0.0),
            resets_at: Some(hofund_router::quota::quota_time(
                now() + Duration::from_secs(3600),
            )),
            ..QuotaSpan::default()
        }),
        WindowInfo::new(QuotaSpan {
            name: "credits".to_owned(),
            used: 10.0,
            remaining: 90.0,
            resets_at: Some(hofund_router::quota::quota_time(
                now() + Duration::from_secs(3600),
            )),
            ..QuotaSpan::default()
        })
        .with_blocking(false),
    ]
}

fn record(quotas: &QuotaStore, provider: &str, user: &str, used_blocking: f64) {
    quotas.record_success(
        Quota {
            provider: provider.to_owned(),
            name: provider.to_owned(),
            kind: QuotaKind::default(),
            user: user.to_owned(),
            ..Quota::default()
        },
        on_credits_windows(used_blocking),
        now(),
    );
}

fn order_router(
    members: Vec<Arc<dyn Member>>,
    quotas: Arc<QuotaStore>,
    modes: Arc<CreditsModeStore>,
) -> Router {
    let hops = members.into_iter().map(Hop::new).collect();
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops))]),
        FailoverPolicy::default(),
    )
    .with_quotas(quotas)
    .with_credits_modes(modes)
}

fn smart_router(
    members: Vec<Arc<dyn Member>>,
    quotas: Arc<QuotaStore>,
    modes: Arc<CreditsModeStore>,
) -> Router {
    let hops = members.into_iter().map(Hop::new).collect::<Vec<_>>();
    let measurements: Arc<dyn Measurements> = Arc::new(ModeMeasurements {
        quotas: Arc::clone(&quotas),
        modes: Arc::clone(&modes),
    });
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops).with_routing("smart"))]),
        FailoverPolicy::default(),
    )
    .with_quotas(quotas)
    .with_credits_modes(modes)
    .with_measurements(measurements)
}

async fn dispatch(router: &Router) {
    router
        .dispatch(&Request::new("demo"), "test")
        .await
        .expect("dispatch");
}

#[tokio::test]
async fn d42_10_last_on_credits_is_asked_after_ready_peers() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first = fake("sub", "a", &order, vec![ok()]);
    let second = fake("sub", "b", &order, vec![ok()]);
    let quotas = Arc::new(QuotaStore::in_memory());
    record(&quotas, "sub", "a", 100.0);
    record(&quotas, "sub", "b", 10.0);
    let modes = Arc::new(CreditsModeStore::in_memory());
    modes.set("sub", "a", CreditsMode::Last);
    let router = order_router(vec![first, second], quotas, modes);
    dispatch(&router).await;
    assert_eq!(*order.lock().expect("order"), ["b"]);
}

#[tokio::test]
async fn d42_11_first_on_credits_is_asked_before_ready_peers() {
    let order = Arc::new(Mutex::new(Vec::new()));
    // List order puts ready peer first; First mode must promote the spent+credits seat.
    let first = fake("sub", "a", &order, vec![ok()]);
    let second = fake("sub", "b", &order, vec![ok()]);
    let quotas = Arc::new(QuotaStore::in_memory());
    record(&quotas, "sub", "a", 10.0);
    record(&quotas, "sub", "b", 100.0);
    let modes = Arc::new(CreditsModeStore::in_memory());
    modes.set("sub", "b", CreditsMode::First);
    let router = order_router(vec![first, second], quotas, modes);
    dispatch(&router).await;
    assert_eq!(*order.lock().expect("order"), ["b"]);
}

#[tokio::test]
async fn d42_12_off_does_not_demote_on_flat_order() {
    // Off is measurement/strategy unavailable, not a hop demotion. Flat order
    // still asks the listed first hop when it answers.
    let order = Arc::new(Mutex::new(Vec::new()));
    let first = fake("sub", "a", &order, vec![ok()]);
    let second = fake("sub", "b", &order, vec![ok()]);
    let quotas = Arc::new(QuotaStore::in_memory());
    record(&quotas, "sub", "a", 100.0);
    record(&quotas, "sub", "b", 10.0);
    let modes = Arc::new(CreditsModeStore::in_memory());
    modes.set("sub", "a", CreditsMode::Off);
    let router = order_router(vec![first, second], quotas, modes);
    dispatch(&router).await;
    assert_eq!(*order.lock().expect("order"), ["a"]);
}

#[tokio::test]
async fn d42_13_off_spent_ranks_after_ready_peer_on_smart() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first = fake("sub", "a", &order, vec![ok()]);
    let second = fake("sub", "b", &order, vec![ok()]);
    let quotas = Arc::new(QuotaStore::in_memory());
    record(&quotas, "sub", "a", 100.0);
    record(&quotas, "sub", "b", 10.0);
    let modes = Arc::new(CreditsModeStore::in_memory());
    modes.set("sub", "a", CreditsMode::Off);
    let router = smart_router(vec![first, second], quotas, modes);
    dispatch(&router).await;
    assert_eq!(*order.lock().expect("order"), ["b"]);
}
