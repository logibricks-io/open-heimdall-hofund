//! Week-low allowance routing through affinity on a new subagent turn (R-4).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use hofund_control_proto::{Quota, QuotaKind, QuotaSpan};
use hofund_ir::{ContentPart, Message, Request, Response, Role, Usage};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::affinity::{AffinityState, Conversation, Stays};
use hofund_router::quota::{QuotaStore, WindowInfo, quota_time};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router};

const H: u64 = 3600;
const SESSION: &str = "demo|session";

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

fn preset(
    route_id: &'static str,
    order: &Arc<Mutex<Vec<&'static str>>>,
    answers: Vec<Result<Completion, PluginError>>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::try_from(route_id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id,
        order: Arc::clone(order),
        answers: Mutex::new(answers.into_iter().collect()),
    })
}

fn ok_with_cache() -> Result<Completion, PluginError> {
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

fn busy() -> Result<Completion, PluginError> {
    Err(PluginError::status_error(429, "busy"))
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

fn quotas(now: SystemTime) -> Arc<QuotaStore> {
    let week_reset = now + Duration::from_secs(168 * H);
    let short_reset = now + Duration::from_secs(4 * H);
    let week_only = window("Weekly", 92.0, week_reset, Duration::from_secs(168 * H));
    let short_cap = window("5h", 92.0, short_reset, Duration::from_secs(5 * H));
    let roomy_week = window("Weekly", 40.0, week_reset, Duration::from_secs(168 * H));

    let store = Arc::new(QuotaStore::in_memory());
    record_member(&store, "preset:week:row", vec![week_only], now);
    record_member(&store, "preset:cap:row", vec![short_cap, roomy_week], now);
    store
}

fn router(hops: Vec<Hop>, quotas: Arc<QuotaStore>, affinity: AffinityState) -> Router {
    let route = Route::new(hops)
        .with_routing("smart")
        .with_stays(Stays::Session);
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
    .with_quotas(quotas)
    .with_affinity(affinity)
}

async fn bind_lead(router: &Router, affinity: &AffinityState, lead: &'static str) {
    let parent = Conversation::new(SESSION, 1, false);
    router
        .dispatch_with_context(&Request::new("demo"), "test", Some(&parent), None)
        .await
        .expect("parent turn binds the lead");
    assert_eq!(
        affinity.stick(SESSION).expect("stick").member,
        format!("preset:{lead}:row")
    );
}

async fn subagent_upstream(router: &Router) -> String {
    let subagent = Conversation::new(format!("{SESSION}|agent:tool"), 1, false);
    router
        .dispatch_with_context(&Request::new("demo"), "test", Some(&subagent), None)
        .await
        .expect("subagent turn")
        .upstream_id
        .to_owned()
}

#[tokio::test]
async fn r4_72_week_low_lead_keeps_subagent_on_week_only_low_member() {
    let now = SystemTime::now();
    let quotas = quotas(now);
    let order = Arc::new(Mutex::new(Vec::new()));
    let cap = preset(
        "cap",
        &order,
        vec![busy(), ok_with_cache(), ok_with_cache()],
    );
    let week = preset("week", &order, vec![ok_with_cache(), ok_with_cache()]);
    let affinity = AffinityState::in_memory();
    let router = router(
        vec![Hop::new(cap), Hop::new(week)],
        quotas,
        affinity.clone(),
    );

    bind_lead(&router, &affinity, "week").await;
    assert_eq!(subagent_upstream(&router).await, "week");
}

#[tokio::test]
async fn r4_72_short_cap_lead_moves_subagent_off_low_member_without_week_low() {
    let now = SystemTime::now();
    let quotas = quotas(now);
    let order = Arc::new(Mutex::new(Vec::new()));
    let week = preset(
        "week",
        &order,
        vec![busy(), ok_with_cache(), ok_with_cache()],
    );
    let cap = preset("cap", &order, vec![ok_with_cache(), ok_with_cache()]);
    let affinity = AffinityState::in_memory();
    let router = router(
        vec![Hop::new(week), Hop::new(cap)],
        quotas,
        affinity.clone(),
    );

    bind_lead(&router, &affinity, "cap").await;
    assert_eq!(subagent_upstream(&router).await, "week");
}
