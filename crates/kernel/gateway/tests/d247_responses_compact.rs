//! Unary Responses compaction at the HTTP edge. Loopback and in-process fakes only.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode, header};
use bytes::Bytes;
use hofund_codec::{Codec, OpenAiResponses};
use hofund_gateway::Gateway;
use hofund_ir::Request;
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, MemberAttempt, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

const RELAY_ASK: &str = r#"{"model":"m1","input":[{"type":"compaction","encrypted_content":"gAAAAearlier=="},{"role":"user","content":"hi"}]}"#;
const RELAY_REPLY: &[u8] = br#"{"object":"response.compaction","output":[{"type":"compaction","encrypted_content":"SEAL-RELAY"}]}"#;
const TRIGGER_ASK: &str = r#"{
    "model":"m1",
    "input":[
        {"type":"compaction","encrypted_content":"gAAAAearlier=="},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"old"}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"one"}]},
        {"type":"message","role":"user","content":[{"type":"input_text","text":"two"}]}
    ]
}"#;
const TRIGGER_STREAM: &[u8] = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"created_at\":1}}\n\n",
    "event: response.output_item.done\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"created_at\":1,\"usage\":{\"total_tokens\":150},\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}]}}\n\n",
)
.as_bytes();
const EMPTY_STREAM: &[u8] = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\"}]}}\n\n";

struct Fake {
    id: MemberId,
    route: &'static str,
    protocol: &'static str,
    subscription: bool,
    trigger: bool,
    serves: bool,
    calls: Arc<Mutex<Vec<&'static str>>>,
    streams: Arc<Mutex<Vec<bool>>>,
    reply: &'static [u8],
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.route
    }

    fn protocol(&self) -> &str {
        self.protocol
    }

    fn compacts_by_trigger(&self) -> bool {
        self.trigger
    }

    fn serves_responses(&self, _model: &str) -> bool {
        self.serves || self.protocol == OpenAiResponses.id()
    }

    fn attempt(&self) -> MemberAttempt {
        MemberAttempt {
            subscription: self.subscription,
            protocol: self.protocol.to_owned(),
            ..MemberAttempt::default()
        }
    }

    async fn execute(&self, request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.streams.lock().expect("streams").push(request.stream);
        self.calls.lock().expect("calls").push(self.route);
        Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from_static(
            self.reply,
        ))))
    }
}

struct Spec {
    key: &'static str,
    route: &'static str,
    protocol: &'static str,
    subscription: bool,
    trigger: bool,
    serves: bool,
    reply: &'static [u8],
}

fn member(spec: Spec, calls: &Arc<Mutex<Vec<&'static str>>>) -> Arc<dyn Member> {
    member_streams(spec, calls, &Arc::new(Mutex::new(Vec::new())))
}

fn member_streams(
    spec: Spec,
    calls: &Arc<Mutex<Vec<&'static str>>>,
    streams: &Arc<Mutex<Vec<bool>>>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        id: MemberId::from_preset(
            PresetId::try_from("row").expect("preset"),
            MemberKey::from_static(spec.key),
        ),
        route: spec.route,
        protocol: spec.protocol,
        subscription: spec.subscription,
        trigger: spec.trigger,
        serves: spec.serves,
        calls: Arc::clone(calls),
        streams: Arc::clone(streams),
        reply: spec.reply,
    })
}

fn gateway(hops: Vec<Arc<dyn Member>>) -> (axum::Router, Arc<Ledger>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let router = Router::new(
        BTreeMap::from([("m1".to_owned(), hops)]),
        FailoverPolicy::default(),
    );
    let codecs = HashMap::from([(
        "/v1/responses".to_owned(),
        Arc::new(OpenAiResponses) as Arc<dyn Codec>,
    )]);
    let app = Gateway::new(
        Snapshot::new(router),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .into_router();
    (app, usage, dir)
}

async fn post(app: &axum::Router, uri: &str, body: &str) -> (StatusCode, String) {
    let (status, body, _) = post_typed(app, uri, body).await;
    (status, body)
}

async fn post_typed(app: &axum::Router, uri: &str, body: &str) -> (StatusCode, String, String) {
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri(uri)
                .header(header::AUTHORIZATION, "Bearer secret")
                .header(header::CONTENT_TYPE, "application/json")
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
        .unwrap_or("")
        .to_owned();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        content_type,
    )
}

#[tokio::test]
async fn d247_responses_key_is_relayed_on_both_paths() {
    for path in ["/v1/responses/compact", "/responses/compact"] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let hop = member(
            Spec {
                key: "responses",
                route: "responses-key",
                protocol: OpenAiResponses.id(),
                subscription: false,
                trigger: false,
                serves: true,
                reply: RELAY_REPLY,
            },
            &calls,
        );
        let (app, usage, dir) = gateway(vec![hop]);
        let (status, body) = post(&app, path, RELAY_ASK).await;
        assert_eq!(status, StatusCode::OK, "{path} {body}");
        assert!(body.contains("response.compaction"), "{path} {body}");
        assert!(body.contains("SEAL-RELAY"), "{path} {body}");
        assert_eq!(calls.lock().expect("calls").as_slice(), ["responses-key"]);
        let rows = usage.rows().expect("rows");
        assert_eq!(rows[0].kind, "compact", "{path}");
        assert_eq!(rows[0].status, "200");
        drop(dir);
    }
}

#[tokio::test]
async fn d247_chat_provider_is_refused_and_not_called() {
    for path in ["/v1/responses/compact", "/responses/compact"] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let hop = member(
            Spec {
                key: "chat",
                route: "chat-key",
                protocol: hofund_codec::CHAT_PROTOCOL_ID,
                subscription: false,
                trigger: false,
                serves: false,
                reply: b"{}",
            },
            &calls,
        );
        let (app, _usage, dir) = gateway(vec![hop]);
        let (status, body) = post(&app, path, RELAY_ASK).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path} {body}");
        assert!(body.contains("invalid_request_error"), "{path} {body}");
        assert!(body.contains("m1"), "{path} {body}");
        assert!(
            body.contains("compact on /responses instead"),
            "{path} {body}"
        );
        assert!(calls.lock().expect("calls").is_empty(), "{path}");
        drop(dir);
    }
}

#[tokio::test]
async fn d247_trigger_account_is_gathered_on_both_paths() {
    for path in ["/v1/responses/compact", "/responses/compact"] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let hop = member(
            Spec {
                key: "account",
                route: "account",
                protocol: "subscription",
                subscription: true,
                trigger: true,
                serves: true,
                reply: TRIGGER_STREAM,
            },
            &calls,
        );
        let (app, _usage, dir) = gateway(vec![hop]);
        let (status, body) = post(&app, path, TRIGGER_ASK).await;
        assert_eq!(status, StatusCode::OK, "{path} {body}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["object"], "response.compaction");
        assert_eq!(value["id"], "resp_1");
        assert_eq!(value["usage"]["total_tokens"], 150);
        assert_eq!(value["output"].as_array().expect("output").len(), 3);
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
        assert!(!body.contains("gAAAAearlier=="), "{path} {body}");
        assert!(!body.starts_with("data:"), "{path} {body}");
        assert_eq!(calls.lock().expect("calls").as_slice(), ["account"]);
        drop(dir);
    }
}

#[tokio::test]
async fn d247_trigger_without_a_sealed_item_is_502() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hop = member(
        Spec {
            key: "account",
            route: "account",
            protocol: "subscription",
            subscription: true,
            trigger: true,
            serves: true,
            reply: EMPTY_STREAM,
        },
        &calls,
    );
    let (app, _usage, dir) = gateway(vec![hop]);
    let (status, body) = post(&app, "/v1/responses/compact", TRIGGER_ASK).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("without a compaction item"), "{body}");
    assert!(!body.contains("response.compaction"), "{body}");
    assert_eq!(calls.lock().expect("calls").as_slice(), ["account"]);
    drop(dir);
}

#[tokio::test]
async fn d247_account_without_the_trigger_is_refused() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hop = member(
        Spec {
            key: "plain",
            route: "plain",
            protocol: "subscription",
            subscription: true,
            trigger: false,
            serves: true,
            reply: b"{}",
        },
        &calls,
    );
    let (app, _usage, dir) = gateway(vec![hop]);
    let (status, body) = post(&app, "/responses/compact", RELAY_ASK).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("compact on /responses instead"), "{body}");
    assert!(calls.lock().expect("calls").is_empty());
    drop(dir);
}

#[tokio::test]
async fn d247_later_responses_call_stays_on_the_member_that_sealed() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chat = member(
        Spec {
            key: "chat",
            route: "chat-key",
            protocol: hofund_codec::CHAT_PROTOCOL_ID,
            subscription: false,
            trigger: false,
            serves: false,
            reply: br#"{"ok":true}"#,
        },
        &calls,
    );
    let responses = member(
        Spec {
            key: "responses",
            route: "responses-key",
            protocol: OpenAiResponses.id(),
            subscription: false,
            trigger: false,
            serves: true,
            reply: RELAY_REPLY,
        },
        &calls,
    );
    let (app, _usage, dir) = gateway(vec![chat, responses]);
    let (status, body) = post(&app, "/v1/responses/compact", RELAY_ASK).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let follow = r#"{"model":"m1","input":[{"type":"compaction","encrypted_content":"SEAL-RELAY"},{"role":"user","content":"next"}]}"#;
    let (status, body) = post(&app, "/v1/responses", follow).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        calls.lock().expect("calls").as_slice(),
        ["responses-key", "responses-key"]
    );
    drop(dir);
}

#[tokio::test]
async fn d247_stream_true_is_unary_json_on_both_paths() {
    let ask = r#"{"model":"m1","stream":true,"input":[{"type":"compaction","encrypted_content":"SEAL-RELAY"}]}"#;
    for path in ["/v1/responses/compact", "/responses/compact"] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let streams = Arc::new(Mutex::new(Vec::new()));
        let hop = member_streams(
            Spec {
                key: "responses",
                route: "responses-key",
                protocol: OpenAiResponses.id(),
                subscription: false,
                trigger: false,
                serves: true,
                reply: RELAY_REPLY,
            },
            &calls,
            &streams,
        );
        let (app, _usage, dir) = gateway(vec![hop]);
        let (status, body, content_type) = post_typed(&app, path, ask).await;
        assert_eq!(status, StatusCode::OK, "{path} {body}");
        assert_eq!(content_type, "application/json", "{path} {content_type}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["object"], "response.compaction", "{path}");
        assert_eq!(value["output"][0]["encrypted_content"], "SEAL-RELAY");
        assert!(!body.starts_with("data:"), "{path} {body}");
        assert_eq!(streams.lock().expect("streams").as_slice(), [false]);
        drop(dir);
    }
}

#[tokio::test]
async fn d247_empty_completed_output_is_still_compaction() {
    let upstream = concat!(
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"created_at\":1,\"output\":[]}}\n\n",
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hop = member(
        Spec {
            key: "account",
            route: "account",
            protocol: "subscription",
            subscription: true,
            trigger: true,
            serves: true,
            reply: upstream.as_bytes(),
        },
        &calls,
    );
    let (app, _usage, dir) = gateway(vec![hop]);
    let (status, body, content_type) = post_typed(&app, "/responses/compact", TRIGGER_ASK).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type, "application/json");
    let value: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(value["object"], "response.compaction");
    assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
    drop(dir);
}
