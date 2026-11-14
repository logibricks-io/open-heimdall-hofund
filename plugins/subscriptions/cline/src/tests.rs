//! Deterministic tests with a recording HTTP backend.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, Request,
    SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_sdk::oauth::secret_text;

use crate::models::{self, parse_feed};
use crate::quota::{self, limit_windows};
use crate::tokens::{self, AuthKind};
use crate::{Endpoints, factory_with_endpoints};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const API: &str = "https://cline.test/api/v1";

fn endpoints() -> Endpoints {
    Endpoints {
        api: API.to_owned(),
        workos_device: "https://workos.test/authorize/device".to_owned(),
        workos_token: "https://workos.test/authenticate".to_owned(),
    }
}

fn plugin() -> PluginId {
    PluginId::from_static("cline")
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn device_auth_body() -> Vec<u8> {
    json!({
        "device_code": "dev-secret-code",
        "user_code": "WDJB-MJHT",
        "verification_uri": "https://app.cline.bot/device",
        "verification_uri_complete": "https://app.cline.bot/device?code=WDJB-MJHT",
        "expires_in": 600,
        "interval": 1
    })
    .to_string()
    .into_bytes()
}

fn register_body() -> Vec<u8> {
    json!({
        "success": true,
        "data": {
            "accessToken": "cline-access-1",
            "refreshToken": "cline-refresh-1",
            "expiresAt": "2030-01-01T00:00:00Z",
            "userInfo": {"email": "user@cline.test", "clineUserId": "uid-1"}
        }
    })
    .to_string()
    .into_bytes()
}

fn seed_oauth(host: &TestHost) -> AccountId {
    let id = AccountId::from_static("a-0000000000000001");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_text("auth_type", AuthKind::Oauth.as_str())
        .with_secret("access_token", &secret("cline-access-1"))
        .with_secret("refresh_token", &secret("cline-refresh-1"))
        .with_number("expires_ms", 4_000_000_000_000)
        .with_text("email", "user@cline.test");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "user@cline.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2a_17_device_sign_in_polls_workos_then_registers() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(device_auth_body());
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::DeviceCode { user_code, .. } = challenge else {
        panic!("device challenge");
    };
    assert_eq!(user_code, "WDJB-MJHT");

    host.http().enqueue(
        br#"{"access_token":"wo-access","refresh_token":"wo-refresh","token_type":"Bearer"}"#
            .to_vec(),
    );
    host.http().enqueue(register_body());
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect("complete");
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert_eq!(parsed.text("auth_type"), Some("oauth"));
    assert!(parsed.secret("refresh_token").is_some());
}

#[test]
fn a2a_18_refresh_rotates_tokens() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({
            "success": true,
            "data": {
                "accessToken": "cline-access-2",
                "refreshToken": "cline-refresh-2",
                "expiresAt": "2031-01-01T00:00:00Z"
            }
        })
        .to_string()
        .into_bytes(),
    );
    block_on(tokens::refresh_for_test(
        host.context().clone(),
        endpoints(),
        id.clone(),
        secret("cline-access-1"),
    ))
    .expect("refresh");
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert!(
        parsed
            .secret("access_token")
            .is_some_and(|t| secret_text(&t) == "cline-access-2")
    );
    assert!(
        parsed
            .secret("refresh_token")
            .is_some_and(|t| secret_text(&t) == "cline-refresh-2")
    );
    assert!(parsed.number("expires_ms").unwrap_or(0) > 0);
}

#[test]
fn a2a_19_model_feed_parse_and_list() {
    let feed = json!({
        "recommended": [{"id": "anthropic/claude-sonnet-5.5"}],
        "free": [{"id": "cline-free/deepseek-v4.1-flash"}],
        "clinePass": [{"id": "cline-pass/deepseek-v4.1-flash"}],
    });
    let models = parse_feed(&feed);
    assert_eq!(models.len(), 3);
    assert!(models[1].free);
    assert!(models[2].pass);

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({"success": true, "data": feed})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({"success": true, "data": [
            {
                "id": "deepseek/deepseek-v4.1-flash",
                "architecture": {"input_modalities": ["text", "image"]},
                "context_length": 262144
            },
            {"id": "local-model", "display_name": "Local Model"}
        ]})
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    assert_eq!(list.models().len(), 5);
    assert_eq!(list.models()[4].id(), "local-model");
    let pass = list
        .models()
        .iter()
        .find(|model| model.id() == "cline-pass/deepseek-v4.1-flash")
        .expect("pass model");
    assert_eq!(pass.context(), 0);
    assert_eq!(pass.image_input(), Some(true));
    assert_eq!(pass.input_modalities(), ["text", "image"]);
    assert!(pass.efforts().is_empty());
    for call in host.http().calls() {
        assert_eq!(call.header("x-client-type"), Some("cline-cli"));
        assert_eq!(call.header("x-client-version"), Some("3.0.68"));
        assert_eq!(call.header("x-platform"), Some("cli"));
        assert_eq!(call.header("x-platform-version"), Some("3.0.68"));
        assert_eq!(call.header("x-core-version"), Some("0.0.90"));
        assert_eq!(call.header("user-agent"), Some("Cline/3.0.68"));
    }
}

#[test]
fn r53_a_failed_cloud_catalog_keeps_the_required_feed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({"success": true, "data": {
            "recommended": [{"id": "anthropic/claude-sonnet-5.5"}]
        }})
        .to_string()
        .into_bytes(),
    );
    host.http()
        .enqueue_response(503, &[], br#"{"error":"unavailable"}"#.to_vec());
    let sub = subscription(&host);
    let list = block_on(sub.list_models(&id, host.context())).expect("recommended models");
    assert_eq!(list.models().len(), 1);
    assert_eq!(list.models()[0].id(), "anthropic/claude-sonnet-5.5");
    assert_eq!(list.models()[0].image_input(), None);
}

#[test]
fn r53_chat_encodes_inline_images_as_openai_image_parts() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({
            "success": true,
            "data": {
                "id": "chat-1",
                "object": "chat.completion",
                "choices": [{"message": {"role": "assistant", "content": "ok"}}]
            }
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let mut user = hofund_plugin_api::Message::new(hofund_plugin_api::Role::User);
    user.content = vec![
        hofund_plugin_api::ContentPart::text("see this"),
        hofund_plugin_api::ContentPart::Image(hofund_plugin_api::ImagePart {
            source: hofund_plugin_api::ImageSource::Base64 {
                media_type: "image/png".to_owned(),
                data: "AA==".to_owned(),
            },
            detail: None,
            extensions: Default::default(),
        }),
    ];
    let request = Request::new("cline-pass/deepseek-v4.1-flash").with_messages(vec![user]);
    block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("chat");
    let calls = host.http().calls();
    let chat = calls
        .iter()
        .find(|call| call.uri().contains("chat/completions"))
        .expect("chat call");
    let sent: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(chat.body()).expect("request json");
    assert_eq!(
        sent["messages"][0]["content"][1],
        json!({
            "type": "image_url",
            "image_url": {"url": "data:image/png;base64,AA=="}
        })
    );
}

#[test]
fn a2a_20_usage_parse_limits_and_balance() {
    let limits = json!({"limits": [{"type": "five_hour", "percentUsed": 25.0}]});
    let windows = limit_windows(Some(&limits));
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].used(), 25);

    let balance = json!({"balance": 1_500_000});
    let usage = quota::assemble(None, Some(&balance), Some(&limits));
    assert_eq!(usage.windows.len(), 1);
    assert_eq!(usage.windows[0].name(), "5 hours");
    assert_eq!(usage.balance.as_deref(), Some("$1.50"));
}

#[test]
fn a2a_21_chat_upstream_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({
            "success": true,
            "data": {
                "id": "chat-1",
                "object": "chat.completion",
                "choices": [{"message": {"role": "assistant", "content": "ok"}}]
            }
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let query =
        ProbeQuery::new(ProtocolId::OPENAI_CHAT, "anthropic/claude-sonnet-5.5").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    assert!(
        calls
            .iter()
            .any(|call| call.uri().contains("chat/completions"))
    );
    assert!(calls.iter().any(|call| {
        call.sensitive_headers()
            .contains(&"authorization".to_owned())
    }));
    assert!(
        calls
            .iter()
            .any(|call| call.uri().contains("chat/completions") && !call.body().is_empty())
    );
}

#[test]
fn a2a_22_no_secrets_in_errors() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http()
        .enqueue_response(400, &[], br#"{"error":"invalid_grant"}"#.to_vec());
    let error = block_on(tokens::refresh_for_test(
        host.context().clone(),
        endpoints(),
        id.clone(),
        secret("cline-access-1"),
    ))
    .expect_err("refresh");
    assert!(!error.message().contains("cline-refresh-1"));
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert!(parsed.flag(tokens::LAPSED));
}

#[test]
fn a2a_23_api_key_sign_in_stores_api_auth() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json!({"success": true, "data": {"email": "api@cline.test", "id": "u-9"}})
            .to_string()
            .into_bytes(),
    );
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Token(secret("sk-cline-test-key")),
        host.context(),
    ))
    .expect("api sign-in");
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&blob).expect("blob");
    assert_eq!(parsed.text("auth_type"), Some("api"));
    assert!(parsed.secret("api_key").is_some());
}

#[test]
fn a2a_24_default_expiry_ms_uses_host_clock() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let now = host
        .context()
        .clock()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("epoch")
        .as_millis() as u64;
    let parsed = tokens::parse_register(
        host.context(),
        &json!({"accessToken": "tok", "refreshToken": "rt"}),
        None,
    )
    .expect("parse");
    assert!(parsed.expires_ms >= now + 3_500_000);
    assert!(parsed.expires_ms <= now + 3_700_000);
}

#[test]
fn a2a_25_invalid_json_400_does_not_set_lapsed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    block_on(async {
        let accounts = host.context().accounts();
        let secret = accounts.secret(&id).await.expect("read").expect("stored");
        let blob = SecretObject::parse(&secret).expect("blob");
        let updated = blob.with_number("expires_ms", 1);
        let account = accounts
            .account(id.clone(), "user@cline.test")
            .expect("account");
        accounts
            .upsert(account, updated.to_secret())
            .await
            .expect("upsert");
    });
    host.http()
        .enqueue_response(400, &[], br#"{"error":"invalid JSON payload"}"#.to_vec());
    let error = block_on(tokens::refresh_for_test(
        host.context().clone(),
        endpoints(),
        id.clone(),
        secret("cline-access-1"),
    ))
    .expect_err("refresh");
    assert_ne!(error.kind(), PluginErrorKind::Auth);
    let blob = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert!(
        !SecretObject::parse(&blob)
            .expect("blob")
            .flag(tokens::LAPSED)
    );
}

#[test]
fn prettify_names_models() {
    assert_eq!(
        models::prettify("anthropic/claude-sonnet-5.5"),
        "Claude Sonnet 5.5"
    );
}

#[test]
fn r53_client_headers_identify_as_the_reference_cli() {
    let headers = crate::client::headers(Some("task-1"));
    let get = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(get("X-CLIENT-TYPE"), Some("cline-cli"));
    assert_eq!(get("X-CLIENT-VERSION"), Some("3.0.68"));
    assert_eq!(get("X-PLATFORM"), Some("cli"));
    assert_eq!(get("X-PLATFORM-VERSION"), Some("3.0.68"));
    assert_eq!(get("X-CORE-VERSION"), Some("0.0.90"));
    assert_eq!(get("User-Agent"), Some("Cline/3.0.68"));
    assert_eq!(get("X-Task-ID"), Some("task-1"));
    assert!(
        crate::client::headers(Some(""))
            .iter()
            .all(|(key, _)| key != "X-Task-ID")
    );
}

#[test]
fn r53_chat_sends_the_task_id_derived_from_the_host_session() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_oauth(&host);
    host.http().enqueue(
        json!({
            "success": true,
            "data": {
                "id": "chat-1",
                "object": "chat.completion",
                "choices": [{"message": {"role": "assistant", "content": "ok"}}]
            }
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let mut request = Request::new("anthropic/claude-sonnet-5.5");
    request.extensions.insert(
        "hofund_client_request_headers",
        json!({"x-session-id": "session-one"}),
    );
    block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("chat");
    let calls = host.http().calls();
    let chat = calls
        .iter()
        .find(|call| call.uri().contains("chat/completions"))
        .expect("chat call");
    for name in [
        "x-task-id",
        "x-client-type",
        "x-client-version",
        "x-platform",
        "x-platform-version",
        "x-core-version",
        "user-agent",
    ] {
        assert!(
            chat.sensitive_headers().contains(&name.to_owned()),
            "{name}"
        );
    }
}
