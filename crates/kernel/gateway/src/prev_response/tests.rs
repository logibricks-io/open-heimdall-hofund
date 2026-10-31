use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};
use hofund_ir::{Message, Request as ModelRequest, Response, Role};
use hofund_plugin_api::{
    BodyStream, Completion, MemberId, MemberKey, PluginError, PresetId, ProtocolId,
};
use hofund_router::{FailoverPolicy, Member, MemberAttempt, Router, Snapshot};
use hofund_usage::Ledger;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, VecDeque};
use tower::ServiceExt;

const RESPONSES: &str = ProtocolId::OPENAI_RESPONSES.as_str();
const CHAT: &str = ProtocolId::OPENAI_CHAT.as_str();

use super::*;
use crate::Gateway;

enum Reply {
    Text(&'static str),
    Raw(Vec<u8>),
    Fail(u16),
}

struct FakeMember {
    member_id: MemberId,
    protocol: &'static str,
    keeps: bool,
    subscription: bool,
    replies: Mutex<VecDeque<Reply>>,
    seen: Mutex<Vec<Value>>,
}

impl FakeMember {
    fn new(protocol: &'static str, keeps: bool, replies: Vec<Reply>) -> Self {
        Self {
            member_id: MemberId::from_preset(
                PresetId::from_static("fake"),
                MemberKey::from_static("default"),
            ),
            protocol,
            keeps,
            subscription: false,
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn subscribed(mut self) -> Self {
        self.subscription = true;
        self
    }

    fn seen(&self) -> Vec<Value> {
        self.seen.lock().expect("seen").clone()
    }
}

#[async_trait]
impl Member for FakeMember {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "fake"
    }

    fn protocol(&self) -> &str {
        self.protocol
    }

    fn attempt(&self) -> MemberAttempt {
        MemberAttempt {
            provider_id: "fake".to_owned(),
            protocol: self.protocol.to_owned(),
            base_url: "http://upstream.test/v1".to_owned(),
            route: "fake\x000123456789abcdef".to_owned(),
            keeps_responses: self.keeps,
            subscription: self.subscription,
            plugin: false,
        }
    }

    async fn execute(
        &self,
        request: ModelRequest,
        inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.execute_with_attempt(request, inbound, None).await
    }

    async fn execute_with_attempt(
        &self,
        request: ModelRequest,
        inbound: &str,
        hook: Option<Arc<dyn AttemptHook>>,
    ) -> Result<Completion, PluginError> {
        let responses = self.protocol == RESPONSES;
        let whole = if responses {
            let encoded = OpenAiResponses
                .encode_request(&request)
                .map_err(|error| PluginError::decode(error.to_string()))?;
            OpenAiResponses::prepare_upstream_body(&encoded)
                .map_err(|error| PluginError::decode(error.to_string()))?
        } else {
            OpenAiChat
                .encode_request(&request)
                .map_err(|error| PluginError::decode(error.to_string()))?
        };
        let info = Attempt {
            member: self.attempt(),
            inbound_protocol: inbound.to_owned(),
        };
        let shortened = hook.as_ref().and_then(|hook| hook.prepare(&info, &whole));
        let delta = shortened.is_some();
        let body = shortened.unwrap_or_else(|| whole.clone());
        self.seen
            .lock()
            .expect("seen")
            .push(serde_json::from_slice(&body).expect("upstream JSON"));
        let mut reply = self
            .replies
            .lock()
            .expect("replies")
            .pop_front()
            .expect("scripted reply");
        if let Reply::Fail(status) = reply
            && delta
            && hook.as_ref().is_some_and(|hook| {
                let text = format!("previous_response_not_found ({status})");
                hook.refused(&info, status, text.as_bytes())
            })
        {
            self.seen
                .lock()
                .expect("seen")
                .push(serde_json::from_slice(&whole).expect("whole JSON"));
            reply = self
                .replies
                .lock()
                .expect("replies")
                .pop_front()
                .expect("retry reply");
        }
        match reply {
            Reply::Text(text) => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(hofund_ir::ContentPart::text(text));
                Ok(Completion::Unary(Box::new(
                    Response::new(message)
                        .with_id(Some(format!(
                            "resp_{}",
                            self.seen.lock().expect("seen").len()
                        )))
                        .with_model(Some("demo".to_owned())),
                )))
            }
            Reply::Raw(bytes) => Ok(Completion::Raw(BodyStream::from_bytes(bytes))),
            Reply::Fail(status) => Err(PluginError::status_error(status, "upstream failed")),
        }
    }
}

fn raw_response(id: &str, text: &str, status: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": id,
        "object": "response",
        "model": "demo",
        "status": status,
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        }],
    }))
    .expect("response")
}

fn gateway(member: Arc<FakeMember>) -> (axum::Router, PrevStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let routes = BTreeMap::from([(
        "demo".to_owned(),
        vec![Arc::clone(&member) as Arc<dyn Member>],
    )]);
    let codecs = HashMap::from([
        (
            "/v1/responses".to_owned(),
            Arc::new(OpenAiResponses) as Arc<dyn Codec>,
        ),
        (
            "/v1/chat/completions".to_owned(),
            Arc::new(OpenAiChat) as Arc<dyn Codec>,
        ),
    ]);
    let gateway = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    );
    let store = gateway.inner.prev.clone();
    (gateway.into_router(), store, dir)
}

async fn post(app: axum::Router, path: &str, body: &str) -> (StatusCode, Vec<u8>) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
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
    (status, bytes.to_vec())
}

fn id_of(bytes: &[u8]) -> String {
    serde_json::from_slice::<Value>(bytes)
        .expect("response JSON")
        .get("id")
        .and_then(Value::as_str)
        .expect("response id")
        .to_owned()
}

fn capture(store: &PrevStore, scope: &str, request: &str, reply: &[u8]) {
    let expansion = store.expand(scope, request.as_bytes());
    let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
    capture.push(reply);
    capture.finish();
}

fn keep(store: &PrevStore, scope: &str, request: &str, reply: &[u8]) -> String {
    let id = parse_reply(reply).expect("reply").0;
    capture(store, scope, request, reply);
    id
}

fn turn(own: usize) -> Turn {
    Turn {
        items: Vec::new().into(),
        own,
        at: Instant::now(),
        upstream_id: String::new(),
        route: String::new(),
    }
}

#[tokio::test]
async fn a1_other_protocols_pass_through_unchanged() {
    let member = Arc::new(FakeMember::new(CHAT, false, vec![Reply::Text("ok")]));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (status, _) = post(
        app,
        "/v1/chat/completions",
        r#"{"model":"demo","previous_response_id":"resp_unknown","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member.seen()[0]["previous_response_id"], "resp_unknown");
}

#[tokio::test]
async fn a2_unary_and_sse_replies_keep_input_and_streamed_output() {
    let store = PrevStore::new();
    let unary = raw_response("resp_unary", "unary answer", "completed");
    keep(
        &store,
        "s",
        r#"{"model":"demo","input":"question"}"#,
        &unary,
    );
    let expanded = store.expand(
        "s",
        br#"{"model":"demo","previous_response_id":"resp_unary","input":"next"}"#,
    );
    let body: Value = serde_json::from_slice(&expanded.body).expect("expanded");
    let items = body["input"].as_array().expect("items");
    assert_eq!(items.len(), 3);
    assert!(
        items
            .iter()
            .any(|item| item.to_string().contains("unary answer"))
    );

    let sse = concat!(
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"streamed\"}]}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_sse\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"stale\"}]}]}}\n\n",
    );
    keep(
        &store,
        "s",
        r#"{"model":"demo","input":"sse question"}"#,
        sse.as_bytes(),
    );
    let expanded = store.expand(
        "s",
        br#"{"model":"demo","previous_response_id":"resp_sse","input":"next"}"#,
    );
    let text = String::from_utf8(expanded.body).expect("UTF-8");
    assert!(text.contains("streamed"));
    assert!(!text.contains("stale"));
}

#[test]
fn a3_string_input_becomes_one_item_and_non_list_input_is_untouched() {
    let store = PrevStore::new();
    let expansion = store.expand("s", br#"{"model":"demo","input":"hello"}"#);
    let capture = expansion.capture.expect("capture");
    assert_eq!(capture.input.len(), 1);
    assert_eq!(capture.input[0]["content"], "hello");
    assert_eq!(capture.input[0]["role"], "user");

    let original = br#"{"model":"demo","input":{"not":"a list"}}"#;
    let expansion = store.expand("s", original);
    assert_eq!(expansion.body, original);
    assert!(expansion.ask.is_none());
    assert!(expansion.capture.is_none());
}

#[tokio::test]
async fn a4_store_false_keeps_nothing_but_still_resolves_an_id() {
    let store = PrevStore::new();
    let reply = raw_response("resp_kept", "answer", "completed");
    keep(&store, "s", r#"{"model":"demo","input":"first"}"#, &reply);
    let expansion = store.expand(
        "s",
        br#"{"model":"demo","store":false,"previous_response_id":"resp_kept","input":"next"}"#,
    );
    assert!(expansion.ask.expect("ask").found);
    assert!(expansion.capture.is_none());
}

#[test]
fn a5_scopes_separate_callers_and_keyless_remote_peers() {
    let store = PrevStore::new();
    let reply = raw_response("resp_scope", "secret", "completed");
    keep(
        &store,
        "alice",
        r#"{"model":"demo","input":"private"}"#,
        &reply,
    );
    assert!(store.get("alice", "resp_scope").is_some());
    assert!(store.get("bob", "resp_scope").is_none());
    let remote: IpAddr = "192.0.2.10".parse().expect("IP");
    let loopback: IpAddr = "127.0.0.1".parse().expect("IP");
    assert_ne!(
        caller_scope(None, None, Some(remote)),
        caller_scope(None, None, Some(loopback))
    );
    assert_ne!(
        caller_scope(Some("one"), None, None),
        caller_scope(Some("two"), None, None)
    );
}

#[tokio::test]
async fn a6_given_whole_to_chat_across_three_turns() {
    let member = Arc::new(FakeMember::new(
        CHAT,
        false,
        vec![
            Reply::Text("answer 1"),
            Reply::Text("answer 2"),
            Reply::Text("answer 3"),
        ],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (status, first) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","instructions":"be brief","input":"my name is Ada"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first = id_of(&first);
    let (status, second) = post(app.clone(), "/v1/responses", &format!(r#"{{"model":"demo","previous_response_id":"{first}","input":[{{"role":"user","content":"what is my name?"}}]}}"#)).await;
    assert_eq!(status, StatusCode::OK);
    let second = id_of(&second);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(r#"{{"model":"demo","previous_response_id":"{second}","input":"and again?"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = member.seen();
    for (turn, expected) in [
        (1, vec!["my name is Ada", "answer 1", "what is my name?"]),
        (
            2,
            vec![
                "my name is Ada",
                "answer 1",
                "what is my name?",
                "answer 2",
                "and again?",
            ],
        ),
    ] {
        let text = seen[turn]["messages"].to_string();
        for needle in expected {
            assert!(
                text.contains(needle),
                "turn {} lacks {needle}: {text}",
                turn + 1
            );
        }
        assert!(seen[turn].get("previous_response_id").is_none());
    }
    assert!(!seen[1]["messages"].to_string().contains("be brief"));
}

#[tokio::test]
async fn a7_not_found_is_exact_and_precedes_translating_or_subscription_calls() {
    let member = Arc::new(FakeMember::new(
        CHAT,
        false,
        vec![Reply::Text("unused"), Reply::Text("unused")],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (status, body) = post(
        app,
        "/v1/responses",
        r#"{"model":"demo","previous_response_id":"resp_nowhere","input":"hi"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        String::from_utf8(body).expect("UTF-8"),
        r#"{"error":{"message":"Previous response with id 'resp_nowhere' not found.","type":"invalid_request_error","param":"previous_response_id","code":"previous_response_not_found"}}"#
    );
    assert!(member.seen().is_empty());

    let member = Arc::new(FakeMember::new(RESPONSES, true, vec![]).subscribed());
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (status, _) = post(
        app,
        "/v1/responses",
        r#"{"model":"demo","previous_response_id":"resp_nowhere","input":"hi"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(member.seen().is_empty());
}

#[tokio::test]
async fn a8_unknown_previous_response_is_relayed_as_sent() {
    let member = Arc::new(FakeMember::new(
        RESPONSES,
        true,
        vec![Reply::Raw(raw_response("resp_up", "ok", "completed"))],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (status, _) = post(
        app,
        "/v1/responses",
        r#"{"model":"demo","previous_response_id":"resp_before","input":"hi"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member.seen()[0]["previous_response_id"], "resp_before");
}

#[tokio::test]
async fn a9_non_keeping_routes_never_get_a_delta() {
    let member = Arc::new(FakeMember::new(
        RESPONSES,
        false,
        vec![
            Reply::Raw(raw_response("resp_up1", "first", "completed")),
            Reply::Raw(raw_response("resp_up2", "second", "completed")),
        ],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (_, first) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","store":true,"input":"large history"}"#,
    )
    .await;
    let first = id_of(&first);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(
            r#"{{"model":"demo","store":true,"previous_response_id":"{first}","input":"next"}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = member.seen();
    assert!(seen[1]["input"].to_string().contains("large history"));
    assert!(seen[1].get("previous_response_id").is_none());
}

#[tokio::test]
async fn omitted_store_keeps_the_privacy_default_and_sends_no_delta() {
    let member = Arc::new(FakeMember::new(
        RESPONSES,
        true,
        vec![
            Reply::Raw(raw_response("resp_up1", "first", "completed")),
            Reply::Raw(raw_response("resp_up2", "second", "completed")),
        ],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (_, first) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","input":"history"}"#,
    )
    .await;
    let first = id_of(&first);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(r#"{{"model":"demo","previous_response_id":"{first}","input":"next"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = member.seen();
    assert!(seen.iter().all(|body| body["store"] == false));
    assert!(seen[1]["input"].to_string().contains("history"));
    assert!(seen[1].get("previous_response_id").is_none());
}

#[tokio::test]
async fn a10_kept_upstream_sends_only_new_input_and_upstream_id() {
    let member = Arc::new(FakeMember::new(
        RESPONSES,
        true,
        vec![
            Reply::Raw(raw_response("resp_up1", "first", "completed")),
            Reply::Raw(raw_response("resp_up2", "second", "completed")),
        ],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let image = "QUFB".repeat(50_000);
    let first_body = format!(
        r#"{{"model":"demo","store":true,"input":[{{"type":"message","role":"user","content":[{{"type":"input_image","image_url":"data:image/png;base64,{image}"}},{{"type":"input_text","text":"what is this?"}}]}}]}}"#
    );
    let (_, first) = post(app.clone(), "/v1/responses", &first_body).await;
    let first = id_of(&first);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(
            r#"{{"model":"demo","store":true,"previous_response_id":"{first}","input":"and now?"}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = member.seen();
    let delta = serde_json::to_vec(&seen[1]).expect("delta");
    assert!(delta.len() < 2_000, "delta was {} bytes", delta.len());
    assert_eq!(seen[1]["previous_response_id"], "resp_up1");
    assert!(seen[1]["input"].to_string().contains("and now?"));
    assert!(!seen[1]["input"].to_string().contains("QUFB"));
}

#[tokio::test]
async fn a11_refused_delta_is_retried_once_with_the_whole_conversation() {
    let member = Arc::new(FakeMember::new(
        RESPONSES,
        true,
        vec![
            Reply::Raw(raw_response("resp_up1", "first", "completed")),
            Reply::Fail(400),
            Reply::Raw(raw_response("resp_up2", "second", "completed")),
        ],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (_, first) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","store":true,"input":"history"}"#,
    )
    .await;
    let first = id_of(&first);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(
            r#"{{"model":"demo","store":true,"previous_response_id":"{first}","input":"next"}}"#
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = member.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[1]["previous_response_id"], "resp_up1");
    assert!(seen[2]["input"].to_string().contains("history"));
    assert!(seen[2]["input"].to_string().contains("next"));
    assert!(seen[2].get("previous_response_id").is_none());
}

#[test]
fn a12_only_an_answered_keeping_relay_records_its_route() {
    let store = PrevStore::new();
    let expansion = store.expand("s", br#"{"model":"demo","store":true,"input":"hi"}"#);
    let ask = expansion.ask.expect("ask");
    let attempt = Attempt {
        member: MemberAttempt {
            protocol: RESPONSES.to_owned(),
            route: "fake\0hash".to_owned(),
            keeps_responses: true,
            ..MemberAttempt::default()
        },
        inbound_protocol: RESPONSES.to_owned(),
    };
    ask.attempt().answered(&attempt);
    let reply = raw_response("resp_route", "ok", "completed");
    let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
    capture.push(&reply);
    capture.finish();
    let kept = store.get("s", "resp_route").expect("kept");
    assert_eq!(kept.route, "fake\0hash");
    assert_eq!(kept.upstream_id, "resp_route");
}

#[test]
fn a13_prev_store_is_bounded_by_cap_ttl_replacement_lru_and_own_bytes() {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let store = PrevStore::with(100, TTL, Arc::new(move || *clock.lock().expect("clock")));
    for index in 0..4 {
        store.put("s".to_owned(), index.to_string(), turn(40));
    }
    assert!(store.get("s", "0").is_none());
    assert!(store.get("s", "3").is_some());
    store.put("s".to_owned(), "big".to_owned(), turn(60));
    assert!(store.get("s", "big").is_none());

    assert!(store.get("s", "2").is_some());
    store.put("s".to_owned(), "4".to_owned(), turn(40));
    assert!(store.get("s", "3").is_none());
    assert!(store.get("s", "2").is_some());
    store.put("s".to_owned(), "4".to_owned(), turn(10));
    assert_eq!(store.get("s", "4").expect("replacement").own, 10);

    *now.lock().expect("clock") += TTL + Duration::from_secs(1);
    assert!(store.get("s", "2").is_none());
    store.put("s".to_owned(), "5".to_owned(), turn(10));
    assert!(store.get("s", "4").is_none());
    assert!(store.get("s", "5").is_some());

    let shared = PrevStore::new();
    let first = shared.expand("s", br#"{"model":"demo","input":[{"type":"message","role":"user","content":"a long first turn with enough bytes to dominate the fresh item"}]}"#);
    let first_own = first.capture.as_ref().expect("first capture").own;
    let reply = raw_response("resp_shared", "answer", "completed");
    let mut capture = ReplyCapture::new(first.capture).expect("capture");
    capture.push(&reply);
    capture.finish();
    let continued = shared.expand(
        "s",
        br#"{"model":"demo","previous_response_id":"resp_shared","input":"tiny"}"#,
    );
    assert!(continued.capture.expect("continued").own < first_own);
}

#[tokio::test]
async fn a14_expansion_precedes_decode_at_the_future_masking_hook_point() {
    let member = Arc::new(FakeMember::new(
        CHAT,
        false,
        vec![Reply::Text("answer"), Reply::Text("next")],
    ));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (_, first) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","input":"stored text"}"#,
    )
    .await;
    let first = id_of(&first);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(r#"{{"model":"demo","previous_response_id":"{first}","input":"new text"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let messages = member.seen()[1]["messages"].to_string();
    assert!(messages.contains("stored text"));
    assert!(messages.contains("answer"));
}

#[tokio::test]
async fn a15_usage_and_routing_observe_the_expanded_request() {
    let member = Arc::new(FakeMember::new(
        CHAT,
        false,
        vec![Reply::Text("answer"), Reply::Text("next")],
    ));
    let (app, _, dir) = gateway(Arc::clone(&member));
    let (_, first) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","input":"history"}"#,
    )
    .await;
    let first = id_of(&first);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(r#"{{"model":"demo","previous_response_id":"{first}","input":"next"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(member.seen()[1]["messages"].to_string().contains("history"));
    let usage = Ledger::open(&dir.path().join("usage.sqlite")).expect("usage");
    let rows = usage.rows().expect("rows");
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.model == "demo" && row.status == "200")
    );
}

#[test]
fn a16_documented_store_and_reply_limits_are_fixed() {
    assert_eq!(CAP, 256 << 20);
    assert_eq!(TTL, Duration::from_secs(6 * 60 * 60));
    assert_eq!(REPLY_CAP, 64 << 20);
}

#[test]
fn failed_cancelled_and_cut_replies_are_not_kept() {
    for status in ["failed", "cancelled"] {
        let store = PrevStore::new();
        let expansion = store.expand("s", br#"{"model":"demo","input":"hi"}"#);
        let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
        capture.push(&raw_response("resp_bad", "no", status));
        capture.finish();
        assert!(store.get("s", "resp_bad").is_none());
    }
    let store = PrevStore::new();
    let expansion = store.expand("s", br#"{"model":"demo","input":"hi"}"#);
    let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
    capture.limit = 16;
    capture.push(&raw_response("resp_cut", "too large", "completed"));
    capture.finish();
    assert!(store.get("s", "resp_cut").is_none());
}

#[test]
fn an_unfinished_capture_is_not_kept() {
    let store = PrevStore::new();
    let expansion = store.expand("s", br#"{"model":"demo","input":"hi"}"#);
    let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
    capture.push(&raw_response(
        "resp_cancelled_stream",
        "partial",
        "completed",
    ));
    drop(capture);
    assert!(store.get("s", "resp_cancelled_stream").is_none());
}

#[tokio::test]
async fn non_200_replies_are_not_kept() {
    let member = Arc::new(FakeMember::new(CHAT, false, vec![Reply::Fail(400)]));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (status, _) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","input":"hi"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(
        app,
        "/v1/responses",
        r#"{"model":"demo","previous_response_id":"resp_never","input":"next"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(member.seen().len() == 1);
}

#[tokio::test]
async fn store_false_response_id_is_not_found_on_the_next_turn() {
    let member = Arc::new(FakeMember::new(CHAT, false, vec![Reply::Text("forget")]));
    let (app, _, _dir) = gateway(Arc::clone(&member));
    let (_, first) = post(
        app.clone(),
        "/v1/responses",
        r#"{"model":"demo","store":false,"input":"forget me"}"#,
    )
    .await;
    let first = id_of(&first);
    let (status, _) = post(
        app,
        "/v1/responses",
        &format!(r#"{{"model":"demo","previous_response_id":"{first}","input":"again"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(member.seen().len(), 1);
}

#[test]
fn a11_refusal_words_count_on_400_404_and_422_only() {
    let store = PrevStore::new();
    let ask = store
        .expand("s", br#"{"model":"demo","input":"hi"}"#)
        .ask
        .expect("ask");
    let hook = ask.attempt();
    let attempt = Attempt {
        member: MemberAttempt::default(),
        inbound_protocol: RESPONSES.to_owned(),
    };
    let word = br#"{"error":{"code":"previous_response_not_found"}}"#;
    for status in [400, 404, 422] {
        assert!(hook.refused(&attempt, status, word), "{status}");
        assert!(hook.refused(&attempt, status, b"Unknown parameter: previousResponse"));
        assert!(!hook.refused(&attempt, status, b"bad input"));
    }
    for status in [401, 409, 429, 500] {
        assert!(!hook.refused(&attempt, status, word), "{status}");
    }
}

#[test]
fn a2_crlf_and_comment_led_sse_replies_are_kept() {
    let store = PrevStore::new();
    let expansion = store.expand("s", br#"{"model":"demo","input":"hi"}"#);
    let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
    capture.push(
        b": keep-alive\r\n\r\nevent: response.output_item.done\r\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":\"yo\"}}\r\n\r\nevent: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_crlf\",\"status\":\"completed\",\"output\":[]}}\r\n\r\n",
    );
    capture.finish();
    let turn = store.get("s", "resp_crlf").expect("kept");
    assert_eq!(turn.items.len(), 2);
    assert_eq!(turn.items[1]["content"], "yo");
}

#[test]
fn a2_a_reply_led_by_a_blank_crlf_line_is_read_as_sse() {
    let store = PrevStore::new();
    capture(
        &store,
        "s",
        r#"{"model":"demo","input":"hi"}"#,
        b"\r\nevent: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_lead\",\"status\":\"completed\",\"output\":[]}}\r\n\r\n",
    );
    assert!(store.get("s", "resp_lead").is_some());
}

#[test]
fn a_reply_cut_at_the_cap_stays_cut_for_later_chunks() {
    let store = PrevStore::new();
    let expansion = store.expand("s", br#"{"model":"demo","input":"hi"}"#);
    let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
    let whole = raw_response("resp_after_cut", "ok", "completed");
    capture.limit = whole.len();
    capture.push(&vec![b' '; whole.len() + 1]);
    capture.push(&whole);
    capture.finish();
    assert!(store.get("s", "resp_after_cut").is_none());
}

fn kept_on_route(store: &PrevStore, route: &str) -> String {
    let expansion = store.expand("s", br#"{"model":"demo","store":true,"input":"first"}"#);
    let attempt = keeping_attempt(route);
    expansion.ask.expect("ask").attempt().answered(&attempt);
    let mut capture = ReplyCapture::new(expansion.capture).expect("capture");
    capture.push(&raw_response("resp_kept", "one", "completed"));
    capture.finish();
    "resp_kept".to_owned()
}

fn keeping_attempt(route: &str) -> Attempt {
    Attempt {
        member: MemberAttempt {
            protocol: RESPONSES.to_owned(),
            route: route.to_owned(),
            keeps_responses: true,
            ..MemberAttempt::default()
        },
        inbound_protocol: RESPONSES.to_owned(),
    }
}

#[test]
fn a10_delta_only_on_the_route_that_answered_and_an_unchanged_item_count() {
    let store = PrevStore::new();
    let id = kept_on_route(&store, "same\0key");
    let next = format!(
        r#"{{"model":"demo","store":true,"previous_response_id":"{id}","input":"second"}}"#
    );
    let expansion = store.expand("s", next.as_bytes());
    let hook = expansion.ask.expect("ask").attempt();
    let body = expansion.body;

    let delta = hook
        .prepare(&keeping_attempt("same\0key"), &body)
        .expect("delta on the answering route");
    let delta: Value = serde_json::from_slice(&delta).expect("delta JSON");
    assert_eq!(delta["previous_response_id"], "resp_kept");
    assert_eq!(delta["input"].as_array().expect("items").len(), 1);

    assert!(
        hook.prepare(&keeping_attempt("other\0key"), &body)
            .is_none(),
        "another provider or key gets the whole conversation"
    );

    let mut changed: Value = serde_json::from_slice(&body).expect("body JSON");
    changed["input"]
        .as_array_mut()
        .expect("items")
        .push(json!({"type":"message","role":"user","content":"extra"}));
    let changed = serde_json::to_vec(&changed).expect("changed");
    assert!(
        hook.prepare(&keeping_attempt("same\0key"), &changed)
            .is_none(),
        "a body whose item count moved is sent whole"
    );
}
