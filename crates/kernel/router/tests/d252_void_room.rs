//! Length cuts without visible output and learned reply room.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use hofund_codec::CHAT_PROTOCOL_ID;
use hofund_ir::{
    ContentPart, Extensions, Message, MessageDelta, MessageStop, Request, Response, Role, Stop,
    StopReason, StreamEvent, TextDelta, ThinkingDelta, ThinkingPart, Usage,
};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Router, RouterError,
    VoidRooms,
};

const RAW_VOID: &[u8] = concat!(
    "data: {\"id\":\"cut\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"thinking\"}}]}\n\n",
    "data: {\"id\":\"cut\",\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":8192,\"completion_tokens_details\":{\"reasoning_tokens\":8192}}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const RAW_SMALL_VOID: &[u8] = concat!(
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"completion_tokens\":16,\"completion_tokens_details\":{\"reasoning_tokens\":16}}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const RAW_TEXT: &[u8] = concat!(
    "data: {\"id\":\"reply\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"thinking\"}}]}\n\n",
    "data: {\"id\":\"reply\",\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
    "data: {\"id\":\"reply\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"completion_tokens\":12000}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const RAW_CUT_TEXT: &[u8] = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"half an answer\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],\"usage\":{\"completion_tokens\":8192}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const RAW_UNARY_VOID: &[u8] = br#"{"id":"cut","choices":[{"message":{"role":"assistant","content":"","reasoning_content":"thinking"},"finish_reason":"length"}],"usage":{"completion_tokens":8192,"completion_tokens_details":{"reasoning_tokens":8192}}}"#;
const RAW_UNARY_TEXT: &[u8] = br#"{"id":"reply","choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"completion_tokens":12000}}"#;

#[derive(Clone, Copy)]
enum Reply {
    RawVoid,
    RawSmallVoid,
    RawText,
    RawCutText,
    RawUnaryVoid,
    RawUnaryText,
    DecodedVoid,
    DecodedText,
    UnaryVoid,
    UnaryText,
    RefuseOutput,
    RefuseWindow,
}

struct Fake {
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    limits: Arc<Mutex<Vec<Option<u64>>>>,
    replies: Vec<Reply>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
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
        match self.replies[hit.min(self.replies.len() - 1)] {
            Reply::RawVoid => raw(RAW_VOID),
            Reply::RawSmallVoid => raw(RAW_SMALL_VOID),
            Reply::RawText => raw(RAW_TEXT),
            Reply::RawCutText => raw(RAW_CUT_TEXT),
            Reply::RawUnaryVoid => raw(RAW_UNARY_VOID),
            Reply::RawUnaryText => raw(RAW_UNARY_TEXT),
            Reply::DecodedVoid => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(thinking()),
                Ok(message_delta(StopReason::MaxTokens)),
                Ok(StreamEvent::MessageStop(MessageStop::default())),
            ])))),
            Reply::DecodedText => Ok(Completion::Stream(Box::pin(stream::iter(vec![
                Ok(thinking()),
                Ok(StreamEvent::TextDelta(TextDelta {
                    index: 0,
                    text: "done".to_owned(),
                    extensions: Extensions::new(),
                })),
                Ok(message_delta(StopReason::EndTurn)),
                Ok(StreamEvent::MessageStop(MessageStop::default())),
            ])))),
            Reply::UnaryVoid => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(ContentPart::Thinking(ThinkingPart {
                    text: "thinking".to_owned(),
                    signature: None,
                    redacted: false,
                    data: None,
                    extensions: Extensions::new(),
                }));
                Ok(Completion::Unary(Box::new(
                    Response::new(message)
                        .with_stop(Some(Stop {
                            reason: StopReason::MaxTokens,
                            sequence: None,
                        }))
                        .with_usage(Some(usage())),
                )))
            }
            Reply::UnaryText => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(ContentPart::text("done"));
                Ok(Completion::Unary(Box::new(Response::new(message))))
            }
            Reply::RefuseOutput => Err(PluginError::status_error(
                400,
                "max_tokens must be at most 16384",
            )),
            Reply::RefuseWindow => Err(PluginError::status_error(
                400,
                "input length and max_tokens exceed context limit: 197202 + 21333 > 200000",
            )),
        }
    }
}

fn raw(bytes: &'static [u8]) -> Result<Completion, PluginError> {
    Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from_static(
        bytes,
    ))))
}

fn thinking() -> StreamEvent {
    StreamEvent::ThinkingDelta(ThinkingDelta {
        index: 0,
        text: "thinking".to_owned(),
        extensions: Extensions::new(),
    })
}

fn message_delta(reason: StopReason) -> StreamEvent {
    StreamEvent::MessageDelta(MessageDelta {
        stop: Some(Stop {
            reason,
            sequence: None,
        }),
        usage: Some(usage()),
        extensions: Extensions::new(),
    })
}

fn usage() -> Usage {
    Usage {
        output_tokens: Some(8_192),
        reasoning_tokens: Some(8_192),
        ..Usage::default()
    }
}

struct Fixture {
    member: Arc<dyn Member>,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    limits: Arc<Mutex<Vec<Option<u64>>>>,
}

fn fixture(route_id: &'static str, replies: Vec<Reply>) -> Fixture {
    let hits = Arc::new(AtomicUsize::new(0));
    let limits = Arc::new(Mutex::new(Vec::new()));
    let member_id = MemberId::from_preset(
        PresetId::try_from(route_id).expect("preset"),
        MemberKey::from_static("row"),
    );
    Fixture {
        member: Arc::new(Fake {
            route_id,
            member_id: member_id.clone(),
            hits: Arc::clone(&hits),
            limits: Arc::clone(&limits),
            replies,
        }),
        member_id,
        hits,
        limits,
    }
}

fn router(members: Vec<Arc<dyn Member>>) -> Router {
    Router::new(
        BTreeMap::from([("demo".to_owned(), members)]),
        FailoverPolicy::default(),
    )
}

fn request(limit: Option<u64>, streaming: bool) -> Request {
    let mut request = Request::new("demo");
    request.params.max_tokens = limit;
    request.stream = streaming;
    request
}

async fn dispatch(router: &Router, limit: Option<u64>, streaming: bool) -> Result<String, String> {
    match router
        .dispatch(&request(limit, streaming), CHAT_PROTOCOL_ID)
        .await
    {
        Ok(routed) => Ok(visible_text(routed.completion).await),
        Err(RouterError::Upstream { error, .. }) => Err(error.message().to_owned()),
        Err(other) => panic!("unexpected routing error: {other}"),
    }
}

async fn visible_text(completion: Completion) -> String {
    match completion {
        Completion::Raw(mut body) => {
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next_chunk().await {
                bytes.extend_from_slice(&chunk.expect("chunk"));
            }
            String::from_utf8(bytes).expect("text reply")
        }
        Completion::Unary(response) => response
            .message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect(),
        Completion::Stream(mut events) => {
            let mut text = String::new();
            while let Some(event) = events.next().await {
                if let StreamEvent::TextDelta(delta) = event.expect("event") {
                    text.push_str(&delta.text);
                }
            }
            text
        }
        _ => panic!("unexpected completion"),
    }
}

#[tokio::test]
async fn d252_default_void_asks_for_room_and_later_requests_start_with_it() {
    let first = fixture("only", vec![Reply::RawVoid, Reply::RawText, Reply::RawText]);
    let rooms = VoidRooms::default();
    let first_router = router(vec![first.member.clone()]).with_void_rooms(rooms.clone());
    let answer = dispatch(&first_router, None, true).await.expect("answer");
    assert!(answer.contains("done"), "{answer}");
    assert!(!answer.contains("cut at its length"), "{answer}");
    assert_eq!(
        first.limits.lock().expect("limits").as_slice(),
        [None, Some(32_768)]
    );

    let second_router = router(vec![first.member.clone()]).with_void_rooms(rooms);
    let answer = dispatch(&second_router, None, true).await.expect("answer");
    assert!(answer.contains("done"), "{answer}");
    assert_eq!(
        first.limits.lock().expect("limits").as_slice(),
        [None, Some(32_768), Some(32_768)]
    );
    assert_eq!(first.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn d252_client_named_limit_fails_once_and_fails_over_without_rest() {
    let first = fixture("first", vec![Reply::RawVoid]);
    let second = fixture("second", vec![Reply::RawText]);
    let book = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    let router =
        router(vec![first.member.clone(), second.member.clone()]).with_cooldowns(Arc::clone(&book));
    let answer = dispatch(&router, Some(8_192), true).await.expect("answer");
    assert!(answer.contains("done"), "{answer}");
    assert_eq!(
        first.limits.lock().expect("limits").as_slice(),
        [Some(8_192)]
    );
    assert_eq!(
        second.limits.lock().expect("limits").as_slice(),
        [Some(8_192)]
    );
    assert_eq!(book.cooling(&first.member_id), None);
}

#[tokio::test]
async fn d252_client_named_limit_error_names_the_clients_limit() {
    let only = fixture("only", vec![Reply::RawVoid]);
    let error = dispatch(&router(vec![only.member]), Some(8_192), true)
        .await
        .expect_err("void");
    assert!(error.contains("cut at its length"), "{error}");
    assert!(error.contains("request's limit was 8192"), "{error}");
    assert_eq!(only.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn d252_small_probe_and_cut_with_text_pass_without_retry() {
    for (limit, reply, wanted) in [
        (Some(100), Reply::RawSmallVoid, "finish_reason\":\"length"),
        (Some(64), Reply::RawCutText, "half an answer"),
    ] {
        let only = fixture("only", vec![reply]);
        let answer = dispatch(&router(vec![only.member]), limit, true)
            .await
            .expect("reply");
        assert!(answer.contains(wanted), "{answer}");
        assert!(!answer.contains("an empty reply"), "{answer}");
        assert_eq!(only.hits.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn d252_catalog_room_is_fitted_to_context() {
    let only = fixture("only", vec![Reply::RawVoid, Reply::RawText]);
    let mut hop = Hop::new(only.member.clone());
    hop.catalog_output_tokens = Some(65_536);
    hop.context_window = Some(60_000);
    let router = Router::from_hops(
        BTreeMap::from([("demo".to_owned(), vec![hop])]),
        FailoverPolicy::default(),
    );
    dispatch(&router, None, true).await.expect("answer");
    assert_eq!(
        only.limits.lock().expect("limits").as_slice(),
        [None, Some(58_976)]
    );
}

#[tokio::test]
async fn d252_refused_room_retries_without_it_and_is_forgotten() {
    let only = fixture(
        "only",
        vec![Reply::RawVoid, Reply::RefuseOutput, Reply::RawVoid],
    );
    let router = router(vec![only.member.clone()]);
    let error = dispatch(&router, None, true).await.expect_err("void");
    assert!(error.contains("cut at its length"), "{error}");
    assert_eq!(
        only.limits.lock().expect("limits").as_slice(),
        [None, Some(32_768), None]
    );
    assert_eq!(only.hits.load(Ordering::SeqCst), 3);

    let error = dispatch(&router, None, true).await.expect_err("void");
    assert!(error.contains("cut at its length"), "{error}");
    assert_eq!(
        only.limits.lock().expect("limits").as_slice(),
        [None, Some(32_768), None, None]
    );
    assert_eq!(only.hits.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn d252_decoded_and_whole_replies_learn_room() {
    for (void, text, streaming) in [
        (Reply::RawUnaryVoid, Reply::RawUnaryText, false),
        (Reply::DecodedVoid, Reply::DecodedText, true),
        (Reply::UnaryVoid, Reply::UnaryText, false),
    ] {
        let only = fixture("only", vec![void, text, text]);
        let router = router(vec![only.member.clone()]);
        let answer = dispatch(&router, None, streaming).await.expect("answer");
        assert!(answer.contains("done"), "{answer}");
        let answer = dispatch(&router, None, streaming).await.expect("answer");
        assert!(answer.contains("done"), "{answer}");
        assert_eq!(
            only.limits.lock().expect("limits").as_slice(),
            [None, Some(32_768), Some(32_768)]
        );
    }
}

#[tokio::test]
async fn d252_explicit_zero_fails_once_without_learning_a_room() {
    let only = fixture("only", vec![Reply::RawVoid, Reply::RawText]);
    let rooms = VoidRooms::default();
    let router = router(vec![only.member.clone()]).with_void_rooms(rooms.clone());
    let error = dispatch(&router, Some(0), true).await.expect_err("void");
    assert!(error.contains("cut at its length"), "{error}");
    assert!(
        !error.contains("request's limit was"),
        "a zero is a client limit, and the error names only a positive one: {error}"
    );
    assert_eq!(only.hits.load(Ordering::SeqCst), 1);
    assert_eq!(only.limits.lock().expect("limits").as_slice(), [Some(0)]);
    assert_eq!(rooms.get("only", "demo"), None);

    let later = dispatch(&router, None, true).await.expect("later");
    assert!(later.contains("done"), "{later}");
    assert_eq!(
        only.limits.lock().expect("limits").as_slice(),
        [Some(0), None]
    );
    assert_eq!(rooms.get("only", "demo"), None);
}

#[tokio::test]
async fn d252_configured_hop_cap_survives_a_void() {
    let rooms = VoidRooms::default();
    let capped = fixture("capped", vec![Reply::RawVoid, Reply::RawText]);
    let capped_router = hop_router(capped.member.clone(), 4_096, rooms.clone());
    let error = dispatch(&capped_router, None, true)
        .await
        .expect_err("void");
    assert!(error.contains("cut at its length"), "{error}");
    assert_eq!(capped.hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        capped.limits.lock().expect("limits").as_slice(),
        [Some(4_096)]
    );
    assert_eq!(rooms.get("capped", "demo"), None);

    let later = dispatch(&capped_router, None, true).await.expect("later");
    assert!(later.contains("done"), "{later}");
    assert_eq!(
        capped.limits.lock().expect("limits").as_slice(),
        [Some(4_096), Some(4_096)]
    );
    assert_eq!(rooms.get("capped", "demo"), None);

    let open = fixture("only", vec![Reply::RawVoid, Reply::RawText]);
    let open_router = router(vec![open.member.clone()]).with_void_rooms(rooms.clone());
    dispatch(&open_router, None, true).await.expect("learn");
    assert_eq!(rooms.get("only", "demo"), Some(32_768));
    let guarded = fixture("only", vec![Reply::RawText]);
    let guarded_router = hop_router(guarded.member.clone(), 4_096, rooms);
    let answer = dispatch(&guarded_router, None, true).await.expect("capped");
    assert!(answer.contains("done"), "{answer}");
    assert_eq!(
        guarded.limits.lock().expect("limits").as_slice(),
        [Some(4_096)]
    );
}

#[tokio::test]
async fn d252_window_refusal_leaves_learned_room() {
    let only = fixture(
        "only",
        vec![
            Reply::RawVoid,
            Reply::RawText,
            Reply::RefuseWindow,
            Reply::RawText,
        ],
    );
    let rooms = VoidRooms::default();
    let router = router(vec![only.member.clone()]).with_void_rooms(rooms.clone());
    dispatch(&router, None, true).await.expect("learn");
    assert_eq!(rooms.get("only", "demo"), Some(32_768));

    let error = dispatch(&router, None, true).await.expect_err("window");
    assert!(error.contains("exceed context limit"), "{error}");
    assert_eq!(only.hits.load(Ordering::SeqCst), 3);
    assert_eq!(
        only.limits.lock().expect("limits").as_slice(),
        [None, Some(32_768), Some(32_768)]
    );
    assert_eq!(rooms.get("only", "demo"), Some(32_768));

    let answer = dispatch(&router, None, true).await.expect("kept");
    assert!(answer.contains("done"), "{answer}");
    assert_eq!(
        only.limits.lock().expect("limits").as_slice(),
        [None, Some(32_768), Some(32_768), Some(32_768)]
    );
    assert_eq!(rooms.get("only", "demo"), Some(32_768));
}

fn hop_router(member: Arc<dyn Member>, cap: u64, rooms: VoidRooms) -> Router {
    let mut hop = Hop::new(member);
    hop.max_output_tokens = Some(cap);
    Router::from_hops(
        BTreeMap::from([("demo".to_owned(), vec![hop])]),
        FailoverPolicy::default(),
    )
    .with_void_rooms(rooms)
}
