//! Input counts: an upstream number, or an estimate marked as one.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{AnthropicMessages, Codec, GenerativeLanguage};
use hofund_gateway::{GENERATIVE_UNARY, Gateway};
use hofund_ir::Request as ModelRequest;
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Counter {
    member_id: MemberId,
    tokens: Option<u64>,
}

#[async_trait]
impl Member for Counter {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn count_input(
        &self,
        _request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<CountOutcome, PluginError> {
        match self.tokens {
            Some(tokens) => Ok(CountOutcome::Counted(tokens)),
            None => Ok(CountOutcome::Unsupported),
        }
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        Err(PluginError::status_error(500, "unused"))
    }
}

fn app(tokens: Option<u64>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![Arc::new(Counter {
            member_id: MemberId::from_preset(
                PresetId::from_static("primary"),
                MemberKey::from_static("row"),
            ),
            tokens,
        }) as Arc<dyn Member>],
    );
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/messages".to_owned(),
        Arc::new(AnthropicMessages) as Arc<dyn Codec>,
    );
    codecs.insert(GENERATIVE_UNARY.to_owned(), Arc::new(GenerativeLanguage));
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    )
    .into_router();
    (app, dir)
}

async fn post(
    (app, _dir): (axum::Router, tempfile::TempDir),
    uri: &str,
    body: &str,
) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", "Bearer secret")
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
async fn messages_count_returns_the_upstream_number() {
    let (status, body) = post(
        app(Some(17)),
        "/v1/messages/count_tokens",
        r#"{"model":"demo","max_tokens":8,"messages":[{"role":"user","content":"abcd"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"input_tokens":17,"estimated":false}"#);
}

#[tokio::test]
async fn a_missing_count_endpoint_is_marked_as_an_estimate() {
    let (status, body) = post(
        app(None),
        "/v1beta/models/demo:countTokens",
        r#"{"contents":[{"role":"user","parts":[{"text":"abcd"}]}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"totalTokens":1,"estimated":true}"#);
}
