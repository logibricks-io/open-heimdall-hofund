//! Copilot Chat `reasoning_opaque` kept across a tool turn.

use std::sync::Arc;

use hofund_kit_subscription::{Http1Response, Http1Server, TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, ContentPart, Extensions, Message, PluginId, PluginKind, ProtocolId,
    Request, Role, Tool, ToolArguments, ToolCallPart, ToolResultPart, UpstreamProvider,
};
use hofund_plugin_sdk::futures::StreamExt;
use hofund_plugin_sdk::json::{self, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache};
use hofund_plugin_sdk::wire::{self, Target};

use crate::models::Book;
use crate::reasoning;
use crate::session::MARGIN;
use crate::upstream::CopilotUpstream;

const API: &str = "https://api.individual.githubcopilot.com";
const MODEL: &str = "gemini-4-hydrogen";
const OTHER_MODEL: &str = "gemini-4-carbon";
const PARIS: &str = "call_paris";
const TOKYO: &str = "call_tokyo";
const SEAL: &str = "WY6FaA+/=d262";

struct Fix {
    _dir: tempfile::TempDir,
    host: TestHost,
    book: Arc<Book>,
}

fn fix() -> Fix {
    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("copilot"),
        dir.path(),
    );
    Fix {
        _dir: dir,
        host,
        book: Arc::new(Book::default()),
    }
}

impl Fix {
    fn account(&self, id: &'static str, oauth: &str) -> CopilotUpstream {
        let account = AccountId::from_static(id);
        let blob = SecretObject::new()
            .with_number("v", 1)
            .with_secret("oauth_token", &oauth.to_owned().into())
            .with_text("host", "github.com")
            .with_text("login", "octo")
            .to_secret();
        block_on(async {
            let accounts = self.host.context().accounts();
            let row = accounts.account(account.clone(), "octo").expect("account");
            accounts.upsert(row, blob).await.expect("seed");
        });
        self.upstream(id)
    }

    fn upstream(&self, id: &'static str) -> CopilotUpstream {
        CopilotUpstream::new(
            AccountId::from_static(id),
            self.host.context().clone(),
            Arc::new(TokenCache::new(MARGIN)),
            Arc::clone(&self.book),
        )
    }

    fn enqueue_session(&self) {
        self.host.http().enqueue(
            json!({
                "token": "tid=session",
                "expires_at": 4_000_000_000_u64,
                "endpoints": {"api": API}
            })
            .to_string()
            .into_bytes(),
        );
    }

    fn prime(&self) {
        self.enqueue_session();
        self.host.http().enqueue(br#"{"data":[]}"#.to_vec());
    }
}

fn tool() -> Tool {
    Tool {
        name: "get_weather".to_owned(),
        description: None,
        input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        extensions: Extensions::new(),
    }
}

fn call(id: &str, city: &str) -> ContentPart {
    ContentPart::ToolCall(ToolCallPart {
        id: id.to_owned(),
        name: "get_weather".to_owned(),
        arguments: ToolArguments::from_text(&format!(r#"{{"city":"{city}"}}"#)),
        extensions: Extensions::new(),
    })
}

fn result(id: &str) -> ContentPart {
    ContentPart::ToolResult(ToolResultPart {
        id: id.to_owned(),
        is_error: false,
        content: vec![ContentPart::text("sunny")],
        extensions: Extensions::new(),
    })
}

fn opening(model: &str, stream: bool) -> Request {
    let mut user = Message::new(Role::User);
    user.content
        .push(ContentPart::text("weather in Paris and Tokyo?"));
    Request::new(model)
        .with_messages(vec![user])
        .with_tools(vec![tool()])
        .with_stream(stream)
}

fn follow_up(model: &str, stream: bool, own: Option<&str>) -> Request {
    let mut user = Message::new(Role::User);
    user.content
        .push(ContentPart::text("weather in Paris and Tokyo?"));
    let mut assistant = Message::new(Role::Assistant);
    assistant.content.push(call(PARIS, "Paris"));
    assistant.content.push(call(TOKYO, "Tokyo"));
    if let Some(own) = own {
        assistant.extensions.insert("reasoning_opaque", json!(own));
    }
    let mut results = Message::new(Role::User);
    results.content.push(result(PARIS));
    results.content.push(result(TOKYO));
    Request::new(model)
        .with_messages(vec![user, assistant, results])
        .with_tools(vec![tool()])
        .with_stream(stream)
}

fn other_call(model: &str) -> Request {
    let mut user = Message::new(Role::User);
    user.content.push(ContentPart::text("a"));
    let mut assistant = Message::new(Role::Assistant);
    assistant.content.push(call("call_other", "Oslo"));
    let mut results = Message::new(Role::User);
    results.content.push(result("call_other"));
    Request::new(model)
        .with_messages(vec![user, assistant, results])
        .with_tools(vec![tool()])
        .with_stream(true)
}

fn sse(lines: &[&str]) -> Vec<u8> {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push_str("\n\n");
    }
    out.into_bytes()
}

fn tool_reply(model: &str, stream: bool) -> Vec<u8> {
    if stream {
        return sse(&[
            &format!(
                r#"data: {{"id":"h1","model":"{model}","choices":[{{"index":0,"delta":{{"role":"assistant","content":null,"reasoning_opaque":"{SEAL}","tool_calls":[{{"index":0,"id":"{PARIS}","type":"function","function":{{"name":"get_weather","arguments":"{{\"city\":\"Paris\"}}"}}}}]}}}}]}}"#
            ),
            &format!(
                r#"data: {{"id":"h1","model":"{model}","choices":[{{"index":0,"delta":{{"role":"assistant","content":null,"tool_calls":[{{"index":1,"id":"{TOKYO}","type":"function","function":{{"name":"get_weather","arguments":"{{\"city\":\"Tokyo\"}}"}}}}]}}}}]}}"#
            ),
            &format!(
                r#"data: {{"id":"h1","model":"{model}","choices":[{{"index":0,"finish_reason":"tool_calls","delta":{{"role":"assistant","content":null}}}}],"usage":{{"prompt_tokens":107,"completion_tokens":88,"total_tokens":195}}}}"#
            ),
            "data: [DONE]",
        ]);
    }
    json!({
        "id": "h1",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning_opaque": SEAL,
                "tool_calls": [
                    {"id": PARIS, "type": "function", "function": {"name": "get_weather", "arguments": r#"{"city":"Paris"}"#}},
                    {"id": TOKYO, "type": "function", "function": {"name": "get_weather", "arguments": r#"{"city":"Tokyo"}"#}}
                ]
            }
        }],
        "usage": {"prompt_tokens": 107, "completion_tokens": 88, "total_tokens": 195}
    })
    .to_string()
    .into_bytes()
}

fn text_reply(model: &str, stream: bool) -> Vec<u8> {
    if stream {
        return sse(&[
            &format!(
                r#"data: {{"id":"h2","model":"{model}","choices":[{{"index":0,"delta":{{"role":"assistant","content":"Sunny in both."}}}}]}}"#
            ),
            &format!(
                r#"data: {{"id":"h2","model":"{model}","choices":[{{"index":0,"finish_reason":"stop","delta":{{}}}}],"usage":{{"prompt_tokens":275,"completion_tokens":4,"total_tokens":279}}}}"#
            ),
            "data: [DONE]",
        ]);
    }
    json!({
        "id": "h2",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "Sunny in both."}
        }],
        "usage": {"prompt_tokens": 275, "completion_tokens": 4, "total_tokens": 279}
    })
    .to_string()
    .into_bytes()
}

fn responses_ok(model: &str) -> Vec<u8> {
    json!({
        "id": "resp-d262",
        "object": "response",
        "status": "completed",
        "model": model,
        "output": [{
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "done", "annotations": []}],
            "reasoning_opaque": SEAL
        }]
    })
    .to_string()
    .into_bytes()
}

fn run(upstream: &CopilotUpstream, request: Request, inbound: ProtocolId) {
    let completion = block_on(upstream.execute(request, inbound)).expect("execute");
    drain(completion);
}

fn drain(completion: Completion) {
    block_on(drain_async(completion));
}

async fn drain_async(completion: Completion) {
    match completion.peel() {
        Completion::Unary(_) => {}
        Completion::Stream(mut stream) => {
            while let Some(event) = stream.next().await {
                event.expect("event");
            }
        }
        Completion::Raw(mut body) => {
            while let Some(chunk) = body.next_chunk().await {
                chunk.expect("chunk");
            }
        }
        Completion::Produced { .. } => unreachable!("peel removes the tag"),
        _ => {}
    }
}

fn chat_bodies(host: &TestHost) -> Vec<Vec<u8>> {
    host.http()
        .calls()
        .iter()
        .filter(|call| call.uri().contains("/chat/completions"))
        .map(|call| call.body().to_vec())
        .collect()
}

fn opaque_on(body: &[u8]) -> Option<String> {
    let value: json::Value = json::from_slice(body).expect("chat json");
    value
        .get("messages")?
        .as_array()?
        .iter()
        .find(|message| {
            message.get("role").and_then(json::Value::as_str) == Some("assistant")
                && message.get("tool_calls").is_some()
        })
        .and_then(|message| message.get("reasoning_opaque"))
        .and_then(json::Value::as_str)
        .map(str::to_owned)
}

fn stored(host: &TestHost, account: &str, model: &str, id: &str) -> Option<String> {
    reasoning::read(
        host.context(),
        &AccountId::try_from(account).expect("account id"),
        model,
        id,
    )
}

#[test]
fn d262_goes_back_on_the_tool_turn() {
    for stream in [true, false] {
        let fix = fix();
        let first = fix.account("d262-echo", "gho-d262");
        fix.prime();
        fix.host.http().enqueue(tool_reply(MODEL, stream));
        run(&first, opening(MODEL, stream), ProtocolId::OPENAI_CHAT);
        assert_eq!(
            stored(&fix.host, "d262-echo", MODEL, PARIS).as_deref(),
            Some(SEAL)
        );
        assert_eq!(
            stored(&fix.host, "d262-echo", MODEL, TOKYO).as_deref(),
            Some(SEAL)
        );

        let second = fix.upstream("d262-echo");
        fix.enqueue_session();
        fix.host.http().enqueue(text_reply(MODEL, stream));
        run(
            &second,
            follow_up(MODEL, stream, None),
            ProtocolId::OPENAI_CHAT,
        );
        let bodies = chat_bodies(&fix.host);
        assert_eq!(bodies.len(), 2, "stream={stream}");
        assert_eq!(
            opaque_on(&bodies[1]).as_deref(),
            Some(SEAL),
            "stream={stream}"
        );
        assert_eq!(
            String::from_utf8_lossy(&bodies[1]).matches(SEAL).count(),
            1,
            "stream={stream}"
        );
    }
}

#[test]
fn d262_only_its_own() {
    let fix = fix();
    let echo = fix.account("d262-echo", "gho-d262");
    fix.prime();
    fix.host.http().enqueue(tool_reply(MODEL, true));
    run(&echo, opening(MODEL, true), ProtocolId::OPENAI_CHAT);

    fix.host.http().enqueue(text_reply(MODEL, true));
    run(
        &echo,
        follow_up(MODEL, true, Some("the client's own")),
        ProtocolId::OPENAI_CHAT,
    );
    let bodies = chat_bodies(&fix.host);
    assert_eq!(
        opaque_on(bodies.last().expect("own")).as_deref(),
        Some("the client's own")
    );

    fix.host.http().enqueue(text_reply(OTHER_MODEL, true));
    run(
        &echo,
        follow_up(OTHER_MODEL, true, None),
        ProtocolId::OPENAI_CHAT,
    );
    let bodies = chat_bodies(&fix.host);
    let other_model = bodies.last().expect("other model");
    assert!(
        !String::from_utf8_lossy(other_model).contains("reasoning_opaque"),
        "another model was sent this one's opaque"
    );

    assert_eq!(
        stored(&fix.host, "d262-echo", MODEL, PARIS).as_deref(),
        Some(SEAL)
    );
    assert!(stored(&fix.host, "d262-other", MODEL, PARIS).is_none());

    let other = fix.account("d262-other", "gho-other");
    fix.prime();
    fix.host.http().enqueue(text_reply(MODEL, true));
    run(
        &other,
        follow_up(MODEL, true, None),
        ProtocolId::OPENAI_CHAT,
    );
    let bodies = chat_bodies(&fix.host);
    let other_account = bodies.last().expect("other account");
    assert!(
        !String::from_utf8_lossy(other_account).contains("reasoning_opaque"),
        "another account was sent this one's opaque"
    );

    fix.host.http().enqueue(text_reply(MODEL, true));
    run(&echo, other_call(MODEL), ProtocolId::OPENAI_CHAT);
    let bodies = chat_bodies(&fix.host);
    let unknown = bodies.last().expect("unknown call");
    assert!(
        !String::from_utf8_lossy(unknown).contains("reasoning_opaque"),
        "a call with nothing kept got an opaque"
    );
}

#[test]
fn d262_back_through_translation() {
    for inbound in [ProtocolId::ANTHROPIC_MESSAGES, ProtocolId::OPENAI_RESPONSES] {
        let stream = inbound == ProtocolId::ANTHROPIC_MESSAGES;
        let fix = fix();
        let upstream = fix.account("d262-echo", "gho-d262");
        fix.prime();
        fix.host.http().enqueue(tool_reply(MODEL, stream));
        run(&upstream, opening(MODEL, stream), inbound);
        assert_eq!(
            stored(&fix.host, "d262-echo", MODEL, TOKYO).as_deref(),
            Some(SEAL)
        );

        fix.host.http().enqueue(text_reply(MODEL, stream));
        run(&upstream, follow_up(MODEL, stream, None), inbound);
        let bodies = chat_bodies(&fix.host);
        assert_eq!(
            opaque_on(bodies.last().expect("translated")).as_deref(),
            Some(SEAL),
            "{inbound:?}"
        );
    }
}

#[test]
fn d262_responses_upstream_does_not_keep_chat_opaque() {
    let fix = fix();
    let upstream = fix.account("d262-echo", "gho-d262");
    fix.book.see(
        &AccountId::from_static("d262-echo"),
        MODEL,
        vec![ProtocolId::OPENAI_RESPONSES],
        Some(false),
    );
    fix.enqueue_session();
    fix.host.http().enqueue(responses_ok(MODEL));
    run(
        &upstream,
        opening(MODEL, false),
        ProtocolId::OPENAI_RESPONSES,
    );
    assert!(stored(&fix.host, "d262-echo", MODEL, PARIS).is_none());
    assert!(chat_bodies(&fix.host).is_empty());
}

fn loopback_response(status: u16, body: Vec<u8>, stream: bool) -> Http1Response {
    let content_type = if stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let response = if stream {
        let split = body.len() / 3;
        Http1Response::chunked(vec![body[..split].to_vec(), body[split..].to_vec()])
    } else {
        Http1Response::new(status).body(body)
    };
    response.header("content-type", content_type)
}

async fn run_loopback(
    host: &TestHost,
    book: &Arc<Book>,
    account: &AccountId,
    base: &str,
    request: Request,
) {
    let (rewrite, observe) =
        reasoning::hooks(book, host.context(), account, &request.model).expect("reasoning hooks");
    let target = Target::new(ProtocolId::OPENAI_CHAT, base)
        .rewrite(rewrite)
        .observe(observe);
    let completion = wire::execute(
        host.context().http(),
        &target,
        None,
        request,
        ProtocolId::OPENAI_CHAT,
    )
    .await
    .expect("loopback execute");
    drain_async(completion).await;
}

#[test]
fn d262_loopback_stream_and_whole_survive_fresh_state_and_ignore_201() {
    block_on(async {
        for stream in [true, false] {
            let dir = tempfile::tempdir().expect("dir");
            let host = TestHost::open_for_with_pooled_http_and_tokio_monotonic(
                PluginKind::Subscription,
                PluginId::from_static("copilot"),
                dir.path(),
            );
            let account = AccountId::from_static("d262-loopback");
            let server = Http1Server::start(vec![
                loopback_response(200, tool_reply(MODEL, stream), stream),
                loopback_response(200, text_reply(MODEL, stream), stream),
                loopback_response(201, tool_reply(OTHER_MODEL, false), false),
            ])
            .await;

            run_loopback(
                &host,
                &Arc::new(Book::default()),
                &account,
                server.base_url(),
                opening(MODEL, stream),
            )
            .await;
            assert_eq!(
                stored(&host, account.as_str(), MODEL, PARIS).as_deref(),
                Some(SEAL)
            );

            run_loopback(
                &host,
                &Arc::new(Book::default()),
                &account,
                server.base_url(),
                follow_up(MODEL, stream, None),
            )
            .await;
            let requests = server.requests();
            assert_eq!(requests.len(), 2, "stream={stream}");
            assert_eq!(
                opaque_on(requests[1].body()).as_deref(),
                Some(SEAL),
                "stream={stream}"
            );

            run_loopback(
                &host,
                &Arc::new(Book::default()),
                &account,
                server.base_url(),
                opening(OTHER_MODEL, false),
            )
            .await;
            assert!(stored(&host, account.as_str(), OTHER_MODEL, PARIS).is_none());
            server.shutdown().await;
        }
    });
}
