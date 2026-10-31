//! Failover happens only before a stream event is produced.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream;
use hofund_ir::{ContentPart, Message, Request, Response, Role, StreamEvent, TextDelta};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PluginErrorKind, PresetId};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Member, Router, RouterError, Snapshot,
};

struct Fake {
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    kind: Kind,
}

enum Kind {
    Status(u16, &'static str),
    Connect(&'static str),
    Transport(&'static str),
    Auth,
    Unary,
    StreamOk,
    StreamFail(u16),
    RawOk,
    RawFail,
    Credit(u16),
    FinishError,
    FinishAfterText,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        match self.kind {
            Kind::Status(status, message) => Err(PluginError::status_error(status, message)),
            Kind::Connect(message) => Err(PluginError::connect(message)),
            Kind::Transport(message) => Err(PluginError::transport(message)),
            Kind::Auth => Err(PluginError::auth("denied")),
            Kind::Unary => Ok(Completion::Unary(Box::new(reply()))),
            Kind::StreamOk => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(text_event("hi")),
                Ok(text_event("!")),
            ])))),
            Kind::StreamFail(status) => Ok(Completion::Stream(Box::pin(stream::iter(vec![Err(
                PluginError::status_error(status, "down"),
            )])))),
            Kind::RawOk => Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from_static(
                b"{\"ok\":true}",
            )))),
            Kind::RawFail => Ok(Completion::Raw(BodyStream::from_stream(stream::iter(
                vec![Err(PluginError::status_error(503, "raw"))],
            )))),
            Kind::Credit(status) => Err(PluginError::credit_status(status, "out of credit")),
            Kind::FinishError => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(StreamEvent::MessageStart(hofund_ir::MessageStart {
                    id: None,
                    model: None,
                    usage: None,
                    extensions: hofund_ir::Extensions::new(),
                })),
                // Usage without a stop does not commit the stream.
                Ok(StreamEvent::MessageDelta(hofund_ir::MessageDelta {
                    stop: None,
                    usage: None,
                    extensions: hofund_ir::Extensions::new(),
                })),
                Err(
                    PluginError::status_error(502, "the reply ended with finish_reason \"error\"")
                        .without_cooldown(),
                ),
            ])))),
            Kind::FinishAfterText => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(text_event("already sent")),
                Err(
                    PluginError::status_error(502, "the reply ended with finish_reason \"error\"")
                        .without_cooldown(),
                ),
            ])))),
        }
    }
}

fn reply() -> Response {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text("ok"));
    Response::new(message)
}

fn text_event(text: &str) -> StreamEvent {
    StreamEvent::TextDelta(TextDelta {
        index: 0,
        text: text.to_owned(),
        extensions: hofund_ir::Extensions::new(),
    })
}

fn router(members: Vec<Arc<dyn Member>>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), members);
    Router::new(routes, FailoverPolicy::default())
}

fn fake(id: &'static str, hits: &Arc<AtomicUsize>, kind: Kind) -> Arc<dyn Member> {
    fake_on(id, id, hits, kind)
}

fn fake_on(
    route_id: &'static str,
    id: &'static str,
    hits: &Arc<AtomicUsize>,
    kind: Kind,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id,
        member_id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(hits),
        kind,
    })
}

fn router_with_windows(members: Vec<(Arc<dyn Member>, Option<u64>)>) -> Router {
    let hops = members
        .into_iter()
        .map(|(member, context_window)| hofund_router::Hop {
            member,
            wire_model: None,
            max_output_tokens: None,
            context_window,
            weight: 1,
            free: false,
        })
        .collect();
    Router::from_hops(
        BTreeMap::from([("demo".to_owned(), hops)]),
        FailoverPolicy::default(),
    )
}

async fn dispatch(router: &Router, model: &str) -> Result<hofund_router::Routed, RouterError> {
    let request = Request::new(model);
    router.dispatch(&request, "inbound").await
}

#[test]
fn interrupted_errors_follow_the_connect_failover_setting() {
    let mut policy = FailoverPolicy::default();
    let error = PluginError::interrupted("cut off");
    assert!(policy.allows(&error));
    policy.connect_errors = false;
    assert!(!policy.allows(&error));
}

#[tokio::test]
async fn ordered_failover_skips_status_and_connect_failures() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let third_hits = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake("first", &first_hits, Kind::Status(503, "busy")),
        fake("second", &second_hits, Kind::Connect("refused")),
        fake("third", &third_hits, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("dispatch");
    assert_eq!(routed.upstream_id, "third");
    assert!(matches!(routed.completion, Completion::Unary(_)));
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(third_hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn client_error_does_not_try_the_next_upstream() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::Status(400, "bad"),
        ),
        fake("second", &second, Kind::Unary),
    ]);
    let error = match dispatch(&router, "demo").await {
        Ok(_) => panic!("expected a status error"),
        Err(error) => error,
    };
    let RouterError::Upstream { upstream_id, error } = error else {
        panic!("unexpected error");
    };
    assert_eq!(upstream_id, "first");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(400));
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_error_before_an_event_fails_over() {
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::StreamFail(502),
        ),
        fake("second", &Arc::new(AtomicUsize::new(0)), Kind::StreamOk),
    ]);
    let routed = dispatch(&router, "demo").await.expect("dispatch");
    assert_eq!(routed.upstream_id, "second");
    let Completion::Stream(mut stream) = routed.completion else {
        panic!("expected a stream");
    };
    let first = stream.next().await.expect("event").expect("ok");
    assert!(matches!(first, StreamEvent::TextDelta(_)));
}

#[tokio::test]
async fn stream_event_commits_the_upstream() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::StreamOk),
        fake("second", &second, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("dispatch");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_model_has_no_route() {
    let router = router(vec![]);
    let error = match dispatch(&router, "other").await {
        Ok(_) => panic!("expected a missing route"),
        Err(error) => error,
    };
    assert!(matches!(error, RouterError::NoRoute { .. }));
    assert!(router.models().is_empty());
}

#[tokio::test]
async fn auth_does_not_try_the_next_member() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::Auth),
        fake("second", &second, Kind::Unary),
    ]);
    let error = match dispatch(&router, "demo").await {
        Ok(_) => panic!("auth should fail the call"),
        Err(error) => error,
    };
    let RouterError::Upstream { error, .. } = error else {
        panic!("upstream");
    };
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_raw_error_before_a_chunk_fails_over_and_a_chunk_commits() {
    let second = Arc::new(AtomicUsize::new(0));
    let failed_over = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::RawFail),
        fake("second", &second, Kind::RawOk),
    ]);
    let routed = dispatch(&failed_over, "demo").await.expect("failover");
    assert_eq!(routed.upstream_id, "second");
    let Completion::Raw(mut body) = routed.completion else {
        panic!("raw");
    };
    let chunk = body.next_chunk().await.expect("chunk").expect("ok");
    assert_eq!(&chunk[..], b"{\"ok\":true}");
    assert_eq!(second.load(Ordering::SeqCst), 1);

    let later = Arc::new(AtomicUsize::new(0));
    let committed = router(vec![
        fake("first", &Arc::new(AtomicUsize::new(0)), Kind::RawOk),
        fake("second", &later, Kind::Unary),
    ]);
    let routed = dispatch(&committed, "demo").await.expect("commit");
    assert_eq!(routed.upstream_id, "first");
    assert_eq!(later.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_loaded_snapshot_is_unchanged_by_a_later_store() {
    let hits = Arc::new(AtomicUsize::new(0));
    let snapshot = Snapshot::new(router(vec![fake("first", &hits, Kind::Unary)]));
    let held = snapshot.load();
    snapshot.store(router(vec![]));
    let routed = dispatch(&held, "demo").await.expect("held");
    assert_eq!(routed.upstream_id, "first");
    assert!(snapshot.load().models().is_empty());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a5_credit_refusal_at_any_4xx_fails_over() {
    for status in [400, 402, 409, 422, 429] {
        let second = Arc::new(AtomicUsize::new(0));
        let router = router(vec![
            fake(
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Credit(status),
            ),
            fake("second", &second, Kind::Unary),
        ]);
        let routed = dispatch(&router, "demo").await.expect("credit failover");
        assert_eq!(routed.upstream_id, "second", "{status}");
        assert_eq!(second.load(Ordering::SeqCst), 1, "{status}");
    }
}

#[tokio::test]
async fn a7_finish_error_before_content_fails_over_without_a_rest() {
    let second = Arc::new(AtomicUsize::new(0));
    let book = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![
            fake("first", &Arc::new(AtomicUsize::new(0)), Kind::FinishError),
            fake("second", &second, Kind::Unary),
        ],
    );
    let router = Router::new(routes, FailoverPolicy::default()).with_cooldowns(Arc::clone(&book));
    let routed = dispatch(&router, "demo").await.expect("finish failover");
    assert_eq!(routed.upstream_id, "second");
    let first = MemberId::from_preset(
        PresetId::from_static("first"),
        MemberKey::from_static("row"),
    );
    assert_eq!(book.cooling(&first), None);
}

#[tokio::test]
async fn a7_finish_error_after_content_does_not_replace_the_stream() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::FinishAfterText,
        ),
        fake("second", &second, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("committed stream");
    assert_eq!(routed.upstream_id, "first");
    let Completion::Stream(mut events) = routed.completion else {
        panic!("stream");
    };
    let first = events.next().await.expect("text").expect("event");
    assert!(matches!(first, StreamEvent::TextDelta(_)));
    let error = events
        .next()
        .await
        .expect("finish error")
        .expect_err("finish error");
    assert!(!error.cools());
    assert_eq!(second.load(Ordering::SeqCst), 0);
}

async fn assert_next(status: u16, message: &'static str) {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::Status(status, message),
        ),
        fake("second", &second, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("failover");
    assert_eq!(routed.upstream_id, "second", "{status}: {message}");
    assert_eq!(second.load(Ordering::SeqCst), 1, "{status}: {message}");
}

#[tokio::test]
async fn c1_auth_statuses_try_the_next_member() {
    for status in [401, 403] {
        assert_next(status, "credentials refused").await;
    }
}

#[tokio::test]
async fn c2_payment_required_tries_the_next_member() {
    assert_next(402, "payment required").await;
}

#[tokio::test]
async fn c3_missing_model_tries_the_next_member() {
    assert_next(404, "not found").await;
}

#[tokio::test]
async fn c4_timeout_and_every_server_status_try_the_next_member() {
    for status in [408, 500, 501, 505, 529] {
        assert_next(status, "upstream unavailable").await;
    }
    let policy = FailoverPolicy::default();
    for status in 500..=599 {
        assert!(
            policy.allows(&PluginError::status_error(status, "upstream unavailable")),
            "{status}"
        );
    }
}

#[tokio::test]
async fn c5_retired_model_tries_the_next_member() {
    for (status, message) in [
        (410, r#"{"type":"ModelDeprecated"}"#),
        (404, "model_deprecated"),
        (422, "Model old has been deprecated"),
        (400, "Model old is now retired"),
    ] {
        assert_next(status, message).await;
    }
    let policy = FailoverPolicy::default();
    assert!(!policy.allows(&PluginError::status_error(
        410,
        "Model old has been deprecated; quota exceeded"
    )));
}

#[tokio::test]
async fn c6_edge_block_at_any_client_status_tries_the_next_member() {
    for (status, message) in [
        (
            400,
            "Sorry, your request has been blocked due to unusual activity",
        ),
        (405, "Attention Required: you have been blocked"),
        (422, "error code: 1020"),
        (499, "used Cloudflare to restrict access"),
    ] {
        assert_next(status, message).await;
    }
}

#[tokio::test]
async fn c7_body_word_classes_try_the_next_member() {
    let cases = [
        r#"{"error":{"message":"\u989d\u5ea6\u5df2\u7528\u5b8c"}}"#,
        "model is not available in the current token plan",
        "Illegal API invocation from an unapproved channel",
        "Failed to deserialize the JSON body: unknown item type",
        r#"{"message":"invalid request error trace_id: 0123456789abcdef","type":"invalid_request_error"}"#,
        "unsupported_country_region_territory",
        "not a chat model; use /v1/responses",
    ];
    for status in [400, 422] {
        for message in cases {
            assert_next(status, message).await;
        }
    }
}

#[tokio::test]
async fn c8_overflow_is_checked_before_status_and_quota_words() {
    // Overflow wins over a listed 400 and the quota words, so the member's
    // mates on the same model are not asked and the client is told.
    let mate = Arc::new(AtomicUsize::new(0));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![
            fake_on(
                "shared",
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Status(400, "context_length_exceeded: quota exceeded"),
            ),
            fake_on("shared", "mate", &mate, Kind::Unary),
        ],
    );
    let router = Router::new(
        routes,
        FailoverPolicy {
            connect_errors: true,
            statuses: [400].into_iter().collect(),
        },
    );
    let error = match dispatch(&router, "demo").await {
        Ok(_) => panic!("expected overflow"),
        Err(error) => error,
    };
    let RouterError::Upstream { error, .. } = error else {
        panic!("upstream");
    };
    assert_eq!(error.status(), Some(400));
    assert!(error.message().contains("context_length_exceeded"));
    assert_eq!(mate.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn c8_overflow_asks_only_members_that_may_hold_the_request() {
    let small = Arc::new(AtomicUsize::new(0));
    let roomy = Arc::new(AtomicUsize::new(0));
    let router = router_with_windows(vec![
        (
            fake(
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Status(400, "prompt is too long"),
            ),
            None,
        ),
        (fake("small", &small, Kind::Unary), Some(1)),
        (fake("roomy", &roomy, Kind::Unary), None),
    ]);
    let mut request = Request::new("demo");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("x".repeat(400)));
    request.messages.push(message);
    let routed = router
        .dispatch(&request, "inbound")
        .await
        .expect("a member with room answers");
    assert_eq!(routed.upstream_id, "roomy");
    assert_eq!(small.load(Ordering::SeqCst), 0);
    assert_eq!(roomy.load(Ordering::SeqCst), 1);

    // A byte cap says nothing of tokens: a member with a small window may
    // still take the body.
    let small = Arc::new(AtomicUsize::new(0));
    let router = router_with_windows(vec![
        (
            fake(
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Status(413, "request entity too large"),
            ),
            None,
        ),
        (fake("small", &small, Kind::Unary), Some(1)),
    ]);
    let routed = router
        .dispatch(&request, "inbound")
        .await
        .expect("a byte cap fails over");
    assert_eq!(routed.upstream_id, "small");
}

#[tokio::test]
async fn c9_shape_refusal_tries_the_next_member_without_a_rest() {
    let second = Arc::new(AtomicUsize::new(0));
    let book = Cooldowns::new(CooldownPolicy::default());
    let first = fake(
        "first",
        &Arc::new(AtomicUsize::new(0)),
        Kind::Status(422, "Unknown parameter: tools"),
    );
    let error = PluginError::status_error(422, "Unknown parameter: tools");
    assert_eq!(book.record_failure(first.id(), &error), None);
    let router = router(vec![first, fake("second", &second, Kind::Unary)]);
    let routed = dispatch(&router, "demo").await.expect("shape failover");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(second.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn c9_shape_and_protection_ask_on_whatever_the_status_list() {
    for (status, message) in [
        (422, "Unknown parameter: tools"),
        (400, "custom tools are not allowed"),
        (502, "response protection is unavailable"),
    ] {
        let second = Arc::new(AtomicUsize::new(0));
        let mut routes = BTreeMap::new();
        routes.insert(
            "demo".to_owned(),
            vec![
                fake(
                    "first",
                    &Arc::new(AtomicUsize::new(0)),
                    Kind::Status(status, message),
                ),
                fake("second", &second, Kind::Unary),
            ],
        );
        let router = Router::new(
            routes,
            FailoverPolicy {
                connect_errors: false,
                statuses: BTreeSet::new(),
            },
        );
        let routed = dispatch(&router, "demo").await.expect(message);
        assert_eq!(routed.upstream_id, "second", "{message}");
        assert_eq!(second.load(Ordering::SeqCst), 1, "{message}");
    }
}

#[tokio::test]
async fn c10_protection_skips_other_accounts_of_the_provider() {
    let mate = Arc::new(AtomicUsize::new(0));
    let other = Arc::new(AtomicUsize::new(0));
    let first = fake_on(
        "shared",
        "first",
        &Arc::new(AtomicUsize::new(0)),
        Kind::Status(502, "response protection is unavailable"),
    );
    let first_id = first.id().clone();
    let router = router(vec![
        first,
        fake_on("shared", "mate", &mate, Kind::Unary),
        fake_on("other", "other", &other, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("scoped failover");
    assert_eq!(routed.upstream_id, "other");
    assert_eq!(mate.load(Ordering::SeqCst), 0);
    assert_eq!(other.load(Ordering::SeqCst), 1);
    let error = PluginError::status_error(502, "response protection is unavailable");
    let book = Cooldowns::new(CooldownPolicy::default());
    assert_eq!(book.record_failure(&first_id, &error), None);
}

#[tokio::test]
async fn c10_region_refusal_asks_the_next_member_as_any_failure() {
    let mate = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake_on(
            "shared",
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::Status(400, "User location is not supported for the API use."),
        ),
        fake_on("shared", "mate", &mate, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("region failover");
    assert_eq!(routed.upstream_id, "shared");
    assert_eq!(mate.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn c12_too_long_unsaid_uses_the_window_margin() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router_with_windows(vec![
        (
            fake(
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Status(502, "capacity issues"),
            ),
            Some(100),
        ),
        (fake("second", &second, Kind::Unary), Some(100)),
    ]);
    let mut request = Request::new("demo");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("x".repeat(480)));
    request.messages.push(message);
    let error = match router.dispatch(&request, "inbound").await {
        Ok(_) => panic!("expected inferred overflow"),
        Err(error) => error,
    };
    let RouterError::Upstream { error, .. } = error else {
        panic!("upstream");
    };
    assert_eq!(error.status(), Some(400));
    assert!(
        error
            .message()
            .starts_with("prompt is too long: about 120 tokens")
    );
    assert!(!error.cools());
    assert_eq!(second.load(Ordering::SeqCst), 0);

    // Told as too long, the request still goes to a member with room.
    let roomy = Arc::new(AtomicUsize::new(0));
    let router = router_with_windows(vec![
        (
            fake(
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Status(502, "capacity issues"),
            ),
            Some(100),
        ),
        (fake("roomy", &roomy, Kind::Unary), None),
    ]);
    let routed = router
        .dispatch(&request, "inbound")
        .await
        .expect("a member with room answers");
    assert_eq!(routed.upstream_id, "roomy");

    let second = Arc::new(AtomicUsize::new(0));
    let router = router_with_windows(vec![
        (
            fake(
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Status(502, "capacity issues"),
            ),
            Some(100),
        ),
        (fake("second", &second, Kind::Unary), None),
    ]);
    request.messages[0].content[0] = ContentPart::text("x".repeat(476));
    let routed = router
        .dispatch(&request, "inbound")
        .await
        .expect("below the margin fails over");
    assert_eq!(routed.upstream_id, "second");

    let second = Arc::new(AtomicUsize::new(0));
    let router = router_with_windows(vec![
        (
            fake(
                "first",
                &Arc::new(AtomicUsize::new(0)),
                Kind::Status(502, "capacity issues"),
            ),
            None,
        ),
        (fake("second", &second, Kind::Unary), None),
    ]);
    request.messages[0].content[0] = ContentPart::text("x".repeat(480));
    let routed = router
        .dispatch(&request, "inbound")
        .await
        .expect("an unknown window skips the inference");
    assert_eq!(routed.upstream_id, "second");
}

#[test]
fn c13_transport_errors_use_the_pre_byte_failover_policy() {
    let mut policy = FailoverPolicy::default();
    let error = PluginError::transport("reset after the request body");
    assert!(policy.allows(&error));
    policy.connect_errors = false;
    assert!(!policy.allows(&error));
}

#[tokio::test]
async fn transport_error_before_first_byte_fails_over() {
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            Kind::Transport("reset after the request body"),
        ),
        fake("second", &second, Kind::Unary),
    ]);
    let routed = dispatch(&router, "demo").await.expect("transport failover");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(second.load(Ordering::SeqCst), 1);
}
