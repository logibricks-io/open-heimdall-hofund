//! Group routing `fit` asks the smallest known context window first (D-239).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router, RouterError,
    WaitHook,
};

#[derive(Clone, Copy)]
enum Script {
    Ok,
    Status(u16),
    Quota,
    Credit,
    /// A short rate-limit pause, one second, which still passes.
    Soon(u16),
}

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<&'static str>>>,
    /// The last script repeats once the earlier ones have been used.
    scripts: Mutex<Vec<Script>>,
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
        match self.next_script() {
            Script::Ok => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(ContentPart::text("ok"));
                Ok(Completion::Unary(Box::new(Response::new(message))))
            }
            Script::Status(status) => Err(PluginError::status_error(status, "unavailable")),
            Script::Quota => Err(PluginError::quota_status(429, "quota")),
            Script::Credit => Err(PluginError::credit_status(402, "credit")),
            Script::Soon(status) => Err(PluginError::status_error(status, "unavailable")
                .with_retry_after(Duration::from_secs(1))),
        }
    }
}

impl Fake {
    fn next_script(&self) -> Script {
        let mut scripts = self.scripts.lock().expect("scripts");
        if scripts.len() > 1 {
            scripts.remove(0)
        } else {
            scripts.last().copied().unwrap_or(Script::Ok)
        }
    }
}

fn hop(id: &'static str, window: Option<u64>, seen: &Arc<Mutex<Vec<&'static str>>>) -> Hop {
    scripted(id, window, seen, Script::Ok)
}

fn scripted(
    id: &'static str,
    window: Option<u64>,
    seen: &Arc<Mutex<Vec<&'static str>>>,
    script: Script,
) -> Hop {
    sequenced(id, window, seen, vec![script])
}

fn sequenced(
    id: &'static str,
    window: Option<u64>,
    seen: &Arc<Mutex<Vec<&'static str>>>,
    scripts: Vec<Script>,
) -> Hop {
    let member = Arc::new(Fake {
        id,
        member_id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        seen: Arc::clone(seen),
        scripts: Mutex::new(scripts),
    });
    let mut hop = Hop::new(member);
    hop.context_window = window;
    hop
}

fn long_request(chars: usize) -> hofund_router::Request {
    let mut request = hofund_router::Request::new("demo");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("x".repeat(chars)));
    request.messages.push(message);
    request
}

fn router(routing: &str, hops: Vec<Hop>) -> Router {
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops).with_routing(routing))]),
        FailoverPolicy::default(),
    )
}

#[tokio::test]
async fn d239_fit_route_asks_the_smallest_known_window_first() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        hop("wide", Some(1_000_000), &seen),
        hop("unknown", None, &seen),
        hop("narrow", Some(4_000), &seen),
    ];
    let fit = router("fit", hops);
    fit.dispatch(&hofund_router::Request::new("demo"), "test")
        .await
        .expect("fit dispatch");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["narrow"]);

    seen.lock().expect("seen").clear();
    let hops = vec![
        hop("wide", Some(1_000_000), &seen),
        hop("unknown", None, &seen),
        hop("narrow", Some(4_000), &seen),
    ];
    let smart = router("smart", hops);
    smart
        .dispatch(&hofund_router::Request::new("demo"), "test")
        .await
        .expect("smart dispatch");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["wide"]);
}

#[tokio::test]
async fn d239_fit_leaves_a_window_the_estimate_exceeds_until_last() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        hop("narrow", Some(4_000), &seen),
        hop("wide", Some(1_000_000), &seen),
    ];
    let router = router("fit", hops);
    // 20_000 characters is 5_000 tokens, past the 4_000 window.
    router
        .dispatch(&long_request(20_000), "test")
        .await
        .expect("long request");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["wide"]);
}

#[tokio::test]
async fn d239_fit_a_fitting_member_500_does_not_ask_the_too_small_member() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        scripted("narrow", Some(4_000), &seen, Script::Ok),
        scripted("wide", Some(1_000_000), &seen, Script::Status(500)),
        scripted("unknown", None, &seen, Script::Status(500)),
    ];
    let router = router("fit", hops);
    let error = match router.dispatch(&long_request(20_000), "test").await {
        Ok(_) => panic!("the fitting member failed"),
        Err(error) => error,
    };
    let RouterError::Upstream { error, .. } = error else {
        panic!("upstream error");
    };
    assert_eq!(error.status(), Some(500));
    // Unknown stays with the members that may hold the request. The 500 is a
    // passing failure, so the window the estimate already exceeds is not asked.
    assert_eq!(seen.lock().expect("seen").as_slice(), ["wide", "unknown"]);
}

#[tokio::test]
async fn d239_fit_asks_the_too_small_member_after_allowance_failures() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        scripted("narrow", Some(4_000), &seen, Script::Ok),
        scripted("mid", Some(100_000), &seen, Script::Quota),
        scripted("wide", Some(1_000_000), &seen, Script::Credit),
    ];
    let router = router("fit", hops);
    router
        .dispatch(&long_request(20_000), "test")
        .await
        .expect("the too-small member answers");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["mid", "wide", "narrow"]
    );
}

#[tokio::test]
async fn d239_fit_an_earlier_other_failure_blocks_the_too_small_member() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        scripted("narrow", Some(4_000), &seen, Script::Ok),
        scripted("mid", Some(100_000), &seen, Script::Status(500)),
        scripted("wide", Some(1_000_000), &seen, Script::Quota),
    ];
    let router = router("fit", hops);
    if router.dispatch(&long_request(20_000), "test").await.is_ok() {
        panic!("an earlier 500 blocks the last resort");
    }
    assert_eq!(seen.lock().expect("seen").as_slice(), ["mid", "wide"]);
}

#[tokio::test]
async fn d239_fit_asks_in_order_when_every_window_is_too_small() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        scripted("larger", Some(2_000), &seen, Script::Ok),
        scripted("smaller", Some(1_000), &seen, Script::Status(401)),
    ];
    let router = router("fit", hops);
    router
        .dispatch(&long_request(20_000), "test")
        .await
        .expect("both windows are too small, so both are asked");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["smaller", "larger"]);
}

struct Recording(Arc<Mutex<Vec<Duration>>>);

impl WaitHook for Recording {
    fn wait(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        self.0.lock().expect("waits").push(duration);
        Box::pin(std::future::ready(()))
    }
}

async fn ask_waiting(
    hops: Vec<Hop>,
    patience: Option<Duration>,
    request: &hofund_router::Request,
) -> (Result<hofund_router::Routed, RouterError>, Vec<Duration>) {
    let waits = Arc::new(Mutex::new(Vec::new()));
    let router = Router::from_routes(
        BTreeMap::from([(
            "demo".to_owned(),
            Route::new(hops).with_routing("fit").with_patience(patience),
        )]),
        FailoverPolicy::default(),
    );
    let result = router
        .dispatch_with_wait(
            request,
            "test",
            None,
            Some(Arc::new(Recording(Arc::clone(&waits)))),
        )
        .await;
    let waits = waits.lock().expect("waits").clone();
    (result, waits)
}

#[tokio::test]
async fn d239_fit_replan_then_allowance_asks_the_left_out_member() {
    let request = long_request(20_000);
    for scripts in [
        vec![Script::Status(500), Script::Quota],
        vec![Script::Soon(429), Script::Credit],
    ] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hops = vec![
            sequenced("narrow", Some(4_000), &seen, vec![Script::Ok]),
            sequenced("wide", Some(1_000_000), &seen, scripts),
        ];
        let (result, waits) = ask_waiting(hops, Some(Duration::from_secs(60)), &request).await;
        result.expect("the left-out member answers after the replan");
        assert!(!waits.is_empty(), "the passing failure is replanned");
        assert_eq!(
            seen.lock().expect("seen").as_slice(),
            ["wide", "wide", "narrow"]
        );
    }
}

#[tokio::test]
async fn d239_fit_a_replanned_500_does_not_ask_the_left_out_member() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = vec![
        sequenced("narrow", Some(4_000), &seen, vec![Script::Ok]),
        sequenced("wide", Some(1_000_000), &seen, vec![Script::Status(500)]),
    ];
    // Three seconds covers the first two-second pause and not the next one.
    let (result, waits) =
        ask_waiting(hops, Some(Duration::from_secs(3)), &long_request(20_000)).await;
    assert!(result.is_err(), "a 500 that keeps passing is the error");
    assert_eq!(waits.len(), 1, "replanned once");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["wide", "wide"]);
}

#[tokio::test]
async fn d239_fit_a_resting_left_out_member_is_not_asked() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let resting = Arc::new(Fake {
        id: "narrow",
        member_id: MemberId::from_preset(
            PresetId::try_from("narrow").expect("preset"),
            MemberKey::from_static("row"),
        ),
        seen: Arc::clone(&seen),
        scripts: Mutex::new(vec![Script::Ok]),
    });
    let cooldowns = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    cooldowns.record_failure_for_model(
        resting.id(),
        "demo",
        &PluginError::quota_status(429, "quota"),
    );
    let mut narrow = Hop::new(resting);
    narrow.context_window = Some(1_000);
    let hops = vec![
        narrow,
        sequenced("open", Some(2_000), &seen, vec![Script::Ok]),
        sequenced("wide", Some(1_000_000), &seen, vec![Script::Quota]),
    ];
    let router = router("fit", hops).with_cooldowns(cooldowns);
    router
        .dispatch(&long_request(20_000), "test")
        .await
        .expect("the left-out member that is not resting answers");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["wide", "open"]);
}
