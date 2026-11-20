//! Group routing `fit` asks the smallest known context window first (D-239).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router};

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<&'static str>>>,
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
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn hop(id: &'static str, window: Option<u64>, seen: &Arc<Mutex<Vec<&'static str>>>) -> Hop {
    let mut hop = Hop::new(Arc::new(Fake {
        id,
        member_id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        seen: Arc::clone(seen),
    }));
    hop.context_window = window;
    hop
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
    let mut request = hofund_router::Request::new("demo");
    let mut message = Message::new(Role::User);
    // 20_000 characters is 5_000 tokens, past the 4_000 window.
    message.content.push(ContentPart::text("x".repeat(20_000)));
    request.messages.push(message);
    router
        .dispatch(&request, "test")
        .await
        .expect("long request");
    assert_eq!(seen.lock().expect("seen").as_slice(), ["wide"]);
}
