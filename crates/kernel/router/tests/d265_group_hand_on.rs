//! Group hand-on to members left out for guessed rests (D-265).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::stream;
use hofund_ir::{ContentPart, Message, Request, Response, Role, StreamEvent, TextDelta};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router,
};

#[derive(Clone, Copy)]
enum Reply {
    Unary,
    Status(u16, &'static str),
    StreamStatus(u16),
    StreamOk,
    Quota,
    Credit,
    Timeout,
    Connect,
}

struct Fake {
    id: &'static str,
    member_id: MemberId,
    seen: Arc<Mutex<Vec<&'static str>>>,
    reply: Reply,
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

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.seen.lock().expect("seen").push(self.id);
        match self.reply {
            Reply::Unary => Ok(Completion::Unary(Box::new(answer()))),
            Reply::Status(status, message) => Err(PluginError::status_error(status, message)),
            Reply::StreamStatus(status) => Ok(Completion::Stream(Box::pin(stream::iter([Err(
                PluginError::status_error(status, "before content"),
            )])))),
            Reply::StreamOk => Ok(Completion::Stream(Box::pin(stream::iter([Ok(
                text_event("ok"),
            )])))),
            Reply::Quota => Err(PluginError::quota_status(429, "quota exhausted")),
            Reply::Credit => Err(PluginError::credit_status(402, "credit exhausted")),
            Reply::Timeout => Err(PluginError::timeout("slow first token")),
            Reply::Connect => Err(PluginError::connect("connection failed")),
        }
    }
}

fn answer() -> Response {
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

fn member_id(id: &'static str) -> MemberId {
    MemberId::from_preset(PresetId::from_static(id), MemberKey::from_static("row"))
}

#[derive(Clone, Copy)]
enum Rest {
    Rate,
    Outage,
    Slow,
    Overload,
    Quota,
    Credit,
    Key,
    RetryRate,
    RetryOutage,
}

struct Fixture {
    now: Arc<Mutex<Instant>>,
    book: Arc<Cooldowns>,
    seen: Arc<Mutex<Vec<&'static str>>>,
}

impl Fixture {
    fn new() -> Self {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = Arc::clone(&now);
        Self {
            now,
            book: Arc::new(Cooldowns::with_clock(
                CooldownPolicy::default(),
                Arc::new(move || *clock.lock().expect("clock")),
            )),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn rest(&self, id: &'static str, rest: Rest) {
        let error = match rest {
            Rest::Rate => PluginError::status_error(429, "too many requests"),
            Rest::Outage => PluginError::status_error(503, "unavailable"),
            Rest::Slow => PluginError::timeout("slow first token"),
            Rest::Overload => PluginError::status_error(529, "overloaded"),
            Rest::Quota => PluginError::quota_status(429, "quota exhausted"),
            Rest::Credit => PluginError::credit_status(402, "credit exhausted"),
            Rest::Key => PluginError::status_error(401, "bad key"),
            Rest::RetryRate => PluginError::status_error(429, "too many requests")
                .with_retry_after(Duration::from_secs(120)),
            Rest::RetryOutage => PluginError::status_error(503, "unavailable")
                .with_retry_after(Duration::from_secs(120)),
        };
        self.book
            .record_failure_for_model(&member_id(id), "demo", &error)
            .expect("rest");
        *self.now.lock().expect("clock") += Duration::from_secs(10);
    }

    fn advance(&self, seconds: u64) {
        *self.now.lock().expect("clock") += Duration::from_secs(seconds);
    }

    fn router(&self, replies: &[(&'static str, Reply)]) -> Router {
        let hops = replies
            .iter()
            .map(|(id, reply)| {
                Hop::new(Arc::new(Fake {
                    id,
                    member_id: member_id(id),
                    seen: Arc::clone(&self.seen),
                    reply: *reply,
                }) as Arc<dyn Member>)
            })
            .collect();
        Router::from_routes(
            BTreeMap::from([(
                "demo".to_owned(),
                Route::new(hops).with_routing("order").with_patience(None),
            )]),
            FailoverPolicy::default(),
        )
        .with_cooldowns(Arc::clone(&self.book))
    }

    fn clear_seen(&self) {
        self.seen.lock().expect("seen").clear();
    }

    fn seen(&self) -> Vec<&'static str> {
        self.seen.lock().expect("seen").clone()
    }
}

async fn dispatch(router: &Router) -> Result<hofund_router::Routed, hofund_router::RouterError> {
    router.dispatch(&Request::new("demo"), "test").await
}

#[tokio::test]
async fn d265_rate_rest_accepts_any_member_failure() {
    let cases = [
        ("401", Reply::Status(401, "bad key")),
        ("402", Reply::Credit),
        ("403", Reply::Status(403, "forbidden")),
        ("404", Reply::Status(404, "model missing")),
        ("quota 429", Reply::Quota),
        ("plain 429", Reply::Status(429, "too many requests")),
        ("500", Reply::Status(500, "failed")),
        ("503", Reply::Status(503, "unavailable")),
        ("504", Reply::Status(504, "slow first token")),
        ("timeout", Reply::Timeout),
        ("connect", Reply::Connect),
    ];
    for (name, failure) in cases {
        let fixture = Fixture::new();
        fixture.rest("first", Rest::Rate);
        fixture.rest("second", Rest::Rate);
        let router = fixture.router(&[("first", failure), ("second", Reply::Unary)]);
        let routed = dispatch(&router).await.expect(name);
        assert_eq!(routed.upstream_id, "second", "{name}");
        assert_eq!(fixture.seen(), ["first", "second"], "{name}");
    }
}

#[tokio::test]
async fn d265_outage_slow_and_overload_rests_accept_only_momentary_failures() {
    let cases = [
        ("500", Reply::Status(500, "failed"), true),
        ("503", Reply::Status(503, "unavailable"), true),
        ("504", Reply::Status(504, "slow first token"), true),
        ("529", Reply::Status(529, "overloaded"), true),
        ("429", Reply::Status(429, "too many requests"), true),
        ("timeout", Reply::Timeout, true),
        ("connect", Reply::Connect, true),
        ("401", Reply::Status(401, "bad key"), false),
        ("402", Reply::Credit, false),
        ("403", Reply::Status(403, "forbidden"), false),
        ("404", Reply::Status(404, "model missing"), false),
        ("quota", Reply::Quota, false),
    ];
    for rest in [Rest::Outage, Rest::Slow, Rest::Overload] {
        for (name, failure, expected) in cases {
            let fixture = Fixture::new();
            fixture.rest(
                "first",
                if matches!(rest, Rest::Overload) {
                    Rest::Overload
                } else {
                    Rest::Rate
                },
            );
            fixture.rest("second", rest);
            let router = fixture.router(&[("first", failure), ("second", Reply::Unary)]);
            let result = dispatch(&router).await;
            assert_eq!(result.is_ok(), expected, "{name}");
            let expected_seen: &[&str] = if expected {
                &["first", "second"]
            } else {
                &["first"]
            };
            assert_eq!(fixture.seen(), expected_seen, "{name}");
        }
    }
}

#[tokio::test]
async fn d265_request_failures_do_not_hand_on() {
    for rest in [Rest::Rate, Rest::Outage] {
        for failure in [
            Reply::Status(400, "invalid request"),
            Reply::Status(400, "context length exceeded: too many tokens"),
            Reply::Status(413, "request body too large"),
        ] {
            let fixture = Fixture::new();
            fixture.rest("first", Rest::Rate);
            fixture.rest("second", rest);
            let router = fixture.router(&[("first", failure), ("second", Reply::Unary)]);
            assert!(dispatch(&router).await.is_err());
            assert_eq!(fixture.seen(), ["first"]);
        }
    }
}

#[tokio::test]
async fn d265_longer_rests_stay_out() {
    for rest in [
        Rest::Quota,
        Rest::Credit,
        Rest::Key,
        Rest::RetryRate,
        Rest::RetryOutage,
    ] {
        let fixture = Fixture::new();
        fixture.rest("first", Rest::Rate);
        fixture.rest("second", rest);
        let router = fixture.router(&[
            ("first", Reply::Status(503, "unavailable")),
            ("second", Reply::Unary),
        ]);
        assert!(dispatch(&router).await.is_err());
        assert_eq!(fixture.seen(), ["first"]);
    }
}

#[tokio::test]
async fn d265_left_out_members_are_asked_soonest_first() {
    let fixture = Fixture::new();
    fixture.rest("first", Rest::Rate);
    fixture.rest("third", Rest::Rate);
    fixture.rest("second", Rest::Rate);
    let router = fixture.router(&[
        ("first", Reply::Status(503, "unavailable")),
        ("second", Reply::Unary),
        ("third", Reply::Unary),
    ]);
    let routed = dispatch(&router).await.expect("hand-on");
    assert_eq!(routed.upstream_id, "third");
    assert_eq!(fixture.seen(), ["first", "third"]);
}

#[tokio::test]
async fn d265_each_last_failure_asks_one_more_left_out_member() {
    let fixture = Fixture::new();
    fixture.rest("first", Rest::Rate);
    fixture.rest("second", Rest::Rate);
    fixture.rest("third", Rest::Rate);
    let router = fixture.router(&[
        ("first", Reply::Status(503, "unavailable")),
        ("second", Reply::Status(503, "unavailable")),
        ("third", Reply::Unary),
    ]);
    let routed = dispatch(&router).await.expect("second hand-on");
    assert_eq!(routed.upstream_id, "third");
    assert_eq!(fixture.seen(), ["first", "second", "third"]);
}

#[tokio::test]
async fn d265_stream_failure_before_content_hands_on() {
    let fixture = Fixture::new();
    fixture.rest("first", Rest::Rate);
    fixture.rest("second", Rest::Outage);
    let router = fixture.router(&[
        ("first", Reply::StreamStatus(503)),
        ("second", Reply::StreamOk),
    ]);
    let routed = dispatch(&router).await.expect("stream hand-on");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(fixture.seen(), ["first", "second"]);
}

#[tokio::test]
async fn d265_resting_member_gets_no_other_hand_on_for_thirty_seconds() {
    let fixture = Fixture::new();
    fixture.rest("first", Rest::Outage);
    fixture.rest("second", Rest::Outage);
    let router = fixture.router(&[
        ("first", Reply::Status(503, "unavailable")),
        ("second", Reply::Status(503, "unavailable")),
    ]);

    assert!(dispatch(&router).await.is_err());
    assert_eq!(fixture.seen(), ["first", "second"]);

    fixture.clear_seen();
    assert!(dispatch(&router).await.is_err());
    assert_eq!(fixture.seen(), ["first"]);

    fixture.advance(29);
    fixture.clear_seen();
    assert!(dispatch(&router).await.is_err());
    assert_eq!(fixture.seen(), ["first"]);

    fixture.advance(1);
    fixture.clear_seen();
    assert!(dispatch(&router).await.is_err());
    assert_eq!(fixture.seen(), ["first", "second"]);
}

#[tokio::test]
async fn d265_soonest_probe_counts_toward_the_guard_across_snapshots() {
    let fixture = Fixture::new();
    fixture.rest("first", Rest::Overload);
    fixture.rest("second", Rest::Rate);
    let first_router = fixture.router(&[
        ("first", Reply::Status(503, "unavailable")),
        ("second", Reply::Unary),
    ]);
    dispatch(&first_router).await.expect("first answer");
    assert_eq!(fixture.seen(), ["first", "second"]);

    fixture.advance(5);
    fixture.rest("first", Rest::Outage);
    fixture.rest("third", Rest::Quota);
    let next_router = fixture.router(&[
        ("first", Reply::Unary),
        ("second", Reply::Status(503, "unavailable")),
        ("third", Reply::Unary),
    ]);
    fixture.clear_seen();
    assert!(dispatch(&next_router).await.is_err());
    assert_eq!(fixture.seen(), ["second"]);

    fixture.advance(5);
    fixture.clear_seen();
    dispatch(&next_router).await.expect("guard elapsed");
    assert_eq!(fixture.seen(), ["second", "first"]);
}

#[tokio::test]
async fn d265_success_releases_the_hand_on_guard() {
    let fixture = Fixture::new();
    fixture.rest("first", Rest::Outage);
    fixture.rest("second", Rest::Outage);
    let router = fixture.router(&[
        ("first", Reply::Status(503, "unavailable")),
        ("second", Reply::Unary),
    ]);

    dispatch(&router).await.expect("first answer");
    fixture
        .book
        .record_success_for_model(&member_id("second"), "demo");
    fixture.clear_seen();
    dispatch(&router).await.expect("second answer");
    assert_eq!(fixture.seen(), ["first", "second"]);
}

#[tokio::test]
async fn d265_named_provider_has_no_hand_on() {
    let fixture = Fixture::new();
    fixture.rest("first", Rest::Rate);
    fixture.rest("second", Rest::Rate);
    let router = fixture.router(&[
        ("first", Reply::Status(429, "too many requests")),
        ("second", Reply::Unary),
    ]);
    let result = router.dispatch(&Request::new("first/demo"), "test").await;
    assert!(result.is_err());
    assert_eq!(fixture.seen(), ["first"]);
}
