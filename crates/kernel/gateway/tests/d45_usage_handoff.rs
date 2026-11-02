//! Usage rows for billed tries handed to another member (D-45).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::stream;
use hofund_codec::{AnthropicMessages, Codec};
use hofund_gateway::Gateway;
use hofund_ir::{
    Extensions, Message, MessageStart, Request as ModelRequest, Response, Role, StreamEvent, Usage,
};
use hofund_plugin_api::{BilledUsage, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::{Ledger, OUTCOME_HANDED_ON, Window};
use tower::ServiceExt;

struct Scripted {
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    reply: Mutex<Option<Reply>>,
}

enum Reply {
    Stream(Vec<Result<StreamEvent, PluginError>>),
    Err(PluginError),
    Ok,
}

impl Scripted {
    fn stream(route_id: &'static str, events: Vec<Result<StreamEvent, PluginError>>) -> Arc<Self> {
        Arc::new(Self {
            route_id,
            member_id: MemberId::from_preset(
                PresetId::try_from(route_id).expect("preset"),
                MemberKey::from_static("row"),
            ),
            hits: Arc::new(AtomicUsize::new(0)),
            reply: Mutex::new(Some(Reply::Stream(events))),
        })
    }

    fn fail(route_id: &'static str, error: PluginError) -> Arc<Self> {
        Arc::new(Self {
            route_id,
            member_id: MemberId::from_preset(
                PresetId::try_from(route_id).expect("preset"),
                MemberKey::from_static("row"),
            ),
            hits: Arc::new(AtomicUsize::new(0)),
            reply: Mutex::new(Some(Reply::Err(error))),
        })
    }

    fn ok(route_id: &'static str) -> Arc<Self> {
        Arc::new(Self {
            route_id,
            member_id: MemberId::from_preset(
                PresetId::try_from(route_id).expect("preset"),
                MemberKey::from_static("row"),
            ),
            hits: Arc::new(AtomicUsize::new(0)),
            reply: Mutex::new(Some(Reply::Ok)),
        })
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Member for Scripted {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        match self.reply.lock().expect("reply").take().expect("one reply") {
            Reply::Stream(events) => Ok(Completion::Stream(Box::pin(stream::iter(events)))),
            Reply::Err(error) => Err(error),
            Reply::Ok => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(hofund_ir::ContentPart::text("ok"));
                Ok(Completion::Unary(Box::new(Response::new(message))))
            }
        }
    }
}

fn gateway(members: Vec<Arc<Scripted>>) -> (axum::Router, Arc<Ledger>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let members = members
        .into_iter()
        .map(|member| member as Arc<dyn Member>)
        .collect();
    let routes = BTreeMap::from([("demo".to_owned(), members)]);
    let codecs: HashMap<String, Arc<dyn Codec>> = HashMap::from([(
        "/v1/messages".to_owned(),
        Arc::new(AnthropicMessages) as Arc<dyn Codec>,
    )]);
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .into_router();
    (app, usage, dir)
}

fn billed_stream() -> Vec<Result<StreamEvent, PluginError>> {
    let billed = BilledUsage {
        input_tokens: Some(5),
        cache_write_5m_tokens: Some(2),
        ..BilledUsage::default()
    };
    vec![
        Ok(StreamEvent::MessageStart(MessageStart {
            id: None,
            model: None,
            usage: Some(Usage {
                input_tokens: Some(5),
                cache_write_5m_tokens: Some(2),
                ..Usage::default()
            }),
            extensions: Extensions::new(),
        })),
        Err(PluginError::status_error(502, "failed").with_billed(billed)),
    ]
}

async fn post(app: axum::Router, body: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn d45_1_streamed_billed_failure_writes_handed_on_then_success() {
    let first = Scripted::stream("first", billed_stream());
    let second = Scripted::ok("second");
    let (app, usage, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = post(
        app,
        r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(first.hits(), 1, "first hits; status={status} body={body}");
    assert_eq!(second.hits(), 1, "second hits; body={body}");
    let (rows, totals) = usage.summary(Window::All.since_now(), 10).expect("summary");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].outcome, OUTCOME_HANDED_ON);
    assert_eq!(rows[0].input_tokens, Some(5));
    assert_eq!(rows[0].cache_write_5m_tokens, Some(2));
    assert_eq!(rows[1].status, "200");
    assert_eq!(totals.requests, 1);
}

#[tokio::test]
async fn d45_2_unary_billed_failure_writes_handed_on_then_success() {
    let billed = BilledUsage {
        input_tokens: Some(4),
        output_tokens: Some(1),
        ..BilledUsage::default()
    };
    let first = Scripted::fail(
        "first",
        PluginError::status_error(502, "failed").with_billed(billed),
    );
    let second = Scripted::ok("second");
    let (app, usage, _dir) = gateway(vec![first, second]);
    let (status, _) = post(
        app,
        r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (rows, totals) = usage.summary(Window::All.since_now(), 10).expect("summary");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].outcome, OUTCOME_HANDED_ON);
    assert_eq!(rows[0].input_tokens, Some(4));
    assert_eq!(totals.requests, 1);
}

#[tokio::test]
async fn d45_3_unbilled_failure_writes_one_row() {
    let first = Scripted::fail("first", PluginError::status_error(502, "failed"));
    let second = Scripted::ok("second");
    let (app, usage, _dir) = gateway(vec![first, second]);
    let (status, _) = post(
        app,
        r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (rows, totals) = usage.summary(Window::All.since_now(), 10).expect("summary");
    assert_eq!(rows.len(), 1);
    assert_eq!(totals.requests, 1);
}

#[tokio::test]
async fn d45_4_request_count_stays_one_with_handed_on() {
    let first = Scripted::stream("first", billed_stream());
    let second = Scripted::ok("second");
    let (app, usage, _dir) = gateway(vec![first, second]);
    let (status, _) = post(
        app,
        r#"{"model":"demo","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, totals) = usage.summary(Window::All.since_now(), 10).expect("summary");
    assert_eq!(totals.requests, 1);
    assert_eq!(totals.input_tokens, 5);
}
