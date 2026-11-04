//! Per-conversation overflow memory (D-38).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::affinity::Conversation;
use hofund_router::{Completion, FailoverPolicy, Hop, Member, OverflowMemory, Route, Router};

struct Fake {
    id: MemberId,
    route_id: &'static str,
    hits: Arc<AtomicUsize>,
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
        self.hits.fetch_add(1, Ordering::SeqCst);
        self.order.lock().expect("order").push(self.route_id);
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

fn fake(
    id: &'static str,
    hits: &Arc<AtomicUsize>,
    order: &Arc<Mutex<Vec<&'static str>>>,
    answers: Vec<Result<Completion, PluginError>>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id: id,
        hits: Arc::clone(hits),
        order: Arc::clone(order),
        answers: Mutex::new(answers.into_iter().collect()),
    })
}

fn ok() -> Result<Completion, PluginError> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Ok(Completion::Unary(Box::new(Response::new(message))))
}

fn overflow_err() -> Result<Completion, PluginError> {
    Err(PluginError::status_error(400, "prompt is too long"))
}

fn large_request(tokens_hint: usize) -> Request {
    let mut request = Request::new("demo");
    let mut message = Message::new(Role::User);
    message
        .content
        .push(ContentPart::text("x".repeat(tokens_hint * 4)));
    request.messages.push(message);
    request
}

fn router(first: Arc<dyn Member>, second: Arc<dyn Member>, overflow: OverflowMemory) -> Router {
    let hops = vec![Hop::new(first), Hop::new(second)];
    Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(hops))]),
        FailoverPolicy::default(),
    )
    .with_overflow_memory(overflow)
}

fn conversation(key: &str) -> Conversation {
    Conversation::new(key, 1, false)
}

async fn dispatch(
    router: &Router,
    request: &Request,
    conversation: Option<&Conversation>,
) -> hofund_router::Routed {
    router
        .dispatch_with_context(request, "test", conversation, None)
        .await
        .expect("dispatch")
}

#[tokio::test]
async fn d38_1_overflow_refusal_then_same_conversation_asks_member_last() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let memory = OverflowMemory::in_memory();
    let first = fake("first", &first_hits, &order, vec![overflow_err(), ok()]);
    let second = fake(
        "second",
        &second_hits,
        &order,
        vec![ok(), overflow_err(), ok()],
    );
    let router = router(first, second, memory);
    let request = large_request(500);
    let key = conversation("demo|one");
    dispatch(&router, &request, Some(&key)).await;
    order.lock().expect("order").clear();
    dispatch(&router, &request, Some(&key)).await;
    assert_eq!(
        order.lock().expect("order").as_slice(),
        &["second", "first"]
    );
}

#[tokio::test]
async fn d38_2_another_conversation_is_unaffected() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let memory = OverflowMemory::in_memory();
    let first = fake("first", &first_hits, &order, vec![overflow_err(), ok()]);
    let second = fake("second", &second_hits, &order, vec![ok(), ok()]);
    let router = router(first, second, memory);
    let request = large_request(500);
    dispatch(&router, &request, Some(&conversation("demo|one"))).await;
    order.lock().expect("order").clear();
    dispatch(&router, &request, Some(&conversation("demo|two"))).await;
    assert_eq!(order.lock().expect("order").as_slice(), &["first"]);
}

#[tokio::test]
async fn d38_3_success_clears_overflow_memory() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let memory = OverflowMemory::in_memory();
    let first = fake(
        "first",
        &first_hits,
        &order,
        vec![overflow_err(), ok(), ok()],
    );
    let second = fake(
        "second",
        &second_hits,
        &order,
        vec![ok(), Err(PluginError::status_error(500, "busy")), ok()],
    );
    let router = router(first, second, memory);
    let request = large_request(500);
    let key = conversation("demo|one");
    dispatch(&router, &request, Some(&key)).await;
    dispatch(&router, &request, Some(&key)).await;
    order.lock().expect("order").clear();
    dispatch(&router, &request, Some(&key)).await;
    assert_eq!(order.lock().expect("order").as_slice(), &["first"]);
}

#[tokio::test]
async fn d38_4_a_smaller_request_clears_overflow_memory() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let memory = OverflowMemory::in_memory();
    let first = fake("first", &first_hits, &order, vec![overflow_err(), ok()]);
    let second = fake("second", &second_hits, &order, vec![ok(), ok()]);
    let router = router(first, second, memory);
    let large = large_request(500);
    let small = large_request(100);
    let key = conversation("demo|one");
    dispatch(&router, &large, Some(&key)).await;
    order.lock().expect("order").clear();
    dispatch(&router, &small, Some(&key)).await;
    assert_eq!(order.lock().expect("order").as_slice(), &["first"]);
}

#[tokio::test]
async fn d38_5_a_413_refusal_is_not_remembered() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let memory = OverflowMemory::in_memory();
    let first = fake(
        "first",
        &first_hits,
        &order,
        vec![
            Err(PluginError::status_error(413, "request entity too large")),
            ok(),
        ],
    );
    let second = fake("second", &second_hits, &order, vec![ok(), ok()]);
    let router = router(first, second, memory);
    let request = large_request(500);
    let key = conversation("demo|one");
    dispatch(&router, &request, Some(&key)).await;
    order.lock().expect("order").clear();
    dispatch(&router, &request, Some(&key)).await;
    assert_eq!(order.lock().expect("order").as_slice(), &["first"]);
}

#[tokio::test]
async fn d38_6_a_single_member_route_is_still_asked() {
    let hits = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::new()));
    let memory = OverflowMemory::in_memory();
    let only = fake(
        "only",
        &hits,
        &order,
        vec![overflow_err(), overflow_err(), ok()],
    );
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), Route::new(vec![Hop::new(only)]))]),
        FailoverPolicy::default(),
    )
    .with_overflow_memory(memory);
    let request = large_request(500);
    let key = conversation("demo|one");
    let error = router
        .dispatch_with_context(&request, "test", Some(&key), None)
        .await;
    assert!(
        matches!(error, Err(hofund_router::RouterError::Upstream { .. })),
        "still too long"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let error = router
        .dispatch_with_context(&request, "test", Some(&key), None)
        .await;
    assert!(
        matches!(error, Err(hofund_router::RouterError::Upstream { .. })),
        "still too long after memory"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    dispatch(&router, &request, Some(&key)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    assert_eq!(order.lock().expect("order").last().copied(), Some("only"));
}

#[tokio::test]
async fn d38_9_requests_without_a_conversation_key_skip_memory() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let memory = OverflowMemory::in_memory();
    let first = fake("first", &first_hits, &order, vec![overflow_err(), ok()]);
    let second = fake("second", &second_hits, &order, vec![ok(), ok()]);
    let router = router(first, second, memory);
    let request = large_request(500);
    dispatch(&router, &request, None).await;
    order.lock().expect("order").clear();
    dispatch(&router, &request, None).await;
    assert_eq!(order.lock().expect("order").as_slice(), &["first"]);
}
