//! Responses turns on Copilot's WebSocket, against a loopback socket.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hofund_kit_subscription::{
    LoopbackSocket, SocketScript, TestHost, advance_time, advance_until, block_on, block_on_paused,
};
use hofund_plugin_api::{
    AccountId, ContentPart, Conversation, Message, PluginError, PluginErrorKind, PluginId,
    PluginKind, ProtocolId, Request, Role, SecretString, UpstreamProvider,
};
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache};

use crate::models::Book;
use crate::session::MARGIN;
use crate::socket::MAX_PER_ACCOUNT;
use crate::upstream::CopilotUpstream;

const API: &str = "https://api.individual.githubcopilot.com";
const SEALED: &str = r#"{"type":"reasoning","id":"Y9Jw8X3JbNrD+gvjyaVn/JJSBK==","summary":[],"encrypted_content":"tInH0RH7w5ID5F7p"}"#;

struct World {
    _dir: tempfile::TempDir,
    host: TestHost,
    book: Arc<Book>,
    account: AccountId,
}

async fn world_with(host: TestHost, dir: tempfile::TempDir) -> World {
    let account = AccountId::from_static("a-0000000000000001");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &SecretString::from("gho-1".to_owned()))
        .with_text("host", "github.com")
        .with_text("login", "octo");
    let accounts = host.context().accounts();
    let stored = accounts.account(account.clone(), "octo").expect("account");
    accounts
        .upsert(stored, blob.to_secret())
        .await
        .expect("upsert");
    World {
        _dir: dir,
        host,
        book: Arc::new(Book::default()),
        account,
    }
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("copilot"),
        dir.path(),
    );
    block_on(world_with(host, dir))
}

impl World {
    fn upstream(&self) -> CopilotUpstream {
        CopilotUpstream::new(
            self.account.clone(),
            self.host.context().clone(),
            Arc::new(TokenCache::new(MARGIN)),
            Arc::clone(&self.book),
        )
    }

    fn prime(&self, token: &str) {
        self.host.http().enqueue(session_reply(token));
        if self.book.get(&self.account).is_none() {
            self.host.http().enqueue(models_reply());
        }
    }
}

fn session_reply(token: &str) -> Vec<u8> {
    json!({
        "token": token,
        "expires_at": 100_000,
        "endpoints": {"api": API}
    })
    .to_string()
    .into_bytes()
}

fn models_reply() -> Vec<u8> {
    json!({"data": [
        {"id": "gpt-5.5", "name": "GPT-5.5", "model_picker_enabled": true,
         "policy": {"state": "enabled"},
         "supported_endpoints": ["/responses", "ws:/responses"],
         "capabilities": {"type": "chat"}},
        {"id": "gpt-5.3-codex", "name": "GPT-5.3-Codex", "model_picker_enabled": true,
         "policy": {"state": "enabled"},
         "supported_endpoints": ["/responses"],
         "capabilities": {"type": "chat"}}
    ]})
    .to_string()
    .into_bytes()
}

fn http_reply() -> Vec<u8> {
    br#"{"id":"r1","object":"response","status":"completed","model":"gpt-5.5","output":[]}"#
        .to_vec()
}

fn say(role: Role, text: &str) -> Message {
    let mut message = Message::new(role);
    message.content.push(ContentPart::text(text));
    message
}

fn request(model: &str, messages: Vec<Message>) -> Request {
    let mut request = Request::new(model).with_messages(messages);
    request.stream = true;
    request
}

fn assistant(text: &str) -> String {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text}]
    })
    .to_string()
}

fn completed(id: &str, assistant_item: &str, leading: &[&str]) -> Vec<String> {
    let mut frames = Vec::new();
    for item in leading {
        frames.push(format!(
            r#"{{"type":"response.output_item.done","item":{item}}}"#
        ));
    }
    frames.push(format!(
        r#"{{"type":"response.output_item.done","item":{assistant_item}}}"#
    ));
    frames.push(format!(
        r#"{{"type":"response.completed","response":{{"id":"{id}","object":"response","status":"completed","output":[]}}}}"#
    ));
    frames
}

fn replies(id_of: impl Fn(usize) -> &'static str + Send + Sync + 'static) -> SocketScript {
    let item = assistant("pong");
    SocketScript::replies(move |turn, _| completed(id_of(turn), &item, &[]))
}

async fn ask(upstream: &CopilotUpstream, request: Request) {
    upstream
        .execute(request, ProtocolId::OPENAI_RESPONSES)
        .await
        .expect("turn");
}

fn posts(host: &TestHost) -> usize {
    host.http()
        .calls()
        .iter()
        .filter(|call| call.uri().ends_with("/responses"))
        .count()
}

fn body(text: &str) -> Value {
    json::from_str(text).expect("socket message")
}

fn input_text(value: &Value) -> &str {
    value
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[test]
fn d235_socket_only_when_listed() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(replies(|_| "resp-1")).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        world.host.http().enqueue(http_reply());
        let upstream = world.upstream();
        ask(
            &upstream,
            request("gpt-5.3-codex", vec![say(Role::User, "ping")]),
        )
        .await;
        ask(
            &upstream,
            request(
                "gpt-5.3-codex",
                vec![
                    say(Role::User, "ping"),
                    say(Role::Assistant, "pong"),
                    say(Role::User, "again"),
                ],
            ),
        )
        .await;
        assert_eq!(posts(&world.host), 2);
        assert_eq!(server.opened(), 0);
    });
}

#[test]
fn d235_second_turn_sends_previous_response_and_delta() {
    let world = world();
    block_on(async {
        let server =
            LoopbackSocket::start(replies(|turn| if turn == 0 { "resp-1" } else { "resp-2" }))
                .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        let upstream = world.upstream();
        ask(&upstream, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        ask(
            &upstream,
            request(
                "gpt-5.5",
                vec![
                    say(Role::User, "ping"),
                    say(Role::Assistant, "pong"),
                    say(Role::User, "again"),
                ],
            ),
        )
        .await;
        assert_eq!(posts(&world.host), 0);
        assert_eq!(server.opened(), 1);
        let frames: Vec<String> = server
            .hits()
            .into_iter()
            .flat_map(|hit| hit.messages().to_vec())
            .collect();
        assert_eq!(frames.len(), 2, "{frames:?}");
        let first = body(&frames[0]);
        let next = body(&frames[1]);
        assert_eq!(first["type"], "response.create");
        assert!(first.get("stream").is_none(), "{first}");
        assert_eq!(first["model"], "gpt-5.5");
        assert!(first.get("previous_response_id").is_none(), "{first}");
        assert_eq!(first["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(next["previous_response_id"], "resp-1", "{next}");
        let input = next["input"].as_array().expect("input");
        assert_eq!(input.len(), 1, "{next}");
        assert_eq!(input_text(&input[0]), "again");
    });
}

#[test]
fn d235_handshake_uses_account_headers() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(replies(|_| "resp-1")).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        let hit = server.hits().into_iter().next().expect("handshake");
        assert!(hit.header("OpenAI-Beta").is_none(), "{:?}", hit.headers());
        assert!(hit.header("X-Initiator").is_none(), "{:?}", hit.headers());
        assert_eq!(hit.header("Authorization"), Some("Bearer sess"));
        assert_eq!(hit.header("Copilot-Integration-Id"), Some("vscode-chat"));
        let message = body(hit.messages().first().expect("message"));
        assert_eq!(message["type"], "response.create");
        assert!(message.get("stream").is_none(), "{message}");
        assert_eq!(message["initiator"], "user");
    });
}

#[test]
fn d235_refused_handshake_uses_http() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(SocketScript::refuse(404)).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        world.host.http().enqueue(http_reply());
        let upstream = world.upstream();
        ask(&upstream, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        ask(
            &upstream,
            request(
                "gpt-5.5",
                vec![
                    say(Role::User, "ping"),
                    say(Role::Assistant, "pong"),
                    say(Role::User, "again"),
                ],
            ),
        )
        .await;
        assert_eq!(posts(&world.host), 2);
        let frames: usize = server.hits().iter().map(|hit| hit.messages().len()).sum();
        assert_eq!(frames, 0);
    });
}

#[test]
fn d235_statusless_error_frame_uses_http() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(SocketScript::replies(|_, _| {
            vec![r#"{"type":"error","error":{"type":"invalid_request_error","code":"bad_request","message":"previous_response_id is not supported"}}"#.to_owned()]
        }))
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        assert_eq!(posts(&world.host), 1);
        assert_eq!(server.opened(), 1);
        assert_eq!(server.hits()[0].messages().len(), 1);
    });
}

#[test]
fn d235_socket_keyed_by_session_token() {
    let world = world();
    block_on(async {
        let issued = Arc::new(AtomicUsize::new(0));
        let ids = Arc::clone(&issued);
        let server = LoopbackSocket::start(SocketScript::replies(move |turn, _| {
            let id = if turn == 0 {
                match ids.fetch_add(1, Ordering::SeqCst) {
                    0 => "resp-b",
                    _ => "resp-a",
                }
            } else {
                "resp-next"
            };
            completed(id, &assistant("pong"), &[])
        }))
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        let first = world.upstream();
        let second = world.upstream();
        world.prime("token-b");
        ask(&first, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        world.prime("token-a");
        ask(&second, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        ask(
            &second,
            request(
                "gpt-5.5",
                vec![
                    say(Role::User, "ping"),
                    say(Role::Assistant, "pong"),
                    say(Role::User, "again"),
                ],
            ),
        )
        .await;
        assert_eq!(server.opened(), 2);
        let hits = server.hits();
        let bearer = |token: &str| {
            hits.iter()
                .find(|hit| hit.header("Authorization") == Some(token))
                .expect("handshake")
        };
        assert_eq!(bearer("Bearer token-b").messages().len(), 1);
        let kept = bearer("Bearer token-a");
        assert_eq!(kept.messages().len(), 2);
        let next = body(&kept.messages()[1]);
        assert_eq!(next["previous_response_id"], "resp-a", "{next}");
    });
}

#[test]
fn d235_delta_without_base64_reasoning() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(SocketScript::replies(|turn, _| {
            let id = if turn == 0 { "resp-1" } else { "resp-2" };
            let leading: &[&str] = if turn == 0 { &[SEALED] } else { &[] };
            completed(id, &assistant("pong"), leading)
        }))
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        let upstream = world.upstream();
        ask(&upstream, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        ask(
            &upstream,
            request(
                "gpt-5.5",
                vec![
                    say(Role::User, "ping"),
                    say(Role::Assistant, "pong"),
                    say(Role::User, "again"),
                ],
            ),
        )
        .await;
        assert_eq!(posts(&world.host), 0);
        assert_eq!(server.opened(), 1);
        let frames: Vec<String> = server
            .hits()
            .into_iter()
            .flat_map(|hit| hit.messages().to_vec())
            .collect();
        assert_eq!(frames.len(), 2, "{frames:?}");
        let next = body(&frames[1]);
        assert_eq!(next["previous_response_id"], "resp-1", "{next}");
        let input = next["input"].as_array().expect("input");
        assert_eq!(input.len(), 1, "{next}");
        assert_eq!(input_text(&input[0]), "again");
    });
}

#[test]
fn d235_idle_socket_is_not_reused() {
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for_with_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("copilot"),
            dir.path(),
        );
        let server =
            LoopbackSocket::start(replies(|turn| if turn == 0 { "resp-1" } else { "resp-2" }))
                .await;
        let world = world_with(host, dir).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        let upstream = world.upstream();
        let mut first = request("gpt-5.5", vec![say(Role::User, "ping")]);
        first.conversation = Some(Conversation::new("s1", ""));
        ask_paused(&upstream, first).await;
        assert_eq!(server.opened(), 1);
        assert_eq!(world.book.sockets.held(&world.account), 1);
        assert_eq!(posts(&world.host), 0);
        assert!(!server.hits()[0].closed());
        advance_time(Duration::from_secs(11 * 60)).await;
        wait_until_closed(&server).await;
        assert!(server.hits()[0].closed());
        assert_eq!(server.hits()[0].messages().len(), 1);
        assert_eq!(server.opened(), 1);
        let mut second = request(
            "gpt-5.5",
            vec![
                say(Role::User, "ping"),
                say(Role::Assistant, "pong"),
                say(Role::User, "again"),
            ],
        );
        second.conversation = Some(Conversation::new("s1", ""));
        ask_paused(&upstream, second).await;
        assert_eq!(server.opened(), 2);
        let frames: Vec<String> = server
            .hits()
            .into_iter()
            .flat_map(|hit| hit.messages().to_vec())
            .collect();
        assert_eq!(frames.len(), 2, "{frames:?}");
        let next = body(&frames[1]);
        assert!(next.get("previous_response_id").is_none(), "{next}");
    });
}

#[test]
fn d235_account_socket_cap() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(replies(|_| "resp-1")).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        let upstream = world.upstream();
        for index in 0..=MAX_PER_ACCOUNT {
            let mut request = request("gpt-5.5", vec![say(Role::User, "ping")]);
            request.conversation = Some(Conversation::new(format!("c{index}"), ""));
            ask(&upstream, request).await;
        }
        assert_eq!(server.opened(), MAX_PER_ACCOUNT + 1);
        assert_eq!(world.book.sockets.held(&world.account), MAX_PER_ACCOUNT);
    });
}

#[test]
fn d235_close_before_a_terminal_event_uses_http() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(
            SocketScript::replies(|_, _| {
                vec![r#"{"type":"response.output_text.delta","delta":"hi"}"#.to_owned()]
            })
            .then_close(),
        )
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        assert_eq!(posts(&world.host), 1);
        assert_eq!(world.book.sockets.held(&world.account), 0);
        assert_eq!(server.opened(), 1);
    });
}

#[test]
fn d235_read_budget_falls_back_before_a_terminal_event() {
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for_with_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("copilot"),
            dir.path(),
        );
        let server = LoopbackSocket::start(SocketScript::replies(|_, _| {
            vec![r#"{"type":"response.output_text.delta","delta":"hi"}"#.to_owned()]
        }))
        .await;
        let world = world_with(host, dir).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        let started = world.host.context().clock().monotonic();
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        let elapsed = world
            .host
            .context()
            .clock()
            .monotonic()
            .saturating_sub(started);
        assert!(
            elapsed >= Duration::from_secs(60) && elapsed < Duration::from_secs(90),
            "{elapsed:?}"
        );
        assert_eq!(posts(&world.host), 1);
        assert_eq!(world.book.sockets.held(&world.account), 0);
    });
}

#[test]
fn d235_accumulated_turn_over_the_cap_uses_http() {
    let world = world();
    block_on(async {
        let chunk = "a".repeat(5_000_000);
        let frame = format!(r#"{{"type":"response.output_text.delta","delta":"{chunk}"}}"#);
        let server = LoopbackSocket::start(SocketScript::replies(move |_, _| {
            vec![frame.clone(), frame.clone()]
        }))
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        assert_eq!(posts(&world.host), 1);
        assert_eq!(world.book.sockets.held(&world.account), 0);
    });
}

#[test]
fn d235_max_age_closes_without_another_turn() {
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for_with_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("copilot"),
            dir.path(),
        );
        let server = LoopbackSocket::start(replies(|turn| match turn {
            0 => "resp-0",
            1 => "resp-1",
            2 => "resp-2",
            3 => "resp-3",
            4 => "resp-4",
            5 => "resp-5",
            _ => "resp-6",
        }))
        .await;
        let world = world_with(host, dir).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        let upstream = world.upstream();
        let opened = world.host.context().clock().monotonic();
        let mut used = opened;
        let mut messages = vec![say(Role::User, "ping")];
        let mut turn = request("gpt-5.5", messages.clone());
        turn.conversation = Some(Conversation::new("age", ""));
        ask_paused(&upstream, turn).await;
        assert_eq!(world.book.sockets.held(&world.account), 1);
        assert_eq!(posts(&world.host), 0);
        let mut index = 0_u32;
        loop {
            let now = world.host.context().clock().monotonic();
            let age_left = Duration::from_secs(55 * 60).saturating_sub(now.saturating_sub(opened));
            if age_left <= Duration::from_secs(2 * 60) {
                break;
            }
            let idle_left = Duration::from_secs(10 * 60).saturating_sub(now.saturating_sub(used));
            let step = idle_left
                .saturating_sub(Duration::from_secs(90))
                .min(Duration::from_secs(8 * 60))
                .min(age_left.saturating_sub(Duration::from_secs(2 * 60)));
            if step < Duration::from_secs(30) {
                break;
            }
            advance_time(step).await;
            assert!(!server.hits()[0].closed(), "closed before max age");
            messages.push(say(Role::Assistant, "pong"));
            messages.push(say(Role::User, &format!("n{index}")));
            index += 1;
            used = world.host.context().clock().monotonic();
            let mut turn = request("gpt-5.5", messages.clone());
            turn.conversation = Some(Conversation::new("age", ""));
            ask_paused(&upstream, turn).await;
            assert_eq!(server.opened(), 1, "turn {index} opened a new socket");
        }
        let now = world.host.context().clock().monotonic();
        assert!(
            now.saturating_sub(opened) > Duration::from_secs(10 * 60),
            "max age was not reached past the idle window"
        );
        assert!(!server.hits()[0].closed());
        let sent = server.hits()[0].messages().len();
        let age_left = Duration::from_secs(55 * 60).saturating_sub(now.saturating_sub(opened));
        advance_time(age_left.saturating_add(Duration::from_secs(30))).await;
        wait_until_closed(&server).await;
        assert!(server.hits()[0].closed());
        assert_eq!(server.opened(), 1);
        assert_eq!(server.hits()[0].messages().len(), sent);
        assert_eq!(posts(&world.host), 0);
    });
}

#[test]
fn d235_proxied_profile_stays_on_http() {
    let world = world();
    block_on(async {
        let server = LoopbackSocket::start(replies(|_| "resp-1")).await;
        world.host.http().use_http_proxy("http://127.0.0.1:9");
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        assert_eq!(server.opened(), 0);
        assert_eq!(posts(&world.host), 1);
        assert_eq!(world.book.sockets.held(&world.account), 0);
    });
}

#[test]
fn d235_status_error_is_classified_like_http() {
    let world = world();
    block_on(async {
        let step = Arc::new(AtomicUsize::new(0));
        let server = LoopbackSocket::start(SocketScript::replies(move |_, _| {
            let index = step.fetch_add(1, Ordering::SeqCst);
            let frame = if index == 0 {
                r#"{"type":"error","status":429,"error":{"message":"You exceeded your plan's usage limit. Try again in 2h","code":"usage_limit_reached"}}"#
            } else {
                r#"{"type":"error","status":402,"error":{"message":"insufficient balance","code":"insufficient_quota"}}"#
            };
            vec![frame.to_owned()]
        }))
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        let upstream = world.upstream();
        let quota = turn_error(&upstream, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        assert_eq!(quota.kind(), PluginErrorKind::Quota);
        assert_eq!(quota.status(), Some(429));
        assert!(
            quota.message().contains("usage_limit_reached"),
            "{}",
            quota.message()
        );
        assert_eq!(quota.retry_after(), Some(Duration::from_secs(2 * 60 * 60)));
        let credit = turn_error(&upstream, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        assert_eq!(credit.kind(), PluginErrorKind::Status);
        assert!(credit.is_credit());
        assert_eq!(credit.status(), Some(402));
        assert!(
            credit.message().contains("insufficient_quota"),
            "{}",
            credit.message()
        );
        assert_eq!(posts(&world.host), 0);
        assert_eq!(world.book.sockets.held(&world.account), 0);
    });
}

#[test]
fn d235_used_is_stamped_when_the_turn_finishes() {
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for_with_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("copilot"),
            dir.path(),
        );
        let server = LoopbackSocket::start(replies(|turn| match turn {
            0 => "resp-0",
            _ => "resp-1",
        }))
        .await;
        let world = world_with(host, dir).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        let upstream = world.upstream();
        let mut first = request("gpt-5.5", vec![say(Role::User, "ping")]);
        first.conversation = Some(Conversation::new("used", ""));
        let started = world.host.context().clock().monotonic();
        ask_paused(&upstream, first).await;
        let finished = world.host.context().clock().monotonic();
        assert!(finished.saturating_sub(started) >= Duration::from_secs(1));
        assert_eq!(world.book.sockets.held(&world.account), 1);
        advance_time(Duration::from_secs(10 * 60) - Duration::from_secs(1)).await;
        let mut second = request(
            "gpt-5.5",
            vec![
                say(Role::User, "ping"),
                say(Role::Assistant, "pong"),
                say(Role::User, "next"),
            ],
        );
        second.conversation = Some(Conversation::new("used", ""));
        ask_paused(&upstream, second).await;
        assert_eq!(server.opened(), 1);
        assert_eq!(posts(&world.host), 0);
        assert_eq!(world.book.sockets.held(&world.account), 1);
    });
}

#[test]
fn d235_skipped_frames_count_toward_the_cap() {
    let world = world();
    block_on(async {
        let chunk = "a".repeat(5_000_000);
        let timing = format!(r#"{{"type":"responsesapi.websocket_timing","payload":"{chunk}"}}"#);
        let item = assistant("pong");
        let server = LoopbackSocket::start(SocketScript::replies(move |_, _| {
            let mut frames = vec![timing.clone(), timing.clone()];
            frames.extend(completed("resp-1", &item, &[]));
            frames
        }))
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        assert_eq!(posts(&world.host), 1);
        assert_eq!(world.book.sockets.held(&world.account), 0);
    });
}

#[test]
fn d235_event_type_is_counted_once_in_the_cap() {
    let world = world();
    block_on(async {
        let kind = "t".repeat(4_500_000);
        let wide = format!(r#"{{"type":"{kind}"}}"#);
        let item = assistant("pong");
        let server = LoopbackSocket::start(SocketScript::replies(move |_, _| {
            let mut frames = vec![wide.clone()];
            frames.extend(completed("resp-1", &item, &[]));
            frames
        }))
        .await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        ask(
            &world.upstream(),
            request("gpt-5.5", vec![say(Role::User, "ping")]),
        )
        .await;
        assert_eq!(posts(&world.host), 1);
        assert_eq!(world.book.sockets.held(&world.account), 0);
    });
}

#[test]
fn d235_timing_frames_do_not_refresh_the_read_budget() {
    block_on_paused(async {
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for_with_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("copilot"),
            dir.path(),
        );
        let item = assistant("pong");
        let server = LoopbackSocket::start(
            SocketScript::replies(move |_, _| {
                let mut frames = vec![
                    r#"{"type":"responsesapi.websocket_timing"}"#.to_owned(),
                    r#"{"type":"responsesapi.websocket_timing"}"#.to_owned(),
                ];
                frames.extend(completed("resp-1", &item, &[]));
                frames
            })
            .gap(Duration::from_secs(31)),
        )
        .await;
        let world = world_with(host, dir).await;
        world.host.http().serve_websockets_at(server.base_url());
        world.prime("sess");
        world.host.http().enqueue(http_reply());
        let upstream = world.upstream();
        let started = world.host.context().clock().monotonic();
        ask_paused(&upstream, request("gpt-5.5", vec![say(Role::User, "ping")])).await;
        let elapsed = world
            .host
            .context()
            .clock()
            .monotonic()
            .saturating_sub(started);
        assert!(
            elapsed >= Duration::from_secs(61) && elapsed < Duration::from_secs(80),
            "{elapsed:?}"
        );
        assert_eq!(posts(&world.host), 1);
        assert_eq!(world.book.sockets.held(&world.account), 0);
    });
}

async fn ask_paused(upstream: &CopilotUpstream, request: Request) {
    let turned = advance_until(
        upstream.execute(request, ProtocolId::OPENAI_RESPONSES),
        Duration::from_millis(1),
    )
    .await;
    assert!(turned.is_ok(), "turn");
}

async fn wait_until_closed(server: &LoopbackSocket) {
    for _ in 0..64 {
        if server.hits().iter().any(|hit| hit.closed()) {
            return;
        }
        hofund_plugin_sdk::sleep(Duration::ZERO).await;
    }
    panic!("the socket stayed open");
}

async fn turn_error(upstream: &CopilotUpstream, request: Request) -> PluginError {
    match upstream
        .execute(request, ProtocolId::OPENAI_RESPONSES)
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("the error frame was treated as a reply"),
    }
}
