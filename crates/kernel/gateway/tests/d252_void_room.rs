//! HTTP-edge behavior for Chat length cuts without visible output.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode, header};
use bytes::Bytes;
use hofund_codec::{CHAT_PROTOCOL_ID, OpenAiChat};
use hofund_gateway::Gateway;
use hofund_ir::Request;
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

const VOID: &[u8] = concat!(
    "data: {\"id\":\"cut\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"thinking\"}}]}\n\n",
    "data: {\"id\":\"cut\",\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"completion_tokens\":8192,\"completion_tokens_details\":{\"reasoning_tokens\":8192}}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const TEXT: &[u8] = concat!(
    "data: {\"id\":\"reply\",\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
    "data: {\"id\":\"reply\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();

type Limits = Arc<Mutex<Vec<Option<u64>>>>;

struct Fake {
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    limits: Limits,
    replies: Vec<&'static [u8]>,
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

    async fn execute(&self, request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.limits
            .lock()
            .expect("limits")
            .push(request.params.max_tokens);
        let hit = self.hits.fetch_add(1, Ordering::SeqCst);
        let bytes = self.replies[hit.min(self.replies.len() - 1)];
        Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from_static(
            bytes,
        ))))
    }
}

fn app(replies: Vec<&'static [u8]>) -> (axum::Router, Arc<AtomicUsize>, Limits, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let hits = Arc::new(AtomicUsize::new(0));
    let limits = Arc::new(Mutex::new(Vec::new()));
    let member = Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::from_static("only"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(&hits),
        limits: Arc::clone(&limits),
        replies,
    });
    let router = Router::new(
        BTreeMap::from([("demo".to_owned(), vec![member as Arc<dyn Member>])]),
        FailoverPolicy::default(),
    );
    let codecs = HashMap::from([(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn hofund_codec::Codec>,
    )]);
    let app = Gateway::new(Snapshot::new(router), "secret".to_owned(), codecs, usage).into_router();
    (app, hits, limits, dir)
}

async fn post(app: axum::Router, body: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, "Bearer secret")
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
async fn d252_http_default_void_reasks_and_later_request_uses_room() {
    let (app, hits, limits, dir) = app(vec![VOID, TEXT, TEXT]);
    let ask = r#"{"model":"demo","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, body) = post(app.clone(), ask).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("done"), "{body}");
    assert!(!body.contains("cut at its length"), "{body}");
    assert_eq!(
        limits.lock().expect("limits").as_slice(),
        [None, Some(32_768)]
    );

    let (status, body) = post(app, ask).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("done"), "{body}");
    assert_eq!(
        limits.lock().expect("limits").as_slice(),
        [None, Some(32_768), Some(32_768)]
    );
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    drop(dir);
}

#[tokio::test]
async fn d252_http_client_named_void_is_one_error_without_length_finish() {
    let (app, hits, limits, dir) = app(vec![VOID]);
    let ask = r#"{"model":"demo","stream":true,"max_tokens":8192,"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, body) = post(app, ask).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.starts_with("data: {\"error\":"), "{body}");
    assert!(body.contains("cut at its length"), "{body}");
    assert!(body.contains("request's limit was 8192"), "{body}");
    assert!(!body.contains("\"finish_reason\":\"length\""), "{body}");
    assert_eq!(body.matches("[DONE]").count(), 1, "{body}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(limits.lock().expect("limits").as_slice(), [Some(8_192)]);
    drop(dir);
}
