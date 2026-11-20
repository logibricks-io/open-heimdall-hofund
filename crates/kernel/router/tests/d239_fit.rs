//! Group routing `fit` asks the smallest known context window first (D-239).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router, RouterError};

#[derive(Clone, Copy)]
enum Script {
    Ok,
    Status(u16),
    Quota,
    Credit,
}

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<&'static str>>>,
    script: Script,
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
        match self.script {
            Script::Ok => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(ContentPart::text("ok"));
                Ok(Completion::Unary(Box::new(Response::new(message))))
            }
            Script::Status(status) => Err(PluginError::status_error(status, "unavailable")),
            Script::Quota => Err(PluginError::quota_status(429, "quota")),
            Script::Credit => Err(PluginError::credit_status(402, "credit")),
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
    let mut hop = Hop::new(Arc::new(Fake {
        id,
        member_id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        seen: Arc::clone(seen),
        script,
    }));
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
