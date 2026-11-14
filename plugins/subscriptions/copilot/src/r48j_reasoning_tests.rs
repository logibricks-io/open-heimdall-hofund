//! Recorded execute fixtures for Copilot reasoning effort and reply decoding.

use std::sync::Arc;

use hofund_ir::Effort;
use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, ContentPart, PluginId, PluginKind, ProbeQuery, ProtocolId, Request,
    UpstreamProvider,
};
use hofund_plugin_sdk::json::{self, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache};

use crate::models::{self, Book};
use crate::session::MARGIN;
use crate::upstream::CopilotUpstream;

const API: &str = "https://api.individual.githubcopilot.com";

struct Fixture {
    _dir: tempfile::TempDir,
    host: TestHost,
    id: AccountId,
}

fn fixture(label: &str) -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix(&format!("hofund-r48j-copilot-reasoning-{label}-"))
        .tempdir()
        .expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("copilot"),
        dir.path(),
    );
    let id = AccountId::from_static("a-00000000000004a0");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &"gho-reasoning".to_owned().into())
        .with_text("host", "github.com")
        .with_text("login", "octo")
        .to_secret();
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "octo")
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, blob)
            .await
            .expect("seed");
    });
    Fixture {
        _dir: dir,
        host,
        id,
    }
}

impl Fixture {
    fn upstream(&self) -> CopilotUpstream {
        CopilotUpstream::new(
            self.id.clone(),
            self.host.context().clone(),
            Arc::new(TokenCache::new(MARGIN)),
            Arc::new(Book::default()),
        )
    }

    fn seed_session_and_models(&self) {
        self.host.http().enqueue(session_reply());
        self.host.http().enqueue(reasoning_models_reply());
    }
}

fn session_reply() -> Vec<u8> {
    json!({"token":"tid=session","expires_at":3600,"endpoints":{"api":API}})
        .to_string()
        .into_bytes()
}

fn reasoning_models_reply() -> Vec<u8> {
    json!({"data":[
        {"id":"gpt-5","model_picker_enabled":true,"capabilities":{"type":"chat","supports":{"reasoning_effort":["low","high"]}},"policy":{"state":"enabled"},"supported_endpoints":["/responses","/chat/completions"]},
        {"id":"o3","model_picker_enabled":true,"capabilities":{"type":"chat","supports":{"reasoning_effort":["low","high"]}},"policy":{"state":"enabled"},"supported_endpoints":["/responses"]},
        {"id":"o4-mini","model_picker_enabled":true,"capabilities":{"type":"chat","supports":{"reasoning_effort":["medium"]}},"policy":{"state":"enabled"},"supported_endpoints":["/chat/completions"]},
        {"id":"gpt-4.1","model_picker_enabled":true,"capabilities":{"type":"chat"},"policy":{"state":"enabled"},"supported_endpoints":["/chat/completions"]}
    ]})
    .to_string()
    .into_bytes()
}

fn unary_request(model: &str, protocol: ProtocolId, effort: Option<Effort>) -> Request {
    let mut request = ProbeQuery::new(protocol, model)
        .expect("query")
        .minimal_request();
    request.stream = false;
    if let Some(effort) = effort {
        request.params.effort = Some(effort);
    }
    request
}

fn outbound_body(index: usize, host: &TestHost) -> json::Value {
    let calls = host.http().calls();
    json::from_slice(calls[index].body()).expect("JSON body")
}

fn thinking_text(completion: Completion) -> Option<String> {
    let Completion::Unary(response) = completion else {
        panic!("expected unary completion");
    };
    response.message.content.iter().find_map(|part| match part {
        ContentPart::Thinking(thinking) => Some(thinking.text.clone()),
        _ => None,
    })
}

#[test]
fn r48j_chat_effort_reaches_recorded_upstream_body() {
    let fx = fixture("chat-effort");
    fx.seed_session_and_models();
    fx.host.http().enqueue(chat_reply_with_reasoning("gpt-5"));
    block_on(fx.upstream().execute(
        unary_request("gpt-5", ProtocolId::OPENAI_CHAT, Some(Effort::High)),
        ProtocolId::OPENAI_CHAT,
    ))
    .expect("chat execute");
    let calls = fx.host.http().calls();
    assert_eq!(calls[2].uri(), format!("{API}/chat/completions"));
    let body = outbound_body(2, &fx.host);
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("reasoning").is_none());
}

#[test]
fn r48j_responses_effort_reaches_recorded_upstream_body() {
    let fx = fixture("responses-effort");
    fx.seed_session_and_models();
    fx.host
        .http()
        .enqueue(responses_reply_with_reasoning("gpt-5"));
    block_on(fx.upstream().execute(
        unary_request("gpt-5", ProtocolId::OPENAI_RESPONSES, Some(Effort::Low)),
        ProtocolId::OPENAI_RESPONSES,
    ))
    .expect("responses execute");
    let calls = fx.host.http().calls();
    assert_eq!(calls[2].uri(), format!("{API}/responses"));
    let body = outbound_body(2, &fx.host);
    assert_eq!(body["reasoning"]["effort"], "low");
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn r48j_chat_reasoning_content_surfaces_in_completion() {
    let fx = fixture("chat-reply");
    fx.seed_session_and_models();
    fx.host.http().enqueue(chat_reply_with_reasoning("o4-mini"));
    let completion = block_on(fx.upstream().execute(
        unary_request("o4-mini", ProtocolId::OPENAI_RESPONSES, None),
        ProtocolId::OPENAI_RESPONSES,
    ))
    .expect("chat completion");
    assert_eq!(
        thinking_text(completion).as_deref(),
        Some("trace through the problem")
    );
}

#[test]
fn r48j_responses_reasoning_item_surfaces_in_completion() {
    let fx = fixture("responses-reply");
    fx.seed_session_and_models();
    fx.host.http().enqueue(responses_reply_with_reasoning("o3"));
    let completion = block_on(fx.upstream().execute(
        unary_request("o3", ProtocolId::OPENAI_CHAT, None),
        ProtocolId::OPENAI_CHAT,
    ))
    .expect("responses completion");
    assert_eq!(
        thinking_text(completion).as_deref(),
        Some("plan step by step")
    );
}

#[test]
fn r48j_plain_model_omits_effort_without_request() {
    let fx = fixture("plain-model");
    fx.seed_session_and_models();
    fx.host.http().enqueue(chat_plain_reply());
    block_on(fx.upstream().execute(
        unary_request("gpt-4.1", ProtocolId::OPENAI_CHAT, None),
        ProtocolId::OPENAI_CHAT,
    ))
    .expect("plain chat");
    let body = outbound_body(2, &fx.host);
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("reasoning").is_none());
    let (models, _, _) = models::parse(&reasoning_models_reply()).expect("catalog");
    let plain = models
        .iter()
        .find(|model| model.id() == "gpt-4.1")
        .expect("gpt-4.1");
    assert!(plain.efforts().is_empty());
}

fn chat_reply_with_reasoning(model: &str) -> Vec<u8> {
    json!({
        "id":"chat-r48j",
        "object":"chat.completion",
        "created":0,
        "model":model,
        "choices":[{
            "index":0,
            "message":{
                "role":"assistant",
                "content":"answer",
                "reasoning_content":"trace through the problem"
            },
            "finish_reason":"stop"
        }]
    })
    .to_string()
    .into_bytes()
}

fn chat_plain_reply() -> Vec<u8> {
    json!({
        "id":"chat-plain",
        "object":"chat.completion",
        "created":0,
        "model":"gpt-4.1",
        "choices":[{
            "index":0,
            "message":{"role":"assistant","content":"ok"},
            "finish_reason":"stop"
        }]
    })
    .to_string()
    .into_bytes()
}

fn responses_reply_with_reasoning(model: &str) -> Vec<u8> {
    json!({
        "id":"resp-r48j",
        "object":"response",
        "status":"completed",
        "model":model,
        "output":[
            {
                "id":"rs_1",
                "type":"reasoning",
                "content":[{"type":"reasoning_text","text":"plan step by step"}]
            },
            {
                "id":"msg_1",
                "type":"message",
                "role":"assistant",
                "status":"completed",
                "content":[{"type":"output_text","text":"done","annotations":[]}]
            }
        ]
    })
    .to_string()
    .into_bytes()
}
