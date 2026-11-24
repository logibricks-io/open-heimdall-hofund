//! Strict stays: a count uses the stuck account, a fork follows that
//! account, and an unanswered subagent follows its lead.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Response, Role};
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::affinity::{AffinityState, CacheUsage, Conversation, Stays};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router};

struct Fake {
    id: MemberId,
    route_id: &'static str,
    hits: Arc<AtomicUsize>,
    counts: Arc<Mutex<Vec<&'static str>>>,
    answers: Mutex<VecDeque<Result<Completion, PluginError>>>,
    count: Count,
}

enum Count {
    Number(u64),
    Missing,
    Reject(u16),
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

    async fn count_input(
        &self,
        _request: hofund_router::Request,
        _inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.counts.lock().expect("counts").push(self.route_id);
        match self.count {
            Count::Number(tokens) => Ok(CountOutcome::Counted(tokens)),
            Count::Missing => Ok(CountOutcome::Unsupported),
            Count::Reject(status) => Err(PluginError::status_error(status, "no count")),
        }
    }

    async fn execute(
        &self,
        _request: hofund_router::Request,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

fn member(
    id: &'static str,
    hits: &Arc<AtomicUsize>,
    counts: &Arc<Mutex<Vec<&'static str>>>,
    answers: Vec<Result<Completion, PluginError>>,
    count: Count,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id: id,
        hits: Arc::clone(hits),
        counts: Arc::clone(counts),
        answers: Mutex::new(answers.into_iter().collect()),
        count,
    })
}

fn answer() -> Result<Completion, PluginError> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Ok(Completion::Unary(Box::new(Response::new(message))))
}

fn router(
    stays: Stays,
    routing: &str,
    first_count: Count,
    second_count: Count,
) -> (Router, Arc<Mutex<Vec<&'static str>>>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(Mutex::new(Vec::new()));
    let first = member(
        "first",
        &hits,
        &counts,
        vec![Err(PluginError::status_error(429, "limited")), answer()],
        first_count,
    );
    let second = member(
        "second",
        &hits,
        &counts,
        vec![answer(), answer(), answer(), answer()],
        second_count,
    );
    let route = Route::new(vec![Hop::new(first), Hop::new(second)])
        .with_routing(routing)
        .with_stays(stays);
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    );
    (router, counts)
}

fn named(key: &str, body: &str) -> Conversation {
    Conversation::new(key, 2, false)
        .with_body(body.as_bytes().to_vec())
        .with_named_session(true)
}

const PARENT: &str = r#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"from parent"},{"role":"user","content":"more"}]}"#;
const FORK: &str = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},{"role":"assistant","content":[{"type":"text","text":"from parent","cache_control":{"type":"ephemeral"}}]},{"role":"user","content":"in the branch"}]}"#;
const OTHER: &str =
    r#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"from other"}]}"#;

async fn dispatch(router: &Router, conversation: &Conversation) -> String {
    router
        .dispatch_with_context(
            &hofund_router::Request::new("demo"),
            "test",
            Some(conversation),
            None,
        )
        .await
        .expect("dispatch")
        .upstream_id
}

#[tokio::test]
async fn d244_strict_dispatch_keeps_a_fork_and_a_subagent() {
    let (router, counts) = router(Stays::Strict, "order", Count::Number(11), Count::Number(22));
    let parent = named(
        "demo|parent",
        r#"{"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(dispatch(&router, &parent).await, "second");
    assert_eq!(
        dispatch(&router, &named("demo|parent", PARENT)).await,
        "second"
    );

    let fork_count = router
        .count_scoped_conversation(
            &hofund_router::Request::new("demo"),
            "test",
            None,
            Some(&named("demo|fork", FORK)),
        )
        .await
        .expect("count before the fork is answered");
    assert_eq!(fork_count.tokens, 11);
    assert!(!fork_count.estimated);
    assert_eq!(counts.lock().expect("counts").as_slice(), ["first"]);

    let parent_count = router
        .count_scoped_conversation(
            &hofund_router::Request::new("demo"),
            "test",
            None,
            Some(&named("demo|parent", PARENT)),
        )
        .await
        .expect("count");
    assert_eq!(parent_count.tokens, 22);
    assert!(!parent_count.estimated);
    assert_eq!(parent_count.upstream_id, "second");

    assert_eq!(dispatch(&router, &named("demo|fork", FORK)).await, "second");
    assert_eq!(
        dispatch(&router, &named("demo|other", OTHER)).await,
        "first"
    );
    assert_eq!(
        dispatch(
            &router,
            &named(
                "demo|parent|agent:one",
                r#"{"messages":[{"role":"user","content":"task"}]}"#
            )
        )
        .await,
        "second"
    );
    assert_eq!(
        counts.lock().expect("counts").as_slice(),
        ["first", "second"]
    );
}

#[tokio::test]
async fn d244_strict_count_estimates_when_the_account_is_unavailable() {
    let state_router = {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let first = member("first", &hits, &seen, vec![answer()], Count::Number(11));
        let second = member("second", &hits, &seen, vec![answer()], Count::Number(22));
        let route = Route::new(vec![Hop::new(first), Hop::new(second)])
            .with_routing("order")
            .with_stays(Stays::Strict);
        let state = AffinityState::in_memory();
        state.answered(
            "demo|parent",
            "preset:missing:row",
            "demo",
            1,
            CacheUsage {
                input: Some(4),
                ..CacheUsage::default()
            },
        );
        (
            Router::from_routes(
                BTreeMap::from([("demo".to_owned(), route)]),
                FailoverPolicy::default(),
            )
            .with_affinity(state),
            seen,
        )
    };
    let missing = state_router
        .0
        .count_scoped_conversation(
            &hofund_router::Request::new("demo"),
            "test",
            None,
            Some(&named("demo|parent", PARENT)),
        )
        .await
        .expect("estimate");
    assert!(missing.estimated);
    assert!(state_router.1.lock().expect("seen").is_empty());

    let stuck = AffinityState::in_memory();
    stuck.answered(
        "demo|parent",
        "preset:second:row",
        "demo",
        1,
        CacheUsage::default(),
    );
    let unsupported_seen = Arc::new(Mutex::new(Vec::new()));
    let unsupported = Router::from_routes(
        BTreeMap::from([(
            "demo".to_owned(),
            Route::new(vec![
                Hop::new(member(
                    "first",
                    &Arc::new(AtomicUsize::new(0)),
                    &unsupported_seen,
                    vec![answer()],
                    Count::Number(11),
                )),
                Hop::new(member(
                    "second",
                    &Arc::new(AtomicUsize::new(0)),
                    &unsupported_seen,
                    vec![answer()],
                    Count::Missing,
                )),
            ])
            .with_routing("order")
            .with_stays(Stays::Strict),
        )]),
        FailoverPolicy::default(),
    )
    .with_affinity(stuck);
    let estimated = unsupported
        .count_scoped_conversation(
            &hofund_router::Request::new("demo"),
            "test",
            None,
            Some(&named("demo|parent", PARENT)),
        )
        .await
        .expect("unsupported count");
    assert!(estimated.estimated);
    assert_eq!(
        unsupported_seen.lock().expect("counts").as_slice(),
        ["second"]
    );

    let failing_seen = Arc::new(Mutex::new(Vec::new()));
    let failing_state = AffinityState::in_memory();
    failing_state.answered(
        "demo|parent",
        "preset:second:row",
        "demo",
        1,
        CacheUsage::default(),
    );
    let failing = Router::from_routes(
        BTreeMap::from([(
            "demo".to_owned(),
            Route::new(vec![
                Hop::new(member(
                    "first",
                    &Arc::new(AtomicUsize::new(0)),
                    &failing_seen,
                    vec![answer()],
                    Count::Number(11),
                )),
                Hop::new(member(
                    "second",
                    &Arc::new(AtomicUsize::new(0)),
                    &failing_seen,
                    vec![answer()],
                    Count::Reject(500),
                )),
            ])
            .with_routing("order")
            .with_stays(Stays::Strict),
        )]),
        FailoverPolicy::default(),
    )
    .with_affinity(failing_state);
    let error = match failing
        .count_scoped_conversation(
            &hofund_router::Request::new("demo"),
            "test",
            None,
            Some(&named("demo|parent", PARENT)),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("stuck account error"),
    };
    assert!(error.to_string().contains("second"));
    assert_eq!(failing_seen.lock().expect("seen").as_slice(), ["second"]);
}

#[tokio::test]
async fn d244_session_count_ignores_the_stick() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(Mutex::new(Vec::new()));
    let first = member("first", &hits, &counts, vec![answer()], Count::Number(11));
    let second = member("second", &hits, &counts, vec![answer()], Count::Number(22));
    let route = Route::new(vec![Hop::new(first), Hop::new(second)])
        .with_routing("order")
        .with_stays(Stays::Session);
    let state = AffinityState::in_memory();
    state.answered(
        "demo|parent",
        "preset:second:row",
        "demo",
        1,
        CacheUsage::default(),
    );
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    )
    .with_affinity(state);
    let count = router
        .count_scoped_conversation(
            &hofund_router::Request::new("demo"),
            "test",
            None,
            Some(&named("demo|parent", PARENT)),
        )
        .await
        .expect("count");
    assert_eq!(count.tokens, 11);
    assert_eq!(count.upstream_id, "first");
}
