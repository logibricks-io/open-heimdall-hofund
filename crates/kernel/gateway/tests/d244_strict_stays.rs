//! Strict stays through the HTTP edge: a count and a fork stay on the
//! session's account, and an unanswered subagent follows its lead.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{AnthropicMessages, Codec, OpenAiChat, SUBAGENT_HEADER};
use hofund_gateway::Gateway;
use hofund_ir::{ContentPart, Message, Response, Role};
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::affinity::Stays;
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Scripted {
    id: MemberId,
    route_id: &'static str,
    tokens: u64,
    answers: Mutex<VecDeque<Result<Completion, PluginError>>>,
}

#[async_trait]
impl Member for Scripted {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn count_input(
        &self,
        _request: hofund_ir::Request,
        _inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        Ok(CountOutcome::Counted(self.tokens))
    }

    async fn execute(
        &self,
        _request: hofund_ir::Request,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.answers
            .lock()
            .expect("answers")
            .pop_front()
            .expect("scripted answer")
    }
}

fn scripted(
    id: &'static str,
    tokens: u64,
    answers: Vec<Result<Completion, PluginError>>,
) -> Arc<dyn Member> {
    Arc::new(Scripted {
        id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id: id,
        tokens,
        answers: Mutex::new(answers.into_iter().collect()),
    })
}

fn text_answer(label: &'static str) -> Result<Completion, PluginError> {
    let mut message = Message::new(Role::Assistant);
    message.content.push(ContentPart::text(label));
    Ok(Completion::Unary(Box::new(Response::new(message))))
}

fn app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let first = scripted(
        "first",
        11,
        vec![
            Err(PluginError::status_error(429, "limited")),
            text_answer("from-first"),
        ],
    );
    let second = scripted(
        "second",
        22,
        vec![
            text_answer("from-second"),
            text_answer("from-second"),
            text_answer("from-second"),
            text_answer("from-second"),
        ],
    );
    let route = Route::new(vec![Hop::new(first), Hop::new(second)])
        .with_routing("order")
        .with_stays(Stays::Strict);
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
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
    ]);
    let gateway = Gateway::new(Snapshot::new(router), "secret".to_owned(), codecs, usage);
    (gateway.into_router(), dir)
}

async fn send(
    app: &axum::Router,
    uri: &str,
    session: &str,
    agent: Option<&str>,
    body: &str,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", "Bearer secret")
        .header("x-session-id", session);
    if let Some(agent) = agent {
        builder = builder.header(SUBAGENT_HEADER, agent);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_owned())).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn d244_strict_count_and_fork_stay_on_the_session_account() {
    let (app, _dir) = app();
    let (status, body) = send(
        &app,
        "/v1/chat/completions",
        "parent-1",
        None,
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from-second"), "{body}");

    let parent = r#"{"model":"demo","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"from parent"},{"role":"user","content":"more"}]}"#;
    let (status, body) = send(&app, "/v1/chat/completions", "parent-1", None, parent).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from-second"), "{body}");

    let (status, body) = send(
        &app,
        "/v1/messages/count_tokens",
        "parent-1",
        None,
        r#"{"model":"demo","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"input_tokens":22,"estimated":false}"#);

    let fork = r#"{"model":"demo","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"from parent"},{"role":"user","content":"in the branch"}]}"#;
    let (status, body) = send(&app, "/v1/chat/completions", "fork-1", None, fork).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from-second"), "{body}");

    let (status, body) = send(
        &app,
        "/v1/chat/completions",
        "parent-1",
        Some("agent-1"),
        r#"{"model":"demo","messages":[{"role":"user","content":"task"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from-second"), "{body}");

    let (status, body) = send(
        &app,
        "/v1/chat/completions",
        "other-1",
        None,
        r#"{"model":"demo","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"from other"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from-first"), "{body}");

    let (status, body) = send(
        &app,
        "/v1/messages/count_tokens",
        "other-1",
        None,
        r#"{"model":"demo","max_tokens":8,"messages":[{"role":"user","content":"other"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, r#"{"input_tokens":11,"estimated":false}"#);
}
