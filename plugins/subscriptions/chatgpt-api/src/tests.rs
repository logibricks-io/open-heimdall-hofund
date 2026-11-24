//! Mock-backed tests for Sign in with ChatGPT.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, RawTable, SecretString, SignInChallenge,
    SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::oauth::{Pkce, SecretObject};

use crate::errors;
use crate::host_id;
use crate::models;
use crate::shape;
use crate::sign_in;
use crate::tokens::{self, Need};
use crate::{Endpoints, factory_with_endpoints};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const TOKEN_URL: &str = "https://auth.test/api/accounts/oauth/token";
const API: &str = "https://api.test/v1";

fn endpoints() -> Endpoints {
    Endpoints {
        issuer: "https://auth.test".to_owned(),
        authorize: "https://auth.test/api/accounts/authorize".to_owned(),
        token: TOKEN_URL.to_owned(),
        api: API.to_owned(),
        port: None,
    }
}

fn plugin() -> PluginId {
    PluginId::from_static("chatgpt-api")
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

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let count = chunk.len() + 1;
        for i in 0..count {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

fn jwt(claims: &str) -> String {
    format!("e30.{}.sig", b64url(claims.as_bytes()))
}

fn id_token(email: &str, sub: &str, nonce: &str, client_id: &str) -> String {
    jwt(&format!(
        r#"{{"email":"{email}","sub":"{sub}","nonce":"{nonce}","iss":"https://auth.openai.com","aud":"{client_id}","https://api.openai.com/auth":{{"chatgpt_plan_type":"plus"}}}}"#
    ))
}

fn token_reply(access: &str, refresh: &str, id: &str) -> Vec<u8> {
    format!(
        r#"{{"access_token":"{access}","refresh_token":"{refresh}","id_token":"{id}","expires_in":3600,"scope":"openid offline_access {}"}}"#,
        sign_in::DIRECT_SCOPE
    )
    .into_bytes()
}

fn query_pairs(url: &str) -> Vec<(String, String)> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[test]
fn authorize_url_carries_dynamic_client_and_host_id() {
    let pkce = Pkce::generate().expect("pkce");
    let url = sign_in::authorize(
        "https://auth.test/api/accounts/authorize",
        "http://127.0.0.1:1455/auth/callback",
        &pkce,
        "state-1",
        "nonce-1",
        "urn:uuid:00000000-0000-4000-8000-000000000001",
        None,
    )
    .expect("url");
    let pairs = query_pairs(&url);
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(get("client_id"), sign_in::DYNAMIC_CLIENT);
    assert_eq!(get("agent_name_hint"), sign_in::AGENT_NAME);
    assert_eq!(
        get("ext_agent_host_id"),
        "urn%3Auuid%3A00000000-0000-4000-8000-000000000001"
    );
    assert_eq!(get("code_challenge_method"), "S256");
    assert_eq!(get("resource"), "https%3A%2F%2Fapi.openai.com%2Fv1");
    assert!(get("scope").contains(sign_in::DIRECT_SCOPE));
}

#[test]
fn host_id_is_stable() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let first = host_id::load(host.context()).expect("first");
    let second = host_id::load(host.context()).expect("second");
    assert_eq!(first, second);
    assert!(first.starts_with("urn:uuid:"));
}

#[test]
fn host_id_load_for_test_seeds_state() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let seeded = "urn:uuid:00000000-0000-4000-8000-000000000001";
    host_id::load_for_test(host.context(), seeded).expect("seed");
    assert_eq!(host_id::load(host.context()).expect("load"), seeded);
}

#[test]
fn sign_in_stores_per_account_client_id() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::OpenUrl { url, .. } = challenge else {
        panic!("open url");
    };
    let pairs = query_pairs(&url);
    let state = pairs
        .iter()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.clone())
        .expect("state");
    let nonce = pairs
        .iter()
        .find(|(k, _)| k == "nonce")
        .map(|(_, v)| v.clone())
        .expect("nonce");
    let idt = id_token("me@example.test", "sub-1", &nonce, "client-issued");
    host.http().enqueue(token_reply("acc", "ref", &idt));
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Callback {
            code: SecretString::from("code-1".to_owned()),
            state,
            issued_client_id: Some("client-issued".to_owned()),
        },
        host.context(),
    ))
    .expect("signed in");
    let blob = SecretObject::parse(
        &block_on(host.context().accounts().secret(&id))
            .expect("read")
            .expect("secret"),
    )
    .expect("blob");
    assert_eq!(blob.text("client_id"), Some("client-issued"));
}

#[test]
fn refresh_uses_per_account_client_id_and_lapses_on_dead_grant() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let account = AccountId::from_static("a-0000000000000001");
    let blob = SecretObject::new()
        .with_text("client_id", "client-issued")
        .with_secret("access_token", &SecretString::from("stale".to_owned()))
        .with_secret("refresh_token", &SecretString::from("refresh-1".to_owned()))
        .with_number("expires_at", 0);
    block_on(async {
        let accounts = host.context().accounts();
        let row = accounts
            .account(account.clone(), "me@example.test")
            .expect("account");
        accounts
            .upsert(row, blob.to_secret())
            .await
            .expect("upsert");
    });
    host.http()
        .enqueue_response(400, &[], br#"{"error":"invalid_grant"}"#);
    let error = block_on(tokens::rotate_for_test(
        host.context().clone(),
        endpoints(),
        account.clone(),
        SecretString::from("stale".to_owned()),
    ))
    .expect_err("lapsed");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    let stored = SecretObject::parse(
        &block_on(host.context().accounts().secret(&account))
            .expect("read")
            .expect("secret"),
    )
    .expect("blob");
    assert!(stored.flag(tokens::LAPSED));
}

#[test]
fn models_fixture_parses_visibility_and_images() {
    let body = include_bytes!("fixtures/models.json");
    let list = models::parse_fixture(body).expect("models");
    assert_eq!(list.models().len(), 2);
    assert_eq!(list.models()[0].id(), "gpt-4.1");
    assert!(list.models()[1].image_input().is_some_and(|v| v));
}

#[test]
fn shape_drops_hosted_tools_and_sets_prompt_cache_key() {
    let mut body = json!({
        "instructions": "agent",
        "input": [{"role": "user", "content": "hi"}],
        "tools": [
            {"type": "function", "name": "run"},
            {"type": "mcp"}
        ],
        "store": true,
        "temperature": 0.2
    });
    let map = body.as_object_mut().expect("object");
    shape::body(map);
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["stream"], json!(true));
    assert!(body.get("temperature").is_none());
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    assert!(body.get("prompt_cache_key").is_some());
}

#[test]
fn d232_chatgpt_api_puts_instructions_first_on_gpt_6_2() {
    let user = json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]});
    for model in [
        "gpt-6.2-astra",
        "gpt-6.2",
        "gpt-6.10",
        "gpt-7",
        "openai/gpt-6.2-sol",
    ] {
        let mut body = json!({
            "model": model,
            "instructions": "Be brief.",
            "input": "hello",
        });
        let map = body.as_object_mut().expect("object");
        shape::body(map);
        assert!(body.get("instructions").is_none(), "{model}: {body}");
        let input = body["input"].as_array().expect("input");
        assert_eq!(input.len(), 2, "{model}");
        assert_eq!(input[0]["role"], json!("developer"));
        assert_eq!(input[0]["type"], json!("message"));
        assert_eq!(input[0]["content"][0]["text"], json!("Be brief."));
        assert_eq!(input[1], user);
        let key = body["prompt_cache_key"].as_str().expect("cache key");
        assert!(!key.is_empty(), "{model}");
    }
    for model in ["gpt-6-astra", "gpt-6.1", "gpt-5.5", "o3", "claude-opus"] {
        let mut body = json!({
            "model": model,
            "instructions": "Be brief.",
            "input": "hello",
        });
        let map = body.as_object_mut().expect("object");
        shape::body(map);
        assert_eq!(body["instructions"], json!("Be brief."), "{model}");
        assert_eq!(body["input"].as_array().map(Vec::len), Some(1), "{model}");
        assert_eq!(body["input"][0]["role"], json!("user"), "{model}");
    }
}

#[test]
fn explain_maps_subscription_sharing_codes() {
    let body = br#"{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}"#;
    let text = errors::explain(429, body).expect("hint");
    assert!(text.contains(errors::USAGE_URL));
}

#[test]
fn quota_returns_nonblocking_display_line() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let windows = block_on(sub.quota(
        &AccountId::from_static("a-0000000000000001"),
        host.context(),
    ))
    .expect("quota");
    assert_eq!(windows.len(), 1);
    assert!(!windows[0].blocking());
    assert!(windows[0].display().contains(errors::USAGE_URL));
}

#[test]
fn refresh_posts_client_id_and_resource() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let account = AccountId::from_static("a-0000000000000001");
    let blob = SecretObject::new()
        .with_text("client_id", "client-issued")
        .with_secret("access_token", &SecretString::from("stale".to_owned()))
        .with_secret("refresh_token", &SecretString::from("refresh-1".to_owned()))
        .with_number("expires_at", 0);
    block_on(async {
        let accounts = host.context().accounts();
        let row = accounts
            .account(account.clone(), "me@example.test")
            .expect("account");
        accounts
            .upsert(row, blob.to_secret())
            .await
            .expect("upsert");
    });
    host.http().enqueue(token_reply(
        "new-acc",
        "refresh-1",
        &id_token("a@b", "s", "n", "client-issued"),
    ));
    let access = block_on(tokens::access(
        host.context(),
        &endpoints(),
        &account,
        Need::Always,
    ))
    .expect("refresh");
    let body = String::from_utf8(host.http().calls()[0].body().to_vec()).expect("utf8");
    assert!(body.contains("client_id=client-issued"));
    assert!(body.contains("resource=https%3A%2F%2Fapi.openai.com%2Fv1"));
    assert_eq!(secret_text(&access.token), "new-acc");
}

#[test]
fn refresh_margin_constant_matches_reference() {
    assert_eq!(tokens::REFRESH_MARGIN.as_secs(), 180);
}

const NOW_SECS: u64 = 1_700_000_000;
const AGAIN_SESSION: &str = "abcdef0123456789abcdef0123456789";
const PROBE_SESSION: &str = "fedcba9876543210fedcba9876543210";

fn fixed_now() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(NOW_SECS)
}

fn host_at(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_at(PluginKind::Subscription, plugin(), dir.path(), fixed_now())
}

fn ctx_for(session: &str, user: Option<&str>) -> SignInCtx {
    let ctx = SignInCtx::new(plugin())
        .with_session(session)
        .expect("session");
    match user {
        Some(user) => ctx.with_option("user", user).expect("user"),
        None => ctx,
    }
}

fn id_token_at(email: &str, sub: &str, nonce: &str, client_id: &str, exp: u64) -> String {
    jwt(&format!(
        r#"{{"email":"{email}","sub":"{sub}","nonce":"{nonce}","iss":"https://auth.openai.com","aud":"{client_id}","exp":{exp},"https://api.openai.com/auth":{{"chatgpt_plan_type":"plus"}}}}"#
    ))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &text[index + 1..index + 3];
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(b'%');
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_value(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(name) == key).then(|| percent_decode(value))
    })
}

fn open_url(challenge: SignInChallenge) -> String {
    let SignInChallenge::OpenUrl { url, .. } = challenge else {
        panic!("open url");
    };
    url
}

fn token_bodies(host: &TestHost) -> Vec<String> {
    host.http()
        .calls()
        .into_iter()
        .filter(|call| call.uri().contains("/oauth/token"))
        .map(|call| String::from_utf8(call.body().to_vec()).unwrap_or_default())
        .collect()
}

fn stored_text(host: &TestHost, id: &AccountId, key: &str) -> String {
    let blob = SecretObject::parse(
        &block_on(host.context().accounts().secret(id))
            .expect("read")
            .expect("secret"),
    )
    .expect("blob");
    blob.text(key).unwrap_or_default().to_owned()
}

fn stored_access(host: &TestHost, id: &AccountId) -> String {
    let blob = SecretObject::parse(
        &block_on(host.context().accounts().secret(id))
            .expect("read")
            .expect("secret"),
    )
    .expect("blob");
    blob.secret("access_token")
        .map(|token| secret_text(&token).to_owned())
        .unwrap_or_default()
}

fn sign_up(host: &TestHost, sub: &dyn Subscription) -> (AccountId, String) {
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let url = open_url(challenge);
    let state = query_value(&url, "state").expect("state");
    let nonce = query_value(&url, "nonce").expect("nonce");
    let idt = id_token_at(
        "me@example.com",
        "user-1",
        &nonce,
        "oaiapp_123",
        NOW_SECS + 3600,
    );
    host.http()
        .enqueue(token_reply("access-first", "refresh-first", &idt));
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    let id = block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Callback {
            code: SecretString::from("code-first".to_owned()),
            state,
            issued_client_id: Some("oaiapp_123".to_owned()),
        },
        host.context(),
    ))
    .expect("signed in");
    (id, idt)
}

#[test]
fn d257_sign_in_again_uses_the_saved_client_and_hints() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir);
    let sub = subscription(&host);
    let (id, idt) = sign_up(&host, sub.as_ref());
    assert_eq!(stored_text(&host, &id, "client_id"), "oaiapp_123");

    let challenge = block_on(sub.start_sign_in(
        &ctx_for(AGAIN_SESSION, Some("Me@Example.COM")),
        host.context(),
    ))
    .expect("again");
    let url = open_url(challenge);
    assert_eq!(
        query_value(&url, "client_id").as_deref(),
        Some("oaiapp_123")
    );
    assert_eq!(
        query_value(&url, "id_token_hint").as_deref(),
        Some(idt.as_str())
    );
    assert_eq!(
        query_value(&url, "login_hint").as_deref(),
        Some("me@example.com")
    );
    assert!(query_value(&url, "agent_name_hint").is_none());
    assert_eq!(
        query_value(&url, "ext_agent_host_id").as_deref(),
        Some(host_id::load(host.context()).expect("host id").as_str())
    );
    assert_eq!(
        query_value(&url, "resource").as_deref(),
        Some("https://api.openai.com/v1")
    );
    assert!(
        query_value(&url, "scope")
            .unwrap_or_default()
            .contains(sign_in::DIRECT_SCOPE)
    );

    let probe = block_on(sub.start_sign_in(&ctx_for(PROBE_SESSION, None), host.context()))
        .expect("new account");
    let probe_url = open_url(probe);
    assert_eq!(
        query_value(&probe_url, "client_id").as_deref(),
        Some(sign_in::DYNAMIC_CLIENT)
    );
    assert_eq!(
        query_value(&probe_url, "agent_name_hint").as_deref(),
        Some(sign_in::AGENT_NAME)
    );
    block_on(sub.cancel_sign_in(&ctx_for(PROBE_SESSION, None), host.context())).expect("cancel");

    let state = query_value(&url, "state").expect("state");
    let nonce = query_value(&url, "nonce").expect("nonce");
    let next = id_token_at(
        "me@example.com",
        "user-1",
        &nonce,
        "oaiapp_123",
        NOW_SECS + 3600,
    );
    host.http()
        .enqueue(token_reply("access-again", "refresh-again", &next));
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    let again = block_on(sub.complete_sign_in(
        &ctx_for(AGAIN_SESSION, Some("me@example.com")),
        SignInProof::Callback {
            code: SecretString::from("code-again".to_owned()),
            state,
            issued_client_id: None,
        },
        host.context(),
    ))
    .expect("signed in again");
    assert_eq!(again, id);
    let traded = token_bodies(&host);
    assert!(
        traded
            .last()
            .is_some_and(|body| body.contains("client_id=oaiapp_123")),
        "the code was not traded under the saved client"
    );
    assert_eq!(stored_text(&host, &id, "client_id"), "oaiapp_123");
    assert_eq!(stored_access(&host, &id), "access-again");
}

#[test]
fn d257_another_client_replaces_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir);
    let sub = subscription(&host);
    let (id, _) = sign_up(&host, sub.as_ref());
    let before = token_bodies(&host).len();
    let challenge = block_on(sub.start_sign_in(
        &ctx_for(AGAIN_SESSION, Some("me@example.com")),
        host.context(),
    ))
    .expect("again");
    let state = query_value(&open_url(challenge), "state").expect("state");
    let error = block_on(sub.complete_sign_in(
        &ctx_for(AGAIN_SESSION, None),
        SignInProof::Callback {
            code: SecretString::from("code-other".to_owned()),
            state,
            issued_client_id: Some("oaiapp_999".to_owned()),
        },
        host.context(),
    ))
    .expect_err("other client");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(error.to_string().contains("another client"));
    assert_eq!(token_bodies(&host).len(), before);
    assert_eq!(stored_text(&host, &id, "client_id"), "oaiapp_123");
    assert_eq!(stored_access(&host, &id), "access-first");
}

#[test]
fn d257_another_subject_replaces_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir);
    let sub = subscription(&host);
    let (id, _) = sign_up(&host, sub.as_ref());
    let challenge = block_on(sub.start_sign_in(
        &ctx_for(AGAIN_SESSION, Some("me@example.com")),
        host.context(),
    ))
    .expect("again");
    let url = open_url(challenge);
    let state = query_value(&url, "state").expect("state");
    let nonce = query_value(&url, "nonce").expect("nonce");
    let other = id_token_at(
        "other@example.com",
        "user-2",
        &nonce,
        "oaiapp_123",
        NOW_SECS + 3600,
    );
    host.http()
        .enqueue(token_reply("access-other", "refresh-other", &other));
    let error = block_on(sub.complete_sign_in(
        &ctx_for(AGAIN_SESSION, None),
        SignInProof::Callback {
            code: SecretString::from("code-other".to_owned()),
            state,
            issued_client_id: None,
        },
        host.context(),
    ))
    .expect_err("other subject");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(error.to_string().contains("not the account"));
    assert!(error.to_string().contains("other@example.com"));
    assert_eq!(stored_text(&host, &id, "client_id"), "oaiapp_123");
    assert_eq!(stored_access(&host, &id), "access-first");
}

#[test]
fn d257_stale_id_token_hint_is_left_out() {
    let pkce = Pkce::generate().expect("pkce");
    let now = fixed_now();
    let cases = [
        (NOW_SECS - 2 * 3600, false),
        (NOW_SECS + 3600, true),
        (NOW_SECS + 60, false),
        (NOW_SECS + 61, true),
    ];
    for (exp, hint) in cases {
        let token = jwt(&format!(r#"{{"sub":"user-1","exp":{exp}}}"#));
        assert_eq!(sign_in::id_token_hint_live(&token, now), hint, "exp {exp}");
        let url = sign_in::authorize(
            "https://auth.test/api/accounts/authorize",
            "http://127.0.0.1:1/auth/callback",
            &pkce,
            "state-1",
            "nonce-1",
            "urn:uuid:x",
            Some(sign_in::AuthorizeSaved {
                client_id: "oaiapp_123",
                id_token: &token,
                email: "me@example.com",
                now,
            }),
        )
        .expect("url");
        assert_eq!(query_value(&url, "id_token_hint").is_some(), hint);
        assert_eq!(
            query_value(&url, "client_id").as_deref(),
            Some("oaiapp_123")
        );
        assert_eq!(
            query_value(&url, "login_hint").as_deref(),
            Some("me@example.com")
        );
        assert!(query_value(&url, "agent_name_hint").is_none());
    }
    for token in ["", "not-a-jwt"] {
        assert!(!sign_in::id_token_hint_live(token, now));
        let url = sign_in::authorize(
            "https://auth.test/api/accounts/authorize",
            "http://127.0.0.1:1/auth/callback",
            &pkce,
            "state-1",
            "nonce-1",
            "urn:uuid:x",
            Some(sign_in::AuthorizeSaved {
                client_id: "oaiapp_123",
                id_token: token,
                email: "me@example.com",
                now,
            }),
        )
        .expect("url");
        assert!(query_value(&url, "id_token_hint").is_none());
        assert_eq!(
            query_value(&url, "login_hint").as_deref(),
            Some("me@example.com")
        );
    }

    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir);
    let sub = subscription(&host);
    let (id, _) = sign_up(&host, sub.as_ref());
    let stale = id_token_at(
        "me@example.com",
        "user-1",
        "old-nonce",
        "oaiapp_123",
        NOW_SECS - 2 * 3600,
    );
    let blob = SecretObject::parse(
        &block_on(host.context().accounts().secret(&id))
            .expect("read")
            .expect("secret"),
    )
    .expect("blob")
    .with_secret("id_token", &SecretString::from(stale));
    block_on(async {
        let accounts = host.context().accounts();
        let row = accounts
            .account(id.clone(), "me@example.com")
            .expect("account");
        accounts
            .upsert(row, blob.to_secret())
            .await
            .expect("upsert");
    });
    let challenge = block_on(sub.start_sign_in(
        &ctx_for(AGAIN_SESSION, Some("me@example.com")),
        host.context(),
    ))
    .expect("again");
    let url = open_url(challenge);
    assert_eq!(
        query_value(&url, "client_id").as_deref(),
        Some("oaiapp_123")
    );
    assert!(query_value(&url, "id_token_hint").is_none());
    assert_eq!(
        query_value(&url, "login_hint").as_deref(),
        Some("me@example.com")
    );
    assert!(query_value(&url, "agent_name_hint").is_none());
}

#[test]
fn d257_unknown_user_stays_dynamic() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir);
    let sub = subscription(&host);
    let _ = sign_up(&host, sub.as_ref());
    let challenge = block_on(sub.start_sign_in(
        &ctx_for(AGAIN_SESSION, Some("nobody@example.com")),
        host.context(),
    ))
    .expect("new");
    let url = open_url(challenge);
    assert_eq!(
        query_value(&url, "client_id").as_deref(),
        Some(sign_in::DYNAMIC_CLIENT)
    );
    assert_eq!(
        query_value(&url, "agent_name_hint").as_deref(),
        Some(sign_in::AGENT_NAME)
    );
    let state = query_value(&url, "state").expect("state");
    let error = block_on(sub.complete_sign_in(
        &ctx_for(AGAIN_SESSION, None),
        SignInProof::Callback {
            code: SecretString::from("code-new".to_owned()),
            state,
            issued_client_id: None,
        },
        host.context(),
    ))
    .expect_err("no client");
    assert!(error.to_string().contains("no client id"));
}

#[test]
fn d257_same_callback_client_is_traded() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host_at(&dir);
    let sub = subscription(&host);
    let (id, _) = sign_up(&host, sub.as_ref());
    let challenge =
        block_on(sub.start_sign_in(&ctx_for(AGAIN_SESSION, Some("user-1")), host.context()))
            .expect("again");
    let url = open_url(challenge);
    assert_eq!(
        query_value(&url, "client_id").as_deref(),
        Some("oaiapp_123")
    );
    let state = query_value(&url, "state").expect("state");
    let nonce = query_value(&url, "nonce").expect("nonce");
    let next = id_token_at(
        "me@example.com",
        "user-1",
        &nonce,
        "oaiapp_123",
        NOW_SECS + 3600,
    );
    host.http()
        .enqueue(token_reply("access-same", "refresh-same", &next));
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    block_on(sub.complete_sign_in(
        &ctx_for(AGAIN_SESSION, None),
        SignInProof::Callback {
            code: SecretString::from("code-same".to_owned()),
            state,
            issued_client_id: Some("  oaiapp_123  ".to_owned()),
        },
        host.context(),
    ))
    .expect("same client");
    assert_eq!(stored_access(&host, &id), "access-same");
    assert_eq!(stored_text(&host, &id, "client_id"), "oaiapp_123");
}
