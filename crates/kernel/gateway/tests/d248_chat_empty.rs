//! Chat empty-reply behavior at the HTTP edge, with local fakes only.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode, header};
use bytes::Bytes;
use futures_util::stream;
use hofund_codec::{AnthropicMessages, CHAT_PROTOCOL_ID, Codec, OpenAiChat, OpenAiResponses};
use hofund_gateway::Gateway;
use hofund_ir::{
    Extensions, Message, MessageDelta, MessageStop, Request, Response, Role, StreamEvent, Usage,
};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

const VOID: &[u8] = concat!(
    "data: {\"id\":\"void\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":null,\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"void\",\"model\":\"m\",\"choices\":[],\"usage\":{\"completion_tokens\":2359,\"prompt_tokens\":91000,\"reasoning_tokens\":2351}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const VOID_WITHOUT_OUTPUT: &[u8] = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"completion_tokens\":0,\"prompt_tokens\":9}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const TEXT_REPLY: &[u8] = concat!(
    "data: {\"id\":\"reply\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
    "data: {\"id\":\"reply\",\"model\":\"m\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const COUNTED_DETAIL: &str = "the model reasoned for 2351 of the 2359 output tokens it counted";

#[derive(Clone)]
enum Reply {
    Raw(&'static [u8]),
    EmptyEvents,
    EmptyUnary,
}

struct Fake {
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    replies: Vec<Reply>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "only"
    }

    fn protocol(&self) -> &str {
        CHAT_PROTOCOL_ID
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        let hit = self.hits.fetch_add(1, Ordering::SeqCst);
        match &self.replies[hit.min(self.replies.len() - 1)] {
            Reply::Raw(bytes) => Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from_static(
                bytes,
            )))),
            Reply::EmptyEvents => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(StreamEvent::MessageDelta(MessageDelta {
                    stop: None,
                    usage: Some(counted_usage()),
                    extensions: Extensions::new(),
                })),
                Ok(StreamEvent::MessageStop(MessageStop::default())),
            ])))),
            Reply::EmptyUnary => Ok(Completion::Unary(Box::new(
                Response::new(Message::new(Role::Assistant)).with_usage(Some(counted_usage())),
            ))),
        }
    }
}

fn counted_usage() -> Usage {
    Usage {
        output_tokens: Some(2359),
        reasoning_tokens: Some(2351),
        ..Usage::default()
    }
}

fn app(
    replies: Vec<Reply>,
) -> (
    axum::Router,
    Arc<AtomicUsize>,
    Arc<Ledger>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hits = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::try_from("only").expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(&hits),
        replies,
    });
    let router = Router::new(
        BTreeMap::from([("demo".to_owned(), vec![member as Arc<dyn Member>])]),
        FailoverPolicy::default(),
    );
    let codecs = HashMap::from([
        (
            "/v1/chat/completions".to_owned(),
            Arc::new(OpenAiChat) as Arc<dyn Codec>,
        ),
        (
            "/v1/messages".to_owned(),
            Arc::new(AnthropicMessages) as Arc<dyn Codec>,
        ),
        (
            "/v1/responses".to_owned(),
            Arc::new(OpenAiResponses) as Arc<dyn Codec>,
        ),
    ]);
    let app = Gateway::new(
        Snapshot::new(router),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .into_router();
    (app, hits, usage, dir)
}

async fn post(app: axum::Router, uri: &str, body: &str) -> (StatusCode, String, String) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri(uri)
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::from(body.to_owned()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

const CHAT_STREAM: &str =
    r#"{"model":"demo","messages":[{"role":"user","content":"hi"}],"stream":true}"#;

#[tokio::test]
async fn d248_chat_void_is_an_sse_error_with_one_done_and_counted_usage() {
    let (app, hits, usage, dir) = app(vec![Reply::Raw(VOID)]);
    let (status, content_type, body) = post(app, "/v1/chat/completions", CHAT_STREAM).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type, "text/event-stream");
    assert!(body.starts_with("data: {\"error\":"), "{body}");
    assert!(body.contains(COUNTED_DETAIL), "{body}");
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    assert_eq!(body.matches("[DONE]").count(), 1, "{body}");
    assert!(!body.contains("\"id\":\"void\""), "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert_eq!(usage.rows().expect("rows")[0].status, "502");
    drop(dir);
}

#[tokio::test]
async fn d248_chat_retry_returns_only_the_eventual_reply() {
    let (app, hits, _usage, dir) = app(vec![Reply::Raw(VOID), Reply::Raw(TEXT_REPLY)]);
    let (status, _, body) = post(app, "/v1/chat/completions", CHAT_STREAM).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_bytes(), TEXT_REPLY);
    assert!(!body.contains("an empty reply"), "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    drop(dir);
}

#[tokio::test]
async fn d248_translated_void_is_an_empty_stream_on_every_client_route() {
    for (uri, ask) in [
        (
            "/v1/messages",
            r#"{"model":"demo","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        ),
        (
            "/v1/responses",
            r#"{"model":"demo","stream":true,"input":"hi"}"#,
        ),
    ] {
        let (app, hits, _usage, dir) = app(vec![Reply::EmptyEvents]);
        let (status, content_type, body) = post(app, uri, ask).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{uri}: {body}");
        assert_ne!(content_type, "text/event-stream", "{uri}");
        assert!(body.contains("an empty reply"), "{uri}: {body}");
        assert!(body.contains(COUNTED_DETAIL), "{uri}: {body}");
        assert!(!body.contains("[DONE]"), "{uri}: {body}");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "{uri}");
        drop(dir);
    }
}

#[tokio::test]
async fn d248_translated_whole_void_is_bad_gateway_with_counted_usage() {
    let (app, hits, _usage, dir) = app(vec![Reply::EmptyUnary]);
    let (status, _, body) = post(
        app,
        "/v1/messages",
        r#"{"model":"demo","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("an empty reply"), "{body}");
    assert!(body.contains(COUNTED_DETAIL), "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    drop(dir);
}

#[tokio::test]
async fn d248_void_without_output_keeps_the_base_error() {
    let (app, _hits, _usage, dir) = app(vec![Reply::Raw(VOID_WITHOUT_OUTPUT)]);
    let (_, _, body) = post(app, "/v1/chat/completions", CHAT_STREAM).await;
    assert!(body.contains("an empty reply"), "{body}");
    assert!(!body.contains("output tokens"), "{body}");
    assert!(!body.contains("compact the conversation"), "{body}");
    drop(dir);
}
