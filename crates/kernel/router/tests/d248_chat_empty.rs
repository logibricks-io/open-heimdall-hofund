//! Chat empty-reply retry and failover with deterministic fakes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use hofund_codec::CHAT_PROTOCOL_ID;
use hofund_ir::{
    Extensions, Message, MessageDelta, MessageStop, Request, Response, Role, StreamEvent, Usage,
};
use hofund_plugin_api::{BodyStream, MemberId, MemberKey, PluginError, PresetId, ProtocolId};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Member, Router, RouterError,
};

const VOID_DONE: &[u8] = concat!(
    "data: {\"id\":\"void\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"\",\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"void\",\"model\":\"m\",\"choices\":[],\"usage\":{\"completion_tokens\":2359,\"prompt_tokens\":91000,\"reasoning_tokens\":2351}}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const VOID_EOF: &[u8] = concat!(
    "data: {\"id\":\"void\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"\",\"role\":\"assistant\"}}]}\n\n",
    "data: {\"id\":\"void\",\"model\":\"m\",\"choices\":[],\"usage\":{\"completion_tokens\":2359,\"prompt_tokens\":91000,\"reasoning_tokens\":2351}}\n\n"
)
.as_bytes();
const TEXT_REPLY: &[u8] = concat!(
    "data: {\"id\":\"reply\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
    "data: {\"id\":\"reply\",\"model\":\"m\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();
const TEXT_WITHOUT_FINISH: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
const REASONING_WITHOUT_FINISH: &[u8] =
    b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thought\"}}]}\n\n";
const EMPTY_STOP: &[u8] = concat!(
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n"
)
.as_bytes();

#[derive(Clone)]
enum Reply {
    Raw(&'static [u8]),
    EmptyEvents,
    Unary,
}

struct Fake {
    route_id: &'static str,
    member_id: MemberId,
    protocol: &'static str,
    stamp_chat: bool,
    hits: Arc<AtomicUsize>,
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
        self.protocol
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        let hit = self.hits.fetch_add(1, Ordering::SeqCst);
        let completion = match &self.replies[hit.min(self.replies.len() - 1)] {
            Reply::Raw(bytes) => Completion::Raw(BodyStream::from_bytes(Bytes::from_static(bytes))),
            Reply::EmptyEvents => Completion::Stream(Box::pin(stream::iter(vec![
                Ok(StreamEvent::MessageDelta(MessageDelta {
                    stop: None,
                    usage: Some(Usage {
                        output_tokens: Some(2359),
                        reasoning_tokens: Some(2351),
                        ..Usage::default()
                    }),
                    extensions: Extensions::new(),
                })),
                Ok(StreamEvent::MessageStop(MessageStop::default())),
            ]))),
            Reply::Unary => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(hofund_ir::ContentPart::text("hello"));
                Completion::Unary(Box::new(Response::new(message)))
            }
        };
        if self.stamp_chat {
            Ok(completion.produced(ProtocolId::OPENAI_CHAT))
        } else {
            Ok(completion)
        }
    }
}

fn fake(route_id: &'static str, hits: &Arc<AtomicUsize>, replies: Vec<Reply>) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id,
        member_id: MemberId::from_preset(
            PresetId::try_from(route_id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        protocol: CHAT_PROTOCOL_ID,
        stamp_chat: false,
        hits: Arc::clone(hits),
        replies,
    })
}

fn tagged(route_id: &'static str, hits: &Arc<AtomicUsize>, replies: Vec<Reply>) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id,
        member_id: MemberId::from_preset(
            PresetId::try_from(route_id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        protocol: "subscription",
        stamp_chat: true,
        hits: Arc::clone(hits),
        replies,
    })
}

fn router(members: Vec<Arc<dyn Member>>) -> Router {
    Router::new(
        BTreeMap::from([("demo".to_owned(), members)]),
        FailoverPolicy::default(),
    )
}

async fn dispatch(router: &Router) -> Result<hofund_router::Routed, RouterError> {
    router
        .dispatch(&Request::new("demo"), CHAT_PROTOCOL_ID)
        .await
}

async fn raw_bytes(completion: Completion) -> Vec<u8> {
    let Completion::Raw(mut body) = completion else {
        panic!("expected raw reply");
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        bytes.extend_from_slice(&chunk.expect("chunk"));
    }
    bytes
}

#[tokio::test]
async fn d248_raw_void_retries_then_returns_only_the_reply() {
    let hits = Arc::new(AtomicUsize::new(0));
    let router = router(vec![fake(
        "only",
        &hits,
        vec![Reply::Raw(VOID_DONE), Reply::Raw(TEXT_REPLY)],
    )]);
    let routed = dispatch(&router).await.expect("reply");
    assert_eq!(raw_bytes(routed.completion).await, TEXT_REPLY);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn d248_raw_void_fails_over_after_one_retry_without_leaking_bytes() {
    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &first,
            vec![Reply::Raw(VOID_DONE), Reply::Raw(VOID_DONE)],
        ),
        fake("second", &second, vec![Reply::Raw(TEXT_REPLY)]),
    ]);
    let routed = dispatch(&router).await.expect("failover");
    assert_eq!(routed.upstream_id, "second");
    let body = raw_bytes(routed.completion).await;
    assert_eq!(body, TEXT_REPLY);
    assert!(!String::from_utf8_lossy(&body).contains("void"));
    assert_eq!(first.load(Ordering::SeqCst), 2);
    assert_eq!(second.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn d248_done_and_eof_void_errors_count_usage_and_do_not_rest() {
    for void in [VOID_DONE, VOID_EOF] {
        let hits = Arc::new(AtomicUsize::new(0));
        let router = router(vec![fake(
            "only",
            &hits,
            vec![Reply::Raw(void), Reply::Raw(void)],
        )]);
        let error = match dispatch(&router).await {
            Err(RouterError::Upstream { error, .. }) => error,
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("expected empty reply"),
        };
        assert_eq!(error.status(), Some(502));
        assert!(!error.cools());
        assert!(
            error
                .message()
                .contains("the model reasoned for 2351 of the 2359 output tokens it counted")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn d248_decoded_void_retries_and_fails_over() {
    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));
    let router = router(vec![
        fake(
            "first",
            &first,
            vec![Reply::EmptyEvents, Reply::EmptyEvents],
        ),
        fake("second", &second, vec![Reply::Unary]),
    ]);
    let routed = dispatch(&router).await.expect("failover");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(first.load(Ordering::SeqCst), 2);
    assert_eq!(second.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn d248_non_chat_label_void_retries_fails_over_without_a_rest_or_leaked_bytes() {
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let first = tagged(
        "first",
        &first_hits,
        vec![Reply::Raw(VOID_DONE), Reply::Raw(VOID_DONE)],
    );
    let second = tagged("second", &second_hits, vec![Reply::Raw(TEXT_REPLY)]);
    assert_ne!(first.protocol(), CHAT_PROTOCOL_ID);
    assert_ne!(second.protocol(), CHAT_PROTOCOL_ID);
    let book = Arc::new(Cooldowns::new(CooldownPolicy::default()));
    let router = router(vec![first, second]).with_cooldowns(Arc::clone(&book));
    let routed = dispatch(&router).await.expect("failover");
    assert_eq!(routed.upstream_id, "second");
    let body = raw_bytes(routed.completion).await;
    assert_eq!(body, TEXT_REPLY);
    assert!(!String::from_utf8_lossy(&body).contains("void"));
    assert_eq!(first_hits.load(Ordering::SeqCst), 2);
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    let rested = MemberId::from_preset(
        PresetId::try_from("first").expect("preset"),
        MemberKey::from_static("row"),
    );
    assert_eq!(book.cooling(&rested), None);
}

#[tokio::test]
async fn d248_text_reasoning_and_stop_empty_pass_without_retry() {
    for reply in [TEXT_WITHOUT_FINISH, REASONING_WITHOUT_FINISH, EMPTY_STOP] {
        let hits = Arc::new(AtomicUsize::new(0));
        let router = router(vec![fake("only", &hits, vec![Reply::Raw(reply)])]);
        let routed = dispatch(&router).await.expect("reply");
        assert_eq!(raw_bytes(routed.completion).await, reply);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
