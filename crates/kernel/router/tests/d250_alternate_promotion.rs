//! Protocol alternates stay with primaries promoted after initial ordering (D-250).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::stream;
use hofund_ir::{
    ContentPart, Extensions, Message, Request, Response, Role, StreamEvent, TextDelta,
};
use hofund_plugin_api::{CountOutcome, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::affinity::{AffinityState, Conversation, Stays};
use hofund_router::{
    Completion, CooldownPolicy, Cooldowns, FailoverPolicy, Hop, Member, Route, Router,
};

const REFUSAL: &str = r#"{"type":"error","error":{"type":"permission_error","message":"This group does not allow /v1/messages dispatch"}}"#;

#[derive(Clone, Copy)]
enum Reply {
    Unary,
    Stream,
    Momentary,
    Quota,
    MessagesRefusal,
}

struct Fake {
    id: MemberId,
    route_id: &'static str,
    protocol: &'static str,
    stands_in_for: Option<String>,
    reply: Reply,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        self.protocol
    }

    fn stands_in_for(&self) -> Option<&str> {
        self.stands_in_for.as_deref()
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.seen
            .lock()
            .expect("seen")
            .push(format!("{}:{}", self.route_id, self.protocol));
        match self.reply {
            Reply::Unary => {
                let mut message = Message::new(Role::Assistant);
                message.content.push(ContentPart::text("chat ok"));
                Ok(Completion::Unary(Box::new(Response::new(message))))
            }
            Reply::Stream => Ok(Completion::Stream(Box::pin(stream::iter([Ok(
                StreamEvent::TextDelta(TextDelta {
                    index: 0,
                    text: "messages ok".to_owned(),
                    extensions: Extensions::new(),
                }),
            )])))),
            Reply::Momentary => Err(PluginError::status_error(503, "unavailable")),
            Reply::Quota => Err(PluginError::quota_status(429, "quota exhausted")),
            Reply::MessagesRefusal => Err(PluginError::status_error(403, REFUSAL)),
        }
    }

    async fn count_input(
        &self,
        _request: Request,
        _inbound: &str,
    ) -> Result<CountOutcome, PluginError> {
        self.seen
            .lock()
            .expect("seen")
            .push(format!("{}:{}", self.route_id, self.protocol));
        Ok(CountOutcome::Counted(9))
    }
}

fn hop(
    route_id: &'static str,
    key: &'static str,
    protocol: &'static str,
    reply: Reply,
    stands_in_for: Option<String>,
    seen: &Arc<Mutex<Vec<String>>>,
) -> Hop {
    Hop::new(Arc::new(Fake {
        id: MemberId::from_preset(PresetId::from_static(route_id), MemberKey::from_static(key)),
        route_id,
        protocol,
        stands_in_for,
        reply,
        seen: Arc::clone(seen),
    }))
}

fn relay_pair(seen: &Arc<Mutex<Vec<String>>>) -> Vec<Hop> {
    let messages = hop(
        "relay",
        "messages",
        "messages",
        Reply::MessagesRefusal,
        None,
        seen,
    );
    let primary = messages.member.id().to_string();
    vec![
        messages,
        hop("relay", "chat", "chat", Reply::Unary, Some(primary), seen),
    ]
}

#[tokio::test]
async fn d250_hand_on_primary_keeps_chat_until_messages_refuses() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    ));
    let probe = hop("probe", "row", "probe", Reply::Momentary, None, &seen);
    let mut hops = vec![probe];
    hops.extend(relay_pair(&seen));
    for (index, hop) in hops
        .iter()
        .filter(|hop| hop.member.stands_in_for().is_none())
        .enumerate()
    {
        cooldowns
            .record_failure_for_model(
                hop.member.id(),
                "model-a",
                &PluginError::status_error(429, "too many requests"),
            )
            .expect("rest");
        if index == 0 {
            *now.lock().expect("clock") += Duration::from_secs(10);
        }
    }
    let router = Router::from_routes(
        BTreeMap::from([(
            "model-a".to_owned(),
            Route::new(hops).with_routing("order").with_patience(None),
        )]),
        FailoverPolicy::default(),
    )
    .with_cooldowns(cooldowns);

    let routed = router
        .dispatch(&Request::new("model-a"), "messages")
        .await
        .expect("handed-on Messages refusal asks Chat");

    assert_eq!(routed.protocol, "chat");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["probe:probe", "relay:messages", "relay:chat"]
    );
}

#[tokio::test]
async fn d250_over_window_promotion_keeps_chat_after_messages_refuses() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut probe = hop("probe", "row", "probe", Reply::Quota, None, &seen);
    probe.context_window = Some(1_000_000);
    let mut hops = vec![probe];
    hops.extend(relay_pair(&seen).into_iter().map(|mut hop| {
        hop.context_window = Some(4_000);
        hop
    }));
    let router = Router::from_routes(
        BTreeMap::from([(
            "model-a".to_owned(),
            Route::new(hops).with_routing("fit").with_patience(None),
        )]),
        FailoverPolicy::default(),
    );
    let mut request = Request::new("model-a");
    let mut message = Message::new(Role::User);
    message.content.push(ContentPart::text("x".repeat(20_000)));
    request.messages.push(message);

    let routed = router
        .dispatch(&request, "messages")
        .await
        .expect("promoted Messages refusal asks Chat");

    assert_eq!(routed.protocol, "chat");
    assert_eq!(
        seen.lock().expect("seen").as_slice(),
        ["probe:probe", "relay:messages", "relay:chat"]
    );
}

#[tokio::test]
async fn d250_chat_answer_sticks_to_the_primary_for_affinity() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hops = relay_pair(&seen);
    let primary = hops[0].member.id().to_string();
    let affinity = AffinityState::in_memory();
    let router = Router::from_routes(
        BTreeMap::from([(
            "model-a".to_owned(),
            Route::new(hops)
                .with_stays(Stays::Strict)
                .with_patience(None),
        )]),
        FailoverPolicy::default(),
    )
    .with_affinity(affinity.clone());
    let first = Conversation::new("model-a|session", 1, false).with_named_session(true);
    router
        .dispatch_with_context(&Request::new("model-a"), "messages", Some(&first), None)
        .await
        .expect("Chat answers");

    let stick = affinity.stick(&first.key).expect("answered stick");
    assert_eq!(stick.member, primary);

    let next = Conversation::new("model-a|session", 2, false).with_named_session(true);
    let routed = router
        .dispatch_with_context(&Request::new("model-a"), "messages", Some(&next), None)
        .await
        .expect("the relay remains a candidate");
    assert_eq!(routed.affinity_reason.as_deref(), Some("strict"));

    seen.lock().expect("seen").clear();
    let count = router
        .count_scoped_conversation(&Request::new("model-a"), "messages", None, Some(&next))
        .await
        .expect("strict count keeps the primary and its active alternate");
    assert_eq!(count.tokens, 9);
    assert_eq!(seen.lock().expect("seen").as_slice(), ["relay:chat"]);
}

#[tokio::test]
async fn d250_successful_messages_stream_omits_an_idle_chat_continuation() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let messages = hop("relay", "messages", "messages", Reply::Stream, None, &seen);
    let primary = messages.member.id().to_string();
    let chat = hop("relay", "chat", "chat", Reply::Unary, Some(primary), &seen);
    let router = Router::from_routes(
        BTreeMap::from([(
            "model-a".to_owned(),
            Route::new(vec![messages, chat]).with_patience(None),
        )]),
        FailoverPolicy::default(),
    );

    let mut routed = router
        .dispatch(&Request::new("model-a"), "messages")
        .await
        .expect("Messages stream answers");

    assert_eq!(routed.protocol, "messages");
    assert!(routed.take_continuation().is_none());
    assert_eq!(seen.lock().expect("seen").as_slice(), ["relay:messages"]);
}
