//! Gateway session identity (R-33).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use hofund_codec::{
    Codec, ECHO_SESSION_HEADER, OpenAiChat, OpenAiResponses, SESSION_HEADERS,
    STREAM_CLIENT_PREFIXES, SUBAGENT_HEADER, session_of,
};
use hofund_gateway::Gateway;
use hofund_gateway::session_headers::gateway_session_header_names;
use hofund_ir::{Conversation, Request as ModelRequest};
use hofund_plugin_api::{
    AgentDescription, AgentRoute, BodyStream, MemberId, MemberKey, PluginError, PluginId, PresetId,
    Visible,
};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

const fn fixture_description(id: &'static str) -> AgentDescription {
    AgentDescription::from_static(
        PluginId::from_static(id),
        "Fixture",
        "fixture",
        &[],
        &[],
        None,
        None,
        Visible::All,
        AgentRoute::Gateway,
        &[],
    )
}

#[test]
fn r33_session_of_orders_echo_before_native_headers() {
    let names: Vec<&str> = [ECHO_SESSION_HEADER, SESSION_HEADERS[0], SESSION_HEADERS[1]]
        .into_iter()
        .collect();
    let headers = [
        (ECHO_SESSION_HEADER, "ledger"),
        (SESSION_HEADERS[0], "native"),
    ];
    assert_eq!(session_of(&names, &headers), "ledger");
}

#[test]
fn r33_plugin_session_headers_merge_after_kernel_list() {
    let agent = fixture_description("fixture").with_session_headers(&["x-fixture-session"]);
    let names = gateway_session_header_names(&[&agent]);
    assert!(names.iter().any(|name| name == "x-fixture-session"));
    assert_eq!(names[0], ECHO_SESSION_HEADER);
}

#[test]
fn r33_plugin_extra_session_header_wins_in_session_of() {
    let agent = fixture_description("fixture").with_session_headers(&["x-opencode-session-id"]);
    let names = gateway_session_header_names(&[&agent]);
    let headers = [("x-opencode-session-id", "opencode-only")];
    let pairs = headers.to_vec();
    let names_ref: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(session_of(&names_ref, &pairs), "opencode-only");
}

enum Reply {
    Raw,
    TooLarge,
}

struct FixtureMember {
    member_id: MemberId,
    reply: Reply,
}

#[async_trait]
impl Member for FixtureMember {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(
        &self,
        request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        match self.reply {
            Reply::Raw => {
                let bytes = if request.stream {
                    RAW_CHAT_STREAM
                } else {
                    RAW_CHAT_UNARY
                };
                Ok(Completion::Raw(BodyStream::from_bytes(
                    bytes.as_bytes().to_vec(),
                )))
            }
            Reply::TooLarge => Err(PluginError::status_error(413, "request too large")),
        }
    }
}

fn http_gateway(reply: Reply) -> (axum::Router, tempfile::TempDir) {
    let member = FixtureMember {
        member_id: MemberId::from_preset(
            PresetId::try_from("primary").expect("preset"),
            MemberKey::from_static("row"),
        ),
        reply,
    };
    let routes = BTreeMap::from([("demo".to_owned(), vec![Arc::new(member) as Arc<dyn Member>])]);
    gateway_with_router(Router::new(routes, FailoverPolicy::default()))
}

fn gateway_with_router(router: Router) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let codecs: HashMap<String, Arc<dyn Codec>> = HashMap::from([
        (
            "/v1/chat/completions".to_owned(),
            Arc::new(OpenAiChat) as Arc<dyn Codec>,
        ),
        ("/v1/responses".to_owned(), Arc::new(OpenAiResponses)),
    ]);
    let app = Gateway::new(Snapshot::new(router), "secret".to_owned(), codecs, usage).into_router();
    (app, dir)
}

async fn post(
    app: axum::Router,
    path: &str,
    body: &'static str,
    session_id: Option<&str>,
    compacting: bool,
) -> axum::http::Response<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", "Bearer secret");
    if let Some(session_id) = session_id {
        request = request.header(ECHO_SESSION_HEADER, session_id);
    }
    if compacting {
        request = request.header(
            "user-agent",
            format!("{}_cli/1.0", STREAM_CLIENT_PREFIXES[0]),
        );
    }
    app.oneshot(request.body(Body::from(body)).expect("request"))
        .await
        .expect("response")
}

const RAW_CHAT_UNARY: &str = r#"{"id":"c","object":"chat.completion","created":0,"model":"upstream","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#;
const RAW_CHAT_STREAM: &str = concat!(
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":0,",
    "\"model\":\"upstream\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
    "\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
    "data: [DONE]\n\n"
);
const CHAT_UNARY_REQUEST: &str = r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#;
const CHAT_STREAM_REQUEST: &str =
    r#"{"model":"demo","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
const RESPONSES_STREAM_REQUEST: &str = r#"{"model":"demo","stream":true,"input":"hi"}"#;

#[tokio::test]
async fn r33_raw_http_responses_echo_named_session() {
    let (app, _dir) = http_gateway(Reply::Raw);
    for body in [CHAT_UNARY_REQUEST, CHAT_STREAM_REQUEST] {
        let response = post(
            app.clone(),
            "/v1/chat/completions",
            body,
            Some("raw-session"),
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(ECHO_SESSION_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("raw-session")
        );
    }
    let response = post(app, "/v1/chat/completions", CHAT_UNARY_REQUEST, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(ECHO_SESSION_HEADER));
}

struct ConversationMember {
    member_id: MemberId,
    seen: Arc<std::sync::Mutex<Option<Conversation>>>,
}

#[async_trait]
impl Member for ConversationMember {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(
        &self,
        request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        *self.seen.lock().expect("conversation") = request.conversation;
        Ok(Completion::Raw(BodyStream::from_bytes(
            RAW_CHAT_UNARY.as_bytes().to_vec(),
        )))
    }
}

#[tokio::test]
async fn r33_native_session_and_subagent_reach_routed_request() {
    let seen = Arc::new(std::sync::Mutex::new(None));
    let member = ConversationMember {
        member_id: MemberId::from_preset(
            PresetId::try_from("primary").expect("preset"),
            MemberKey::from_static("row"),
        ),
        seen: Arc::clone(&seen),
    };
    let routes = BTreeMap::from([("demo".to_owned(), vec![Arc::new(member) as Arc<dyn Member>])]);
    let (app, _dir) = gateway_with_router(Router::new(routes, FailoverPolicy::default()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer secret")
                .header("x-session-id", "session-1")
                .header(SUBAGENT_HEADER, "agent-1")
                .body(Body::from(CHAT_UNARY_REQUEST))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let conversation = seen.lock().expect("conversation").clone();
    assert_eq!(
        conversation,
        Some(Conversation::new("session-1", "agent-1"))
    );
}

#[tokio::test]
async fn r33_compacting_stream_error_echoes_named_session() {
    let (app, _dir) = http_gateway(Reply::TooLarge);
    let response = post(
        app,
        "/v1/responses",
        RESPONSES_STREAM_REQUEST,
        Some("compacting-session"),
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(ECHO_SESSION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("compacting-session")
    );
}

struct HeldMember {
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for HeldMember {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        if self.hits.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(PluginError::status_error(503, "busy"))
        } else {
            Ok(Completion::Raw(BodyStream::from_bytes(
                RAW_CHAT_STREAM.as_bytes().to_vec(),
            )))
        }
    }
}

fn held_gateway() -> (axum::Router, tempfile::TempDir, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let member = Arc::new(HeldMember {
        member_id: MemberId::from_preset(
            PresetId::try_from("primary").expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(&hits),
    });
    let mut hop = hofund_router::Hop::new(member);
    hop.patience = Some(Duration::from_secs(60));
    let router = Router::from_hops(
        BTreeMap::from([("demo".to_owned(), vec![hop])]),
        FailoverPolicy::default(),
    );
    let (app, dir) = gateway_with_router(router);
    (app, dir, hits)
}

#[tokio::test(start_paused = true)]
async fn r33_held_stream_http_response_echoes_named_session() {
    for session_id in ["held-session", ""] {
        let (app, _dir, hits) = held_gateway();
        let response = post(
            app,
            "/v1/chat/completions",
            CHAT_STREAM_REQUEST,
            Some(session_id),
            false,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            response
                .headers()
                .get(ECHO_SESSION_HEADER)
                .and_then(|value| value.to_str().ok()),
            (!session_id.is_empty()).then_some(session_id)
        );
        let first = response
            .into_body()
            .into_data_stream()
            .next()
            .await
            .expect("keepalive")
            .expect("chunk");
        assert_eq!(first.as_ref(), b": keepalive\n\n");
    }
}

#[tokio::test]
async fn r33_spend_cap_http_response_echoes_named_session() {
    let member = FixtureMember {
        member_id: MemberId::from_preset(
            PresetId::try_from("primary").expect("preset"),
            MemberKey::from_static("row"),
        ),
        reply: Reply::Raw,
    };
    let routes = BTreeMap::from([("demo".to_owned(), vec![Arc::new(member) as Arc<dyn Member>])]);
    let router = Router::new(routes, FailoverPolicy::default())
        .with_member_skip(Arc::new(|_, _, _, _, _| true))
        .with_spend_cap_exhausted(Arc::new(|_| Some(Some(120))));
    let (app, _dir) = gateway_with_router(router);
    let response = post(
        app,
        "/v1/chat/completions",
        CHAT_UNARY_REQUEST,
        Some("spend-cap-session"),
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(ECHO_SESSION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("spend-cap-session")
    );
}
