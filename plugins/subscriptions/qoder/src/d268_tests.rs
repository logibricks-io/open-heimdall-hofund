//! Claude Code's identity line on a Qoder system prompt.

use std::time::SystemTime;

use hofund_kit_subscription::{
    Http1Response, Http1Server, RecordedHttp1Request, TestHost, block_on,
};
use hofund_plugin_api::{
    AccountId, Completion, ContentPart, Message, PluginId, PluginKind, ProtocolId, Request, Role,
    SecretString, SubscriptionFactory,
};
use hofund_plugin_sdk::json::{self, Value};

use crate::creds::{self, Credential, Site};
use crate::shape::Tasks;
use crate::{Endpoints, cosy, factory_with_endpoints};

const CLAUDE_CODE_LINE: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const AGENT_SDK_LINE: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const QODER_LINE: &str = "You are a Qoder agent. Use the instructions below and the tools available to you to assist the user.";
const BILLING: &str = "x-anthropic-billing-header: cc_version=2.1.80; cc_entrypoint=cli;";
const REST: &str =
    "\nYou are an interactive agent that helps users with software engineering tasks.";

fn chat_body(bytes: &[u8]) -> Value {
    let plain = std::str::from_utf8(bytes).unwrap_or("");
    let decoded = cosy::decode(plain).unwrap_or_else(|_| plain.to_owned());
    json::from_str(&decoded).unwrap_or(Value::Null)
}

fn answer(request: &RecordedHttp1Request) -> Http1Response {
    if request.path().contains("model/list") {
        return Http1Response::new(200)
            .header("content-type", "application/json")
            .body(include_bytes!("fixtures/models.json").to_vec());
    }
    if request.path().contains("agent_chat_generation") {
        let body = chat_body(request.body());
        if carries_claude_code_identity(&system_text(&body)) {
            return Http1Response::new(400)
                .header("content-type", "application/json")
                .body(br#"{"message":"Cannot find model"}"#.to_vec());
        }
        return Http1Response::new(200)
            .header("content-type", "text/event-stream")
            .body(chat_success_sse());
    }
    Http1Response::new(404).body(b"unexpected".to_vec())
}

fn chat_success_sse() -> Vec<u8> {
    let chunk = json::json!({
        "choices": [{"index": 0, "delta": {"content": "hello"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1}
    });
    format!(
        "data: {}\n\ndata: {{\"statusCodeValue\":200,\"body\":\"[DONE]\"}}\n\n",
        json::json!({"statusCodeValue": 200, "body": chunk.to_string()})
    )
    .into_bytes()
}

fn carries_claude_code_identity(text: &str) -> bool {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.contains("You are Claude Code, Anthropic's official CLI for Claude.")
        || flat.contains(
            "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.",
        )
}

fn system_text(body: &Value) -> String {
    body["system"][0]["text"].as_str().unwrap_or("").to_owned()
}

fn endpoints(base: &str) -> Endpoints {
    Endpoints {
        web_global: "https://qoder.test".to_owned(),
        web_cn: "https://qoder-cn.test".to_owned(),
        openapi_global: "https://openapi.qoder.test".to_owned(),
        openapi_cn: "https://openapi-cn.qoder.test".to_owned(),
        chat_global: base.to_owned(),
        chat_cn: "https://chat-cn.qoder.test".to_owned(),
    }
}

fn credential() -> Credential {
    Credential {
        uid: "uid-1".to_owned(),
        email: "me@example.test".to_owned(),
        name: "Example".to_owned(),
        token: SecretString::from("job-token".to_owned()),
        refresh_token: SecretString::from("job-refresh".to_owned()),
        expires_at: 4_000_000_000_000,
        machine_id: "machine-id".to_owned(),
        site: Site::Global,
        vpc: String::new(),
        device_chat: false,
        device_token: SecretString::from("device-token".to_owned()),
        device_refresh_token: SecretString::from("device-refresh".to_owned()),
        pat_auth: false,
        pat: SecretString::from(String::new()),
    }
}

async fn seed(host: &TestHost) -> AccountId {
    let credential = credential();
    let id = creds::account_id(&credential);
    let account = host
        .context()
        .accounts()
        .account(id.clone(), creds::label(&credential))
        .expect("account");
    host.context()
        .accounts()
        .upsert(account, creds::secret_blob(&credential).to_secret())
        .await
        .expect("seed");
    id
}

fn claude_code_request() -> Request {
    let mut system = Message::new(Role::System);
    system.content.push(ContentPart::text(BILLING));
    system.content.push(ContentPart::text(CLAUDE_CODE_LINE));
    system.content.push(ContentPart::text(REST));
    let mut user = Message::new(Role::User);
    user.content
        .push(ContentPart::text(format!("{CLAUDE_CODE_LINE} hi")));
    Request::new("qfmodel").with_messages(vec![system, user])
}

fn shaped_system(messages: Value) -> (String, Value) {
    let models = crate::models::parse_list(include_bytes!("fixtures/models.json")).expect("models");
    let model = models
        .iter()
        .find(|model| model.key == "qfmodel")
        .expect("model");
    let body = Tasks::default()
        .body(
            &json::json!({"messages": messages}),
            model,
            SystemTime::UNIX_EPOCH,
        )
        .expect("body");
    (system_text(&body.value), body.value)
}

#[test]
fn d268_loopback_refuses_claude_code_identity_line() {
    block_on(async {
        let server = Http1Server::start_with(answer).await;
        let dir = tempfile::tempdir().expect("dir");
        let host = TestHost::open_for_with_pooled_http_and_tokio_monotonic(
            PluginKind::Subscription,
            PluginId::from_static("qoder"),
            dir.path(),
        );
        let id = seed(&host).await;
        let factory = factory_with_endpoints(endpoints(server.base_url()));
        let config = factory
            .validate(&hofund_plugin_api::RawTable::new())
            .expect("config");
        let subscription = factory
            .build(&config, host.context())
            .expect("subscription");
        let upstream = subscription
            .upstream(&id, host.context())
            .expect("upstream");
        let outcome = upstream
            .execute(claude_code_request(), ProtocolId::OPENAI_CHAT)
            .await;
        let chats: Vec<Value> = server
            .requests()
            .iter()
            .filter(|request| request.path().contains("agent_chat_generation"))
            .map(|request| chat_body(request.body()))
            .collect();
        let system = chats.first().map(system_text).unwrap_or_default();
        let error = outcome.as_ref().err().map(|error| error.to_string());
        assert!(
            !carries_claude_code_identity(&system),
            "forwarded system text still names Claude Code: {system:?}; upstream error: {error:?}"
        );
        assert!(
            outcome.is_ok(),
            "loopback answered 400 Cannot find model for system text {system:?}: {error:?}"
        );
        let completion = outcome.expect("completion");
        assert!(matches!(completion.peel(), Completion::Unary(_)));
        assert!(system.starts_with(QODER_LINE), "{system}");
        assert!(system.contains(BILLING), "{system}");
        assert!(
            system.contains(&format!("{AGENT_SDK_LINE}{REST}")),
            "{system}"
        );
        assert!(!system.contains("You are Claude Code"), "{system}");
        let user = chats[0]["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["role"] == "user")
            .expect("user");
        let user_text = user.to_string();
        assert!(
            user_text.contains(&format!("{CLAUDE_CODE_LINE} hi")),
            "{user_text}"
        );
        assert_eq!(
            chats[0]["messages"][0]["content"][0]["text"].as_str(),
            Some(system.as_str())
        );
        server.shutdown().await;
    });
}

#[test]
fn d268_system_text_swaps_only_the_identity_line() {
    let sdk_form = CLAUDE_CODE_LINE.replacen('.', ", running within the Claude Agent SDK.", 1);
    let (system, _) = shaped_system(json::json!([
        {"role": "system", "content": [
            {"type": "text", "text": BILLING},
            {"type": "text", "text": CLAUDE_CODE_LINE},
            {"type": "text", "text": REST}
        ]},
        {"role": "user", "content": format!("{CLAUDE_CODE_LINE} hi")}
    ]));
    assert_eq!(
        system,
        format!("{QODER_LINE}\n\n{BILLING}{AGENT_SDK_LINE}{REST}")
    );
    assert!(!system.contains("You are Claude Code"));

    let (spaced, _) = shaped_system(json::json!([
        {"role": "system", "content": "You are Claude Code,\nAnthropic's  official CLI for Claude.\n\nBe brief."}
    ]));
    assert_eq!(
        spaced,
        format!("{QODER_LINE}\n\n{AGENT_SDK_LINE}\n\nBe brief.")
    );

    let (formed, _) = shaped_system(json::json!([
        {"role": "developer", "content": sdk_form}
    ]));
    assert_eq!(formed, format!("{QODER_LINE}\n\n{AGENT_SDK_LINE}"));

    let doubled = format!("{CLAUDE_CODE_LINE}\n\nnotes\n\n{CLAUDE_CODE_LINE}");
    let (both, value) = shaped_system(json::json!([
        {"role": "system", "content": doubled},
        {"role": "user", "content": CLAUDE_CODE_LINE}
    ]));
    assert_eq!(
        both,
        format!("{QODER_LINE}\n\n{AGENT_SDK_LINE}\n\nnotes\n\n{AGENT_SDK_LINE}")
    );
    assert!(value["messages"].to_string().contains(CLAUDE_CODE_LINE));

    for left in [
        AGENT_SDK_LINE,
        "You are a coding agent.",
        "Claude Code is Anthropic's official CLI for Claude.",
    ] {
        let (system, _) = shaped_system(json::json!([{"role": "system", "content": left}]));
        assert_eq!(system, format!("{QODER_LINE}\n\n{left}"));
    }
}
