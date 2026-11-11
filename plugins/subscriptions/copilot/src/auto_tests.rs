//! Offline Copilot Auto negotiation, execution, and refusal fixtures.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, ModelList, PluginError, PluginErrorKind, PluginId, PluginKind, ProbeQuery,
    ProtocolId, RawTable, Request, Subscription, SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::json::{self, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache};

use crate::CopilotFactory;
use crate::auto;
use crate::models::{self, Book};
use crate::session::{MARGIN, Session};
use crate::upstream::CopilotUpstream;

const API: &str = "https://api.individual.githubcopilot.com";

struct Fixture {
    _dir: tempfile::TempDir,
    host: TestHost,
    id: AccountId,
    book: Arc<Book>,
}

fn fixture(label: &str) -> Fixture {
    let dir = tempfile::Builder::new()
        .prefix(&format!("hofund-r48-copilot-auto-{label}-"))
        .tempdir()
        .expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("copilot"),
        dir.path(),
    );
    let id = AccountId::from_static("a-0000000000000256");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &"gho-auto".to_owned().into())
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
        book: Arc::new(Book::default()),
    }
}

impl Fixture {
    fn upstream(&self) -> CopilotUpstream {
        CopilotUpstream::new(
            self.id.clone(),
            self.host.context().clone(),
            Arc::new(TokenCache::new(MARGIN)),
            Arc::clone(&self.book),
        )
    }

    fn subscription(&self) -> Arc<dyn Subscription> {
        let cfg = CopilotFactory.validate(&RawTable::new()).expect("config");
        CopilotFactory
            .build(&cfg, self.host.context())
            .expect("build")
    }

    fn seed_session(&self) {
        self.host.http().enqueue(session_reply());
    }
}

fn session_reply() -> Vec<u8> {
    json!({"token":"tid=session","expires_at":3600,"endpoints":{"api":API}})
        .to_string()
        .into_bytes()
}

fn request(model: &str, protocol: ProtocolId) -> Request {
    let mut request = ProbeQuery::new(protocol, model)
        .expect("query")
        .minimal_request();
    request.stream = false;
    request
}

fn chat_reply(model: &str) -> Vec<u8> {
    json!({"id":"chat-1","object":"chat.completion","created":0,"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]})
        .to_string()
        .into_bytes()
}

fn responses_reply(model: &str) -> Vec<u8> {
    json!({"id":"resp-1","object":"response","status":"completed","model":model,"output":[]})
        .to_string()
        .into_bytes()
}

fn models_reply() -> Vec<u8> {
    json!({"data":[
        {"id":"claude-haiku-4.5","model_picker_enabled":true,"capabilities":{"type":"chat"},"policy":{"state":"enabled"},"billing":{"is_premium":true}},
        {"id":"gpt-5","model_picker_enabled":true,"capabilities":{"type":"chat"},"policy":{"state":"enabled"},"supported_endpoints":["/responses"],"billing":{"is_premium":true}},
        {"id":"gpt-4.1","model_picker_enabled":true,"capabilities":{"type":"chat"},"policy":{"state":"enabled"},"is_chat_fallback":true,"billing":{"is_premium":false}}
    ]})
    .to_string()
    .into_bytes()
}

fn uris(host: &TestHost) -> Vec<String> {
    host.http()
        .calls()
        .iter()
        .map(|call| call.uri().to_owned())
        .collect()
}

#[test]
fn auto_is_listed_even_when_no_manual_model_is_offered() {
    let fixture = fixture("listed");
    fixture.seed_session();
    fixture.host.http().enqueue(
        br#"{"data":[{"id":"gpt-5-mini","model_picker_enabled":true,"capabilities":{"type":"chat"},"policy":{"state":"disabled"}}]}"#
            .to_vec(),
    );
    let list: ModelList = block_on(
        fixture
            .subscription()
            .list_models(&fixture.id, fixture.host.context()),
    )
    .expect("models");
    assert_eq!(list.models().len(), 1);
    assert_eq!(list.models()[0].id(), "auto");
    assert_eq!(list.models()[0].display_name(), Some("Auto"));
}

#[test]
fn auto_v2_pick_is_reused_and_sent_with_its_session() {
    let fixture = fixture("v2");
    let upstream = fixture.upstream();
    fixture.seed_session();
    fixture.host.http().enqueue(models_reply());
    fixture.host.http().enqueue(
        json!({"session_token":"auto-1","selected_model":{"id":"gpt-5","supported_endpoints":["/responses"]},"expires_at":0})
            .to_string()
            .into_bytes(),
    );
    fixture.host.http().enqueue(responses_reply("gpt-5"));
    fixture.host.http().enqueue(responses_reply("gpt-5"));
    for _ in 0..2 {
        block_on(upstream.execute(
            request("auto", ProtocolId::OPENAI_RESPONSES),
            ProtocolId::OPENAI_RESPONSES,
        ))
        .expect("Auto call");
    }
    assert_eq!(
        uris(&fixture.host),
        vec![
            "https://api.github.com/copilot_internal/v2/token".to_owned(),
            format!("{API}/models"),
            format!("{API}/auto"),
            format!("{API}/responses"),
            format!("{API}/responses")
        ]
    );
    let calls = fixture.host.http().calls();
    let negotiation: json::Value = json::from_slice(calls[2].body()).expect("Auto body");
    assert_eq!(negotiation["tier"], "balance");
    assert!(!negotiation["prompt"].as_str().expect("prompt").is_empty());
    assert_eq!(calls[2].header("X-GitHub-Api-Version"), Some("2026-08-01"));
    let sent: json::Value = json::from_slice(calls[3].body()).expect("request body");
    assert_eq!(sent["model"], "gpt-5");
    assert!(
        calls[3]
            .sensitive_headers()
            .contains(&"x-github-api-version".to_owned())
    );
    assert!(
        calls[3]
            .sensitive_headers()
            .contains(&"copilot-session-token".to_owned())
    );
}

#[test]
fn legacy_session_uses_its_api_version_and_first_known_available_model() {
    let fixture = fixture("legacy");
    let upstream = fixture.upstream();
    fixture.seed_session();
    fixture.host.http().enqueue(models_reply());
    fixture
        .host
        .http()
        .enqueue_response(404, &[], b"not offered".to_vec());
    fixture.host.http().enqueue(
        json!({"session_token":"legacy-1","available_models":["unknown-model","gpt-5"],"expires_at":7200})
            .to_string()
            .into_bytes(),
    );
    fixture.host.http().enqueue(responses_reply("gpt-5"));
    block_on(upstream.execute(
        request("auto", ProtocolId::OPENAI_RESPONSES),
        ProtocolId::OPENAI_RESPONSES,
    ))
    .expect("legacy Auto");
    let calls = fixture.host.http().calls();
    assert_eq!(calls[2].uri(), format!("{API}/auto"));
    assert_eq!(calls[3].uri(), format!("{API}/models/session"));
    assert_eq!(calls[3].header("X-GitHub-Api-Version"), Some("2025-10-01"));
    assert_eq!(
        calls[3].body(),
        br#"{"auto_mode":{"model_hints":["auto"]}}"#
    );
    assert_eq!(calls[4].uri(), format!("{API}/responses"));
}

#[test]
fn auto_404_falls_back_to_copilots_base_model_without_session() {
    let fixture = fixture("fallback");
    let upstream = fixture.upstream();
    fixture.seed_session();
    fixture.host.http().enqueue(models_reply());
    fixture
        .host
        .http()
        .enqueue_response(404, &[], b"not offered".to_vec());
    fixture
        .host
        .http()
        .enqueue_response(404, &[], b"not offered".to_vec());
    fixture.host.http().enqueue(chat_reply("gpt-4.1"));
    fixture.host.http().enqueue(chat_reply("gpt-4.1"));
    for _ in 0..2 {
        block_on(upstream.execute(
            request("auto", ProtocolId::OPENAI_CHAT),
            ProtocolId::OPENAI_CHAT,
        ))
        .expect("fallback");
    }
    let calls = fixture.host.http().calls();
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[4].uri(), format!("{API}/chat/completions"));
    let sent: json::Value = json::from_slice(calls[4].body()).expect("body");
    assert_eq!(sent["model"], "gpt-4.1");
    assert!(
        !calls[4]
            .sensitive_headers()
            .contains(&"copilot-session-token".to_owned())
    );
    assert_eq!(calls[5].uri(), format!("{API}/chat/completions"));
}

fn refusal() -> Vec<u8> {
    br#"{"error":{"message":"The requested model is not supported.","code":"model_not_supported"}}"#
        .to_vec()
}

#[test]
fn refused_auto_models_are_skipped_now_and_later() {
    let fixture = fixture("refused");
    let upstream = fixture.upstream();
    fixture.seed_session();
    fixture.host.http().enqueue(models_reply());
    for attempt in 0..3 {
        fixture
            .host
            .http()
            .enqueue_response(404, &[], b"not offered".to_vec());
        fixture.host.http().enqueue(
            json!({"session_token":"legacy-1","selected_model":"claude-haiku-4.5","available_models":["claude-haiku-4.5","gpt-5","gpt-4.1"],"expires_at":7200})
                .to_string()
                .into_bytes(),
        );
        if attempt < 2 {
            fixture.host.http().enqueue_response(400, &[], refusal());
        } else {
            fixture.host.http().enqueue(chat_reply("gpt-4.1"));
        }
    }
    block_on(upstream.execute(
        request("auto", ProtocolId::OPENAI_CHAT),
        ProtocolId::OPENAI_CHAT,
    ))
    .unwrap_or_else(|error| panic!("{error:?}: {:?}", uris(&fixture.host)));
    assert!(auto::refused(
        &fixture.book,
        &fixture.id,
        "claude-haiku-4.5",
        fixture.host.context().clock().now()
    ));
    assert!(auto::refused(
        &fixture.book,
        &fixture.id,
        "gpt-5",
        fixture.host.context().clock().now()
    ));
    let calls = fixture.host.http().calls();
    let concrete_calls: Vec<_> = calls
        .iter()
        .filter(|call| {
            call.uri().ends_with("/chat/completions") || call.uri().ends_with("/responses")
        })
        .collect();
    assert_eq!(concrete_calls.len(), 3);
    let models: Vec<String> = concrete_calls
        .iter()
        .map(|call| {
            let body: json::Value = json::from_slice(call.body()).expect("body");
            body["model"].as_str().expect("model").to_owned()
        })
        .collect();
    assert_eq!(models, ["claude-haiku-4.5", "gpt-5", "gpt-4.1"]);

    fixture.host.http().enqueue(models_reply());
    let listed = block_on(models::list(
        fixture.host.context(),
        &Session {
            token: "tid=session".to_owned().into(),
            base: API.to_owned(),
            direct: false,
        },
        &fixture.id,
        &fixture.book,
    ))
    .expect("list after refusals");
    let ids: Vec<_> = listed.models().iter().map(|model| model.id()).collect();
    assert_eq!(ids, ["gpt-4.1", "auto"]);
}

#[test]
fn manual_refusal_is_not_retried_and_wrong_endpoint_is_not_a_refusal() {
    let fixture = fixture("manual-refusal");
    let upstream = fixture.upstream();
    fixture.seed_session();
    fixture.host.http().enqueue(models_reply());
    fixture.host.http().enqueue_response(400, &[], refusal());
    let error = block_on(upstream.execute(
        request("claude-haiku-4.5", ProtocolId::OPENAI_CHAT),
        ProtocolId::OPENAI_CHAT,
    ))
    .err()
    .expect("manual refusal");
    assert_eq!(error.status(), Some(400));
    assert_eq!(fixture.host.http().calls().len(), 3);
    assert!(auto::refused(
        &fixture.book,
        &fixture.id,
        "claude-haiku-4.5",
        fixture.host.context().clock().now()
    ));

    let error = PluginError::status_error(
        400,
        r#"{"error":{"code":"unsupported_api_for_model","message":"not accessible via /chat/completions"}}"#,
    );
    assert!(!auto::model_refusal(&error));
}

#[test]
fn lost_auto_session_is_replaced_without_marking_model_refused() {
    let fixture = fixture("session-gone");
    let upstream = fixture.upstream();
    fixture.seed_session();
    fixture.host.http().enqueue(models_reply());
    fixture.host.http().enqueue(
        json!({"session_token":"auto-1","selected_model":{"id":"gpt-5","supported_endpoints":["/responses"]},"expires_at":7200})
            .to_string()
            .into_bytes(),
    );
    fixture.host.http().enqueue_response(
        400,
        &[],
        br#"{"error":{"message":"Requested model not available for session"}}"#.to_vec(),
    );
    fixture.host.http().enqueue(
        json!({"session_token":"auto-2","selected_model":{"id":"gpt-5","supported_endpoints":["/responses"]},"expires_at":7200})
            .to_string()
            .into_bytes(),
    );
    fixture.host.http().enqueue(responses_reply("gpt-5"));
    block_on(upstream.execute(
        request("auto", ProtocolId::OPENAI_RESPONSES),
        ProtocolId::OPENAI_RESPONSES,
    ))
    .expect("new Auto session");
    assert!(!auto::refused(
        &fixture.book,
        &fixture.id,
        "gpt-5",
        fixture.host.context().clock().now()
    ));
    assert_eq!(
        uris(&fixture.host)
            .iter()
            .filter(|uri| uri.ends_with("/auto"))
            .count(),
        2,
        "{:?}",
        uris(&fixture.host)
    );
}

#[test]
fn no_auto_and_no_manual_model_is_an_auth_error() {
    let fixture = fixture("unavailable");
    let upstream = fixture.upstream();
    fixture.seed_session();
    fixture.host.http().enqueue(br#"{"data":[]}"#.to_vec());
    for _ in 0..2 {
        fixture
            .host
            .http()
            .enqueue_response(404, &[], b"not offered".to_vec());
    }
    let error = block_on(upstream.execute(
        request("auto", ProtocolId::OPENAI_CHAT),
        ProtocolId::OPENAI_CHAT,
    ))
    .err()
    .expect("unavailable");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(error.message().contains("does not offer Auto"));
}
