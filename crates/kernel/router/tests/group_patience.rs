//! Route-pool patience with fake members and paused time.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_ir::{Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router, RouterError,
    WaitHook,
};

type Script = Mutex<VecDeque<Result<Completion, PluginError>>>;

struct Fake {
    id: MemberId,
    name: &'static str,
    calls: Arc<Mutex<Vec<&'static str>>>,
    answers: Script,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.name
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.calls.lock().expect("calls").push(self.name);
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

fn busy() -> Result<Completion, PluginError> {
    Err(PluginError::status_error(503, "busy"))
}

fn member_id(name: &'static str) -> MemberId {
    MemberId::from_preset(PresetId::from_static(name), MemberKey::from_static("row"))
}

struct Pool {
    router: Router,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

/// A pool `demo` asking members in the order given, each with its script.
fn pool(
    members: Vec<(&'static str, Vec<Result<Completion, PluginError>>)>,
    patience: Option<Duration>,
    cooldowns: Option<Arc<Cooldowns>>,
) -> Pool {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hops = members
        .into_iter()
        .map(|(name, script)| {
            let mut hop = Hop::new(Arc::new(Fake {
                id: member_id(name),
                name,
                calls: Arc::clone(&calls),
                answers: Mutex::new(script.into_iter().collect()),
            }));
            // A direct-provider budget a pool must not use.
            hop.patience = Some(Duration::from_secs(1));
            hop
        })
        .collect();
    let route = Route::new(hops)
        .with_routing("order")
        .with_patience(patience);
    let mut router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    );
    if let Some(cooldowns) = cooldowns {
        router = router.with_cooldowns(cooldowns);
    }
    Pool { router, calls }
}

struct Recording(Arc<Mutex<Vec<Duration>>>);

impl WaitHook for Recording {
    fn wait(
        &self,
        duration: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.0.lock().expect("waits").push(duration);
        Box::pin(tokio::time::sleep(duration))
    }

    fn now(&self) -> std::time::Instant {
        tokio::time::Instant::now().into_std()
    }
}

async fn ask(pool: &Pool) -> (Result<hofund_router::Routed, RouterError>, Vec<Duration>) {
    let waits = Arc::new(Mutex::new(Vec::new()));
    let result = pool
        .router
        .dispatch_with_wait(
            &Request::new("demo"),
            "test",
            None,
            Some(Arc::new(Recording(Arc::clone(&waits)))),
        )
        .await;
    let waits = waits.lock().expect("waits").clone();
    (result, waits)
}

fn calls(pool: &Pool) -> Vec<&'static str> {
    pool.calls.lock().expect("calls").clone()
}

#[tokio::test(start_paused = true)]
async fn b11_pool_waits_for_busy_members_with_its_default_60_seconds() {
    let pool = pool(
        vec![
            ("one", vec![busy(), busy(), busy()]),
            ("two", vec![busy(), busy(), Ok(answer())]),
        ],
        Some(Duration::from_secs(60)),
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert_eq!(result.expect("answered").upstream_id, "two");
    assert_eq!(
        waits,
        vec![Duration::from_secs(2), Duration::from_secs(4)],
        "the pool's patience, not the members' one-second provider budget"
    );
    assert_eq!(calls(&pool), ["one", "two", "one", "two", "one", "two"]);
}

#[tokio::test(start_paused = true)]
async fn b11_pool_patience_off_fails_at_once() {
    let pool = pool(
        vec![("one", vec![busy()]), ("two", vec![busy()])],
        None,
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert!(result.is_err());
    assert!(waits.is_empty());
    assert_eq!(calls(&pool), ["one", "two"]);
}

#[tokio::test(start_paused = true)]
async fn b11_pool_stops_when_the_next_pause_would_pass_its_budget() {
    let pool = pool(
        vec![("one", vec![busy(), busy(), busy()])],
        Some(Duration::from_secs(5)),
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert!(result.is_err());
    assert_eq!(
        waits,
        vec![Duration::from_secs(2)],
        "2 s fits, 2+4 s does not"
    );
    assert_eq!(calls(&pool), ["one", "one"]);
}

#[tokio::test(start_paused = true)]
async fn b11_ready_members_go_next_and_those_asking_for_time_later() {
    let slow =
        || Err(PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(10)));
    let pool = pool(
        vec![
            ("one", vec![slow(), Ok(answer())]),
            ("two", vec![busy(), busy()]),
        ],
        Some(Duration::from_secs(60)),
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert_eq!(result.expect("answered").upstream_id, "one");
    assert_eq!(
        waits,
        vec![Duration::from_secs(2)],
        "the pause is two seconds: one member is back by then"
    );
    assert_eq!(
        calls(&pool),
        ["one", "two", "two", "one"],
        "the ready member is asked before the one still asking for time"
    );
}

#[tokio::test(start_paused = true)]
async fn b11_the_pause_waits_out_the_soonest_vendor_time() {
    let slow = |secs| {
        Err(PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(secs)))
    };
    let pool = pool(
        vec![
            ("one", vec![slow(20), Ok(answer())]),
            ("two", vec![slow(10), busy()]),
        ],
        Some(Duration::from_secs(60)),
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert_eq!(result.expect("answered").upstream_id, "one");
    assert_eq!(waits, vec![Duration::from_secs(10)]);
    assert_eq!(calls(&pool), ["one", "two", "two", "one"]);
}

#[tokio::test(start_paused = true)]
async fn b11_a_last_member_newly_out_of_quota_still_lets_busy_ones_wait() {
    let pool = pool(
        vec![
            ("one", vec![busy(), Ok(answer())]),
            (
                "two",
                vec![Err(PluginError::quota_status(429, "out of quota"))],
            ),
        ],
        Some(Duration::from_secs(60)),
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert_eq!(result.expect("answered").upstream_id, "one");
    assert_eq!(waits, vec![Duration::from_secs(2)]);
    assert_eq!(
        calls(&pool),
        ["one", "two", "one"],
        "the spent member is not asked again"
    );
}

#[tokio::test(start_paused = true)]
async fn b11_a_last_member_already_resting_on_quota_ends_the_request() {
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    cooldowns.record_failure_for_model(
        &member_id("two"),
        "demo",
        &PluginError::quota_status(429, "out of quota"),
    );
    let pool = pool(
        vec![
            ("one", vec![busy()]),
            (
                "two",
                vec![Err(PluginError::quota_status(429, "still out"))],
            ),
        ],
        Some(Duration::from_secs(60)),
        Some(cooldowns),
    );
    let (result, waits) = ask(&pool).await;
    assert!(result.is_err());
    assert!(
        waits.is_empty(),
        "no pause for a member that was already spent"
    );
    assert_eq!(calls(&pool), ["one", "two"]);
}

#[tokio::test(start_paused = true)]
async fn b11_a_last_failure_that_does_not_pass_ends_the_request() {
    let pool = pool(
        vec![
            ("one", vec![busy()]),
            ("two", vec![Err(PluginError::status_error(401, "denied"))]),
        ],
        Some(Duration::from_secs(60)),
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert!(result.is_err());
    assert!(waits.is_empty());
    assert_eq!(calls(&pool), ["one", "two"]);
}

#[tokio::test(start_paused = true)]
async fn b11_a_busy_last_member_waits_though_an_earlier_one_failed_for_good() {
    let pool = pool(
        vec![
            ("one", vec![Err(PluginError::status_error(401, "denied"))]),
            ("two", vec![busy(), Ok(answer())]),
        ],
        Some(Duration::from_secs(60)),
        None,
    );
    let (result, waits) = ask(&pool).await;
    assert_eq!(result.expect("answered").upstream_id, "two");
    assert_eq!(waits, vec![Duration::from_secs(2)]);
    assert_eq!(
        calls(&pool),
        ["one", "two", "two"],
        "only the busy member is asked again"
    );
}
