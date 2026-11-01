//! Thinking-only refusal handover across the three streaming protocols.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::stream;
use hofund_codec::{AnthropicMessages, Codec, OpenAiChat, OpenAiResponses};
use hofund_gateway::Gateway;
use hofund_ir::{
    BlockStart, ContentBlockStart, ContentBlockStop, Extensions, MessageDelta, MessageStart,
    MessageStop, Request as ModelRequest, Stop, StopReason, StreamEvent, TextDelta, ThinkingDelta,
    ToolArgumentsDelta, UnknownEvent,
};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

enum Reply {
    Events(Vec<Result<StreamEvent, PluginError>>),
    Raw(Vec<u8>),
}

struct Scripted {
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    reply: Mutex<Option<Reply>>,
}

impl Scripted {
    fn events(route_id: &'static str, events: Vec<StreamEvent>) -> Arc<Self> {
        Self::new(
            route_id,
            Reply::Events(events.into_iter().map(Ok).collect()),
        )
    }

    fn raw(route_id: &'static str, bytes: Vec<u8>) -> Arc<Self> {
        Self::new(route_id, Reply::Raw(bytes))
    }

    fn new(route_id: &'static str, reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            route_id,
            member_id: MemberId::from_preset(
                PresetId::try_from(route_id).expect("preset"),
                MemberKey::from_static("row"),
            ),
            hits: Arc::new(AtomicUsize::new(0)),
            reply: Mutex::new(Some(reply)),
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
            Reply::Events(events) => Ok(Completion::Stream(Box::pin(stream::iter(events)))),
            Reply::Raw(bytes) => Ok(Completion::Raw(BodyStream::from_bytes(bytes))),
        }
    }
}

fn gateway(members: Vec<Arc<Scripted>>) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let members = members
        .into_iter()
        .map(|member| member as Arc<dyn Member>)
        .collect();
    let routes = BTreeMap::from([("demo".to_owned(), members)]);
    let codecs: HashMap<String, Arc<dyn Codec>> = HashMap::from([
        (
            "/v1/messages".to_owned(),
            Arc::new(AnthropicMessages) as Arc<dyn Codec>,
        ),
        (
            "/v1/chat/completions".to_owned(),
            Arc::new(OpenAiChat) as Arc<dyn Codec>,
        ),
        (
            "/v1/responses".to_owned(),
            Arc::new(OpenAiResponses) as Arc<dyn Codec>,
        ),
    ]);
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    )
    .into_router();
    (app, dir)
}

async fn call(app: axum::Router, path: &str, body: &'static str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", "Bearer secret")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8(bytes.to_vec()).expect("utf-8"))
}

const MESSAGES: &str =
    r#"{"model":"demo","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
const CHAT: &str = r#"{"model":"demo","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
const RESPONSES: &str = r#"{"model":"demo","stream":true,"input":"hi"}"#;

fn start() -> StreamEvent {
    StreamEvent::MessageStart(MessageStart {
        id: Some("reply".to_owned()),
        model: Some("demo".to_owned()),
        usage: None,
        extensions: Extensions::new(),
    })
}

fn block(index: u32, block: BlockStart) -> StreamEvent {
    StreamEvent::ContentBlockStart(ContentBlockStart {
        index,
        block,
        extensions: Extensions::new(),
    })
}

fn thinking_block(index: u32) -> StreamEvent {
    block(
        index,
        BlockStart::Thinking {
            redacted: false,
            data: None,
        },
    )
}

fn thinking(index: u32, text: &str) -> StreamEvent {
    StreamEvent::ThinkingDelta(ThinkingDelta {
        index,
        text: text.to_owned(),
        extensions: Extensions::new(),
    })
}

fn text_block(index: u32) -> StreamEvent {
    block(index, BlockStart::Text)
}

fn text(index: u32, text: &str) -> StreamEvent {
    StreamEvent::TextDelta(TextDelta {
        index,
        text: text.to_owned(),
        extensions: Extensions::new(),
    })
}

fn stop_block(index: u32) -> StreamEvent {
    StreamEvent::ContentBlockStop(ContentBlockStop {
        index,
        extensions: Extensions::new(),
    })
}

fn stopped(reason: StopReason) -> StreamEvent {
    StreamEvent::MessageDelta(MessageDelta {
        stop: Some(Stop {
            reason,
            sequence: None,
        }),
        usage: None,
        extensions: Extensions::new(),
    })
}

fn message_stop() -> StreamEvent {
    StreamEvent::MessageStop(MessageStop::default())
}

fn thought_then_refusal(thinking_index: u32) -> Vec<StreamEvent> {
    vec![
        start(),
        thinking_block(thinking_index),
        thinking(thinking_index, "first thought"),
        stopped(StopReason::Refusal),
        message_stop(),
    ]
}

fn answer() -> Vec<StreamEvent> {
    vec![
        start(),
        text_block(0),
        text(0, "from next"),
        stop_block(0),
        stopped(StopReason::EndTurn),
        message_stop(),
    ]
}

fn error_event(code: &str) -> StreamEvent {
    StreamEvent::Unknown(UnknownEvent {
        name: "error".to_owned(),
        payload: serde_json::json!({"error":{"code":code,"message":"refused"}}),
    })
}

fn data_events(body: &str) -> Vec<serde_json::Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("JSON event"))
        .collect()
}

#[tokio::test]
async fn b9a_anthropic_thinking_refusal_splices_the_next_member() {
    let first = Scripted::events("first", thought_then_refusal(0));
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("first thought") && body.contains("from next"),
        "{body}"
    );
    assert!(!body.contains("refusal"), "{body}");
    assert_eq!(
        body.matches("\"type\":\"message_start\"").count(),
        1,
        "{body}"
    );
    assert_eq!(
        body.matches("\"type\":\"message_stop\"").count(),
        1,
        "{body}"
    );
    let events = data_events(&body);
    let indexes: Vec<u64> = events
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .map(|event| event["index"].as_u64().expect("index"))
        .collect();
    assert_eq!(indexes, [0, 1], "{body}");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "content_block_stop" && event["index"] == 0)
    );
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9a_chat_thinking_refusal_splices_the_next_member() {
    let first = Scripted::events("first", thought_then_refusal(1));
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/chat/completions", CHAT).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("first thought") && body.contains("from next"),
        "{body}"
    );
    assert!(!body.contains("refusal"), "{body}");
    assert_eq!(body.matches("data: [DONE]").count(), 1, "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9a_responses_thinking_refusal_splices_the_next_member() {
    let first = Scripted::events("first", thought_then_refusal(0));
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/responses", RESPONSES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("first thought") && body.contains("from next"),
        "{body}"
    );
    assert!(!body.contains("refusal"), "{body}");
    let events = data_events(&body);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.created")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "response.completed")
            .count(),
        1
    );
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event["sequence_number"], sequence, "{body}");
    }
    let added: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.output_item.added")
        .collect();
    assert_eq!(added[0]["output_index"], 0, "{body}");
    assert_eq!(added[1]["output_index"], 1, "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9b_text_then_refusal_stays_with_the_member() {
    let mut events = thought_then_refusal(0);
    events.splice(3..3, [text_block(1), text(1, "already said")]);
    let first = Scripted::events("first", events);
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("already said") && body.contains("refusal"),
        "{body}"
    );
    assert!(!body.contains("from next"), "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 0));
}

#[tokio::test]
async fn b9c_no_output_refusal_keeps_fresh_framing() {
    let first = Scripted::events(
        "first",
        vec![
            start(),
            text_block(0),
            stopped(StopReason::Refusal),
            message_stop(),
        ],
    );
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body.contains("first thought") && !body.contains("refusal"),
        "{body}"
    );
    assert_eq!(
        body.matches("\"type\":\"message_start\"").count(),
        1,
        "{body}"
    );
    let events = data_events(&body);
    let block = events
        .iter()
        .find(|event| event["type"] == "content_block_start")
        .expect("block");
    assert_eq!(block["index"], 0, "{body}");
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "content_block_stop")
            .count(),
        1,
        "{body}"
    );
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9d_last_member_keeps_the_refusal() {
    let first = Scripted::events("first", thought_then_refusal(1));
    let (app, _dir) = gateway(vec![first]);
    let (status, body) = call(app, "/v1/chat/completions", CHAT).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("first thought") && body.contains("refusal"),
        "{body}"
    );
    assert_eq!(body.matches("data: [DONE]").count(), 1, "{body}");
}

#[tokio::test]
async fn b9e_refusal_error_after_thinking_hands_over() {
    let first = Scripted::events(
        "first",
        vec![
            start(),
            thinking_block(0),
            thinking(0, "first thought"),
            error_event("content_filter"),
        ],
    );
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from next"), "{body}");
    assert!(!body.contains("content_filter"), "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9f_tool_call_then_refusal_stays() {
    let first = Scripted::events(
        "first",
        vec![
            start(),
            thinking_block(0),
            thinking(0, "first thought"),
            block(
                1,
                BlockStart::ToolCall {
                    id: "call".to_owned(),
                    name: "tool".to_owned(),
                },
            ),
            StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: 1,
                partial_json: "{}".to_owned(),
                extensions: Extensions::new(),
            }),
            stopped(StopReason::Refusal),
            message_stop(),
        ],
    );
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("tool_use") && body.contains("refusal"),
        "{body}"
    );
    assert_eq!((first.hits(), second.hits()), (1, 0));
}

#[tokio::test]
async fn b9g_repeated_refusals_accumulate_block_indexes() {
    let first = Scripted::events("first", thought_then_refusal(0));
    let second = Scripted::events("second", thought_then_refusal(0));
    let third = Scripted::events("third", answer());
    let (app, _dir) = gateway(vec![first, second, Arc::clone(&third)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body.matches("\"type\":\"message_start\"").count(),
        1,
        "{body}"
    );
    let indexes: Vec<u64> = data_events(&body)
        .iter()
        .filter(|event| event["type"] == "content_block_start")
        .map(|event| event["index"].as_u64().expect("index"))
        .collect();
    assert_eq!(indexes, [0, 1, 2], "{body}");
    assert_eq!(third.hits(), 1);
}

#[tokio::test]
async fn b9h_non_refusal_error_after_thinking_stays() {
    let first = Scripted::events(
        "first",
        vec![
            start(),
            thinking_block(0),
            thinking(0, "first thought"),
            error_event("server_error"),
        ],
    );
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("server_error"), "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 0));
}

#[tokio::test]
async fn b9i_empty_thinking_output_still_hands_over() {
    let first = Scripted::events(
        "first",
        vec![
            start(),
            thinking_block(0),
            thinking(0, ""),
            stop_block(0),
            stopped(StopReason::Refusal),
            message_stop(),
        ],
    );
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from next"), "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9j_raw_same_protocol_streams_splice() {
    let first_bytes = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"demo\",\"content\":[]}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"raw thought\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let second_bytes = AnthropicMessages.encode_stream(&answer()).expect("answer");
    let first = Scripted::raw("first", first_bytes.as_bytes().to_vec());
    let second = Scripted::raw("second", second_bytes);
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("raw thought") && body.contains("from next"),
        "{body}"
    );
    assert_eq!(
        body.matches("\"type\":\"message_start\"").count(),
        1,
        "{body}"
    );
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9k_sensitive_chat_finish_hands_over() {
    let first_bytes = concat!(
        "data: {\"id\":\"c\",\"model\":\"demo\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"sensitive thought\"}}]}\n\n",
        "data: {\"id\":\"c\",\"model\":\"demo\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"sensitive\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let second_bytes = OpenAiChat.encode_stream(&answer()).expect("answer");
    let first = Scripted::raw("first", first_bytes.as_bytes().to_vec());
    let second = Scripted::raw("second", second_bytes);
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/chat/completions", CHAT).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("sensitive thought") && body.contains("from next"),
        "{body}"
    );
    assert!(!body.contains("finish_reason\":\"sensitive"), "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9l_responses_refusal_error_hands_over() {
    let first_bytes = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"r\",\"model\":\"demo\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"id\":\"rs\",\"type\":\"reasoning\",\"summary\":[]}}\n\n",
        "event: response.reasoning_text.delta\n",
        "data: {\"type\":\"response.reasoning_text.delta\",\"sequence_number\":2,\"output_index\":0,\"content_index\":0,\"delta\":\"response thought\"}\n\n",
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"sequence_number\":3,\"response\":{\"id\":\"r\",\"status\":\"failed\",\"output\":[],\"error\":{\"code\":\"bio_policy\",\"message\":\"refused\"}}}\n\n"
    );
    let second_bytes = OpenAiResponses.encode_stream(&answer()).expect("answer");
    let first = Scripted::raw("first", first_bytes.as_bytes().to_vec());
    let second = Scripted::raw("second", second_bytes);
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/responses", RESPONSES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("response thought") && body.contains("from next"),
        "{body}"
    );
    assert!(!body.contains("bio_policy"), "{body}");
    assert_eq!(
        body.matches("\"type\":\"response.created\"").count(),
        1,
        "{body}"
    );
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

fn thought_then_error(code: &str) -> Vec<StreamEvent> {
    vec![
        start(),
        thinking_block(0),
        thinking(0, "first thought"),
        error_event(code),
    ]
}

fn error_frame_not_raw(body: &str) {
    assert!(body.contains("refused"), "{body}");
    assert!(!body.contains("\"code\":\"content_filter\""), "{body}");
}

#[tokio::test]
async fn b9m_lone_member_refusal_error_is_the_protocol_error_frame() {
    let first = Scripted::events("first", thought_then_error("content_filter"));
    let (app, _dir) = gateway(vec![Arc::clone(&first)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("first thought"), "{body}");
    assert!(body.contains("event: error"), "{body}");
    error_frame_not_raw(&body);
}

#[tokio::test]
async fn b9n_refusal_error_after_text_is_the_protocol_error_frame() {
    let first = Scripted::events(
        "first",
        vec![
            start(),
            text_block(0),
            text(0, "said"),
            error_event("content_filter"),
        ],
    );
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/chat/completions", CHAT).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("said"), "{body}");
    error_frame_not_raw(&body);
    assert_eq!((first.hits(), second.hits()), (1, 0));
}

#[tokio::test]
async fn b9o_last_handed_member_refusal_error_is_the_protocol_error_frame() {
    let first = Scripted::events("first", thought_then_refusal(0));
    let second = Scripted::events("second", thought_then_error("content_filter"));
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("event: error"), "{body}");
    error_frame_not_raw(&body);
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[tokio::test]
async fn b9q_open_thinking_block_closes_before_the_next_member_starts() {
    for path in ["/v1/messages", "/v1/responses"] {
        let first = Scripted::events("first", thought_then_refusal(0));
        let second = Scripted::events("second", answer());
        let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
        let body_in = if path == "/v1/messages" {
            MESSAGES
        } else {
            RESPONSES
        };
        let (status, body) = call(app, path, body_in).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let events = data_events(&body);
        let closed = events.iter().position(|event| {
            (event["type"] == "content_block_stop" && event["index"] == 0)
                || (event["type"] == "response.output_item.done" && event["output_index"] == 0)
        });
        let next = events.iter().position(|event| {
            (event["type"] == "content_block_start" && event["index"] == 1)
                || (event["type"] == "response.output_item.added" && event["output_index"] == 1)
        });
        assert!(
            matches!((closed, next), (Some(closed), Some(next)) if closed < next),
            "{path}: {body}"
        );
    }
}

#[tokio::test]
async fn b9r_tool_arguments_alone_count_as_spoken() {
    let first = Scripted::events(
        "first",
        vec![
            start(),
            thinking_block(0),
            thinking(0, "first thought"),
            StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: 1,
                partial_json: "{}".to_owned(),
                extensions: Extensions::new(),
            }),
            stopped(StopReason::Refusal),
            message_stop(),
        ],
    );
    let second = Scripted::events("second", answer());
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("from next"), "{body}");
    assert_eq!((first.hits(), second.hits()), (1, 0));
}

#[tokio::test]
async fn b9s_spliced_responses_stream_drops_the_next_members_progress_events() {
    let first = Scripted::events("first", thought_then_refusal(0));
    let mut next = answer();
    next.insert(
        1,
        StreamEvent::Unknown(UnknownEvent {
            name: "response.in_progress".to_owned(),
            payload: serde_json::json!({"type":"response.in_progress","response":{"id":"r2","status":"in_progress"}}),
        }),
    );
    let second = Scripted::events("second", next);
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/responses", RESPONSES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from next"), "{body}");
    assert!(!body.contains("\"id\":\"r2\""), "{body}");
}

#[tokio::test]
async fn b9t_last_member_refusal_error_keeps_its_held_events() {
    let first = Scripted::events("first", thought_then_refusal(0));
    let second = Scripted::events(
        "second",
        vec![start(), text_block(0), error_event("content_filter")],
    );
    let (app, _dir) = gateway(vec![Arc::clone(&first), Arc::clone(&second)]);
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = data_events(&body);
    let held = events
        .iter()
        .position(|event| event["type"] == "content_block_start" && event["index"] == 1);
    let error = events.iter().position(|event| event["type"] == "error");
    assert!(
        matches!((held, error), (Some(held), Some(error)) if held < error),
        "{body}"
    );
    assert_eq!((first.hits(), second.hits()), (1, 1));
}

#[derive(Default)]
struct Ends(Mutex<Vec<Option<String>>>);

impl hofund_gateway::RequestEvents for Ends {
    fn started(&self, _start: &hofund_gateway::RequestStart) {}
    fn finished(&self, end: &hofund_gateway::RequestEnd) {
        self.0.lock().expect("ends").push(end.member.clone());
    }
}

#[tokio::test]
async fn b9u_request_end_names_the_member_that_took_over() {
    let first = Scripted::events("first", thought_then_refusal(0));
    let second = Scripted::events("second", answer());
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let members = vec![
        Arc::clone(&first) as Arc<dyn Member>,
        Arc::clone(&second) as Arc<dyn Member>,
    ];
    let routes = BTreeMap::from([("demo".to_owned(), members)]);
    let codecs: HashMap<String, Arc<dyn Codec>> = HashMap::from([(
        "/v1/messages".to_owned(),
        Arc::new(AnthropicMessages) as Arc<dyn Codec>,
    )]);
    let ends = Arc::new(Ends::default());
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    )
    .with_request_events(
        Arc::clone(&ends) as Arc<dyn hofund_gateway::RequestEvents>,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
    )
    .into_router();
    let (status, body) = call(app, "/v1/messages", MESSAGES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from next"), "{body}");
    let ends = ends.0.lock().expect("ends").clone();
    assert_eq!(ends, [Some(second.route_id().to_owned())], "{ends:?}");
}
