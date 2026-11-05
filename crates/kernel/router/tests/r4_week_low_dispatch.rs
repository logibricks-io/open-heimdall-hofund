//! Router wiring for week-low allowance routing (R-4).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::affinity::Conversation;
use hofund_router::credits_mode::CreditsModeStore;
use hofund_router::quota::{QuotaStore, WindowInfo, quota_time};
use hofund_router::strategy::{Measurements, Needs, Seat};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router};

const H: u64 = 3600;

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

struct QuotaMeasurements {
    quotas: Arc<QuotaStore>,
}

impl Measurements for QuotaMeasurements {
    fn weigh(
        &self,
        seats: &mut [Seat],
        wire_models: &[String],
        _usage_model: &str,
        _needs: Needs,
        now: SystemTime,
    ) {
        let allowances = self
            .quotas
            .allowances(now)
            .into_iter()
            .map(|allowance| (allowance.member.clone(), allowance))
            .collect::<BTreeMap<_, _>>();
        for (seat, model) in seats.iter_mut().zip(wire_models) {
            let Some(allowance) = allowances.get(&seat.key) else {
                continue;
            };
            seat.known = !allowance.windows.is_empty();
            seat.used = allowance.used_for_routing(model, now);
            seat.week_low = allowance.week_low(model, seat.used, now);
            seat.soon = allowance.routing_renewals(model, now);
            seat.pace = allowance.pace(model, now);
            hofund_router::credits_mode::apply_to_seat(
                seat,
                allowance,
                model,
                now,
                CreditsModeStore::in_memory().mode_for_member(&seat.key),
            );
        }
    }
}

fn preset(route_id: &'static str, order: &Arc<Mutex<Vec<&'static str>>>) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::try_from(route_id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id,
        order: Arc::clone(order),
        answers: Mutex::new(VecDeque::from(vec![ok()])),
    })
}

fn ok() -> Result<Completion, PluginError> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Ok(Completion::Unary(Box::new(Response::new(message))))
}

fn window(name: &str, used: f64, reset: SystemTime, span: Duration) -> WindowInfo {
    WindowInfo::new(QuotaSpan {
        name: name.to_owned(),
        used,
        remaining: (100.0 - used).max(0.0),
        resets_at: Some(quota_time(reset)),
        ..QuotaSpan::default()
    })
    .with_window_span(span)
}

fn record_member(quotas: &QuotaStore, member: &str, windows: Vec<WindowInfo>, now: SystemTime) {
    quotas.record_success(
        Quota {
            member: member.to_owned(),
            provider: "quota".to_owned(),
            name: member.to_owned(),
            kind: QuotaKind::default(),
            user: member.to_owned(),
            ..Quota::default()
        },
        windows,
        now,
    );
}

fn router(
    cap: Arc<dyn Member>,
    week: Arc<dyn Member>,
    quotas: Arc<QuotaStore>,
    routing: &str,
) -> Router {
    let hops = vec![Hop::new(cap), Hop::new(week)];
    let route = Route::new(hops).with_routing(routing);
    let measurements = Arc::new(QuotaMeasurements {
        quotas: Arc::clone(&quotas),
    });
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
    .with_quotas(quotas)
    .with_measurements(measurements)
}

fn conversation() -> Conversation {
    Conversation::new("demo|session", 1, false)
}

#[tokio::test]
async fn r4_72_router_wires_week_low_from_allowance_windows() {
    let now = SystemTime::now();
    let week_reset = now + Duration::from_secs(168 * H);
    let short_reset = now + Duration::from_secs(4 * H);
    let week_only = window("Weekly", 92.0, week_reset, Duration::from_secs(168 * H));
    let short_cap = window("5h", 92.0, short_reset, Duration::from_secs(5 * H));
    let roomy_week = window("Weekly", 40.0, week_reset, Duration::from_secs(168 * H));

    let quotas = Arc::new(QuotaStore::in_memory());
    record_member(&quotas, "preset:week:row", vec![week_only.clone()], now);
    record_member(&quotas, "preset:cap:row", vec![short_cap, roomy_week], now);

    let order = Arc::new(Mutex::new(Vec::new()));
    let cap = preset("cap", &order);
    let week = preset("week", &order);
    let router = router(cap, week, quotas, "smart");

    let candidates = router.test_planned_candidates("demo", &conversation());
    let cap_candidate = candidates
        .iter()
        .find(|candidate| candidate.member == "preset:cap:row")
        .expect("cap candidate");
    let week_candidate = candidates
        .iter()
        .find(|candidate| candidate.member == "preset:week:row")
        .expect("week candidate");
    assert!(
        cap_candidate.low_allowance,
        "short-cap member should be in the low band"
    );
    assert!(!cap_candidate.week_low);
    assert!(week_candidate.week_low);
    assert_ne!(cap_candidate.week_low, week_candidate.week_low);
    assert!(
        cap_candidate.low_allowance && !cap_candidate.week_low,
        "short-cap low should not count as week-low"
    );
    assert!(
        !(week_candidate.low_allowance && !week_candidate.week_low),
        "week-only low should stay eligible for new turns"
    );

    let routed = router
        .dispatch_with_context(&Request::new("demo"), "test", Some(&conversation()), None)
        .await
        .expect("dispatch");
    assert_eq!(
        routed.upstream_id, "week",
        "smart routing should prefer the week-low seat over the short-cap-low seat"
    );
    assert_eq!(*order.lock().expect("order"), ["week"]);
}
