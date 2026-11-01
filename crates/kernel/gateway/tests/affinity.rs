//! Conversation affinity through the HTTP edge.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::Gateway;
use hofund_ir::{ContentPart, Message, Request as ModelRequest, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::affinity::Stays;
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Route, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Counted {
    id: MemberId,
    route_id: &'static str,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Counted {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound_protocol: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn counted(id: &'static str, hits: &Arc<AtomicUsize>) -> Arc<dyn Member> {
    Arc::new(Counted {
        id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        route_id: id,
        hits: Arc::clone(hits),
    })
}

async fn post(app: axum::Router, session: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer secret")
        .header("x-session-id", session)
        .body(Body::from(
            r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .expect("request");
    app.oneshot(request).await.expect("response").status()
}

#[tokio::test]
async fn c2h_gateway_keeps_a_session_on_its_answerer_under_rotation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let route = Route::new(vec![
        Hop::new(counted("first", &first_hits)),
        Hop::new(counted("second", &second_hits)),
    ])
    .with_routing("rotate")
    .with_stays(Stays::Session);
    let router = Router::from_routes(
        BTreeMap::from([("demo".to_owned(), route)]),
        FailoverPolicy::default(),
    );
    let codecs = HashMap::from([(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    )]);
    let app = Gateway::new(Snapshot::new(router), "secret".to_owned(), codecs, usage).into_router();

    for _ in 0..3 {
        assert_eq!(post(app.clone(), "s-1").await, StatusCode::OK);
    }
    let first = first_hits.load(Ordering::SeqCst);
    let second = second_hits.load(Ordering::SeqCst);
    assert_eq!(first + second, 3);
    assert!(first == 3 || second == 3, "split {first}/{second}");
}
