//! Time-to-first-content is stored on gateway usage rows (R-10).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::stream;
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::Gateway;
use hofund_ir::{
    BlockStart, ContentBlockStart, ContentPart, Extensions, Message, Request as ModelRequest,
    Response, Role, StreamEvent, TextDelta, ToolArgumentsDelta,
};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::{Ledger, row_tokens_per_second};
use tokio::time::sleep;
use tower::ServiceExt;

struct Reply {
    id: MemberId,
    stream: bool,
    delay: Duration,
    mode: ReplyMode,
}

#[derive(Clone, Copy)]
enum ReplyMode {
    Text,
    ToolBurst,
}

#[async_trait]
impl Member for Reply {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        sleep(self.delay).await;
        if self.stream {
            let events = match self.mode {
                ReplyMode::Text => vec![Ok(StreamEvent::TextDelta(TextDelta {
                    index: 0,
                    text: "ok".to_owned(),
                    extensions: Extensions::default(),
                }))],
                ReplyMode::ToolBurst => vec![
                    Ok(StreamEvent::ContentBlockStart(ContentBlockStart {
                        index: 2,
                        block: BlockStart::ToolCall {
                            id: "call".to_owned(),
                            name: "demo".to_owned(),
                        },
                        extensions: Extensions::default(),
                    })),
                    Ok(StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                        index: 2,
                        partial_json: r#"{"x":1}"#.to_owned(),
                        extensions: Extensions::default(),
                    })),
                ],
            };
            return Ok(Completion::Stream(Box::pin(stream::iter(events))));
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn app(
    stream: bool,
    delay: Duration,
    mode: ReplyMode,
) -> (axum::Router, Arc<Ledger>, tempfile::TempDir) {
    app_with(stream, delay, mode, false)
}

fn member(key: &'static str, stream: bool, delay: Duration, mode: ReplyMode) -> Arc<dyn Member> {
    Arc::new(Reply {
        id: MemberId::from_preset(
            PresetId::from_static("primary"),
            MemberKey::from_static(key),
        ),
        stream,
        delay,
        mode,
    })
}

fn app_with(
    stream: bool,
    delay: Duration,
    mode: ReplyMode,
    fallback: bool,
) -> (axum::Router, Arc<Ledger>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut routes = BTreeMap::new();
    let mut members = vec![member("row", stream, delay, mode)];
    if fallback {
        members.push(member("spare", stream, delay, mode));
    }
    routes.insert("demo".to_owned(), members);
    let codecs = HashMap::from([(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    )]);
    let router = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .into_router();
    (router, usage, dir)
}

async fn chat(app: axum::Router, stream: bool) -> StatusCode {
    let body = if stream {
        r#"{"model":"demo","stream":true,"messages":[{"role":"user","content":"hi"}]}"#
    } else {
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#
    };
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer secret")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    status
}

#[tokio::test]
async fn r10_gateway_records_ttft_for_non_streamed_chat() {
    let (app, usage, _dir) = app(false, Duration::from_millis(40), ReplyMode::Text);
    assert_eq!(chat(app, false).await, StatusCode::OK);
    let row = usage.rows().expect("rows").pop().expect("one row");
    assert!(row.ttft_ms.is_none());
    assert!(row.flow_ms.is_none());
}

#[tokio::test]
async fn r10_gateway_records_ttft_before_streamed_body_finishes() {
    let (app, usage, _dir) = app(true, Duration::from_millis(50), ReplyMode::Text);
    assert_eq!(chat(app, true).await, StatusCode::OK);
    let row = usage.rows().expect("rows").pop().expect("one row");
    let ttft = row.ttft_ms.expect("ttft");
    assert!(ttft >= 40, "ttft={ttft} latency={}", row.latency_ms);
    assert!(row.latency_ms >= ttft);
}

#[tokio::test]
async fn r10_gateway_single_chunk_stream_stores_flow_without_speed() {
    let (app, usage, _dir) = app(true, Duration::from_millis(20), ReplyMode::Text);
    assert_eq!(chat(app, true).await, StatusCode::OK);
    let row = usage.rows().expect("rows").pop().expect("one row");
    assert!(row.flow_ms.is_some_and(|flow| flow > 0));
    assert!(row_tokens_per_second(&row).is_none());
}

#[tokio::test]
async fn r10_gateway_tool_burst_stream_stores_flow_without_speed() {
    let (app, usage, _dir) = app(true, Duration::from_millis(20), ReplyMode::ToolBurst);
    assert_eq!(chat(app, true).await, StatusCode::OK);
    let row = usage.rows().expect("rows").pop().expect("one row");
    assert!(
        row.flow_ms.is_some_and(|flow| flow >= 1),
        "flow_ms={:?}",
        row.flow_ms
    );
    assert!(row_tokens_per_second(&row).is_none());
}

#[tokio::test]
async fn r10_gateway_records_ttft_when_a_fallback_member_is_held() {
    let (app, usage, _dir) = app_with(true, Duration::from_millis(50), ReplyMode::Text, true);
    assert_eq!(chat(app, true).await, StatusCode::OK);
    let row = usage.rows().expect("rows").pop().expect("one row");
    let ttft = row.ttft_ms.expect("ttft on the handover stream");
    assert!(ttft >= 40, "ttft={ttft} latency={}", row.latency_ms);
    assert!(
        row.flow_ms.is_some_and(|flow| flow > 0),
        "flow_ms={:?}",
        row.flow_ms
    );
}
