//! Offline tests against the recording subscription host.

use std::sync::Arc;
use std::time::Duration;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, Completion, PluginErrorKind, PluginId, PluginKind, ProtocolId,
    RawTable, RawValue, SecretString, SignInChallenge, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache, account_id_from};

use crate::project;
use crate::{Endpoints, SIGN_IN_WARNING, client, factory_with_endpoints, tokens};

const SESSION: &str = "0123456789abcdef0123456789abcdef";
const TOKEN_URL: &str = "https://oauth.test/token";
const USERINFO_URL: &str = "https://oauth.test/userinfo";
const LOAD: &str = "https://code-assist-prod.test";
const DAILY: &str = "https://code-assist-daily.test";

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
        token: TOKEN_URL.to_owned(),
        userinfo: USERINFO_URL.to_owned(),
        load_code_assist: LOAD.to_owned(),
        code_assist: DAILY.to_owned(),
        onboard_poll: Duration::from_millis(1),
    }
}

const TEST_CLIENT_ID: &str = "hofund-test-antigravity-client";
const TEST_CLIENT_SECRET: &str = "hofund-test-antigravity-secret";

fn test_oauth() -> client::OauthClient {
    client::OauthClient::new(TEST_CLIENT_ID.to_owned(), TEST_CLIENT_SECRET.to_owned())
}

fn configured(raw: &mut RawTable) {
    raw.insert(client::ID_KEY, RawValue::String(TEST_CLIENT_ID.to_owned()));
    raw.insert(
        client::SECRET_KEY,
        RawValue::String(TEST_CLIENT_SECRET.to_owned()),
    );
}

fn plugin() -> PluginId {
    PluginId::from_static("antigravity")
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(PluginKind::Subscription, plugin(), dir.path())
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    subscription_with(host, None)
}

fn subscription_with(host: &TestHost, project_id: Option<&str>) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let mut raw = RawTable::new();
    configured(&mut raw);
    if let Some(project_id) = project_id {
        raw.insert("project", RawValue::String(project_id.to_owned()));
    }
    let config = factory.validate(&raw).expect("config");
    factory.build(&config, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(plugin())
        .with_session(SESSION)
        .expect("session")
}

fn secret(text: &str) -> SecretString {
    SecretString::from(text.to_owned())
}

fn token_reply(access: &str, refresh: Option<&str>) -> Vec<u8> {
    let mut body =
        format!(r#"{{"access_token":"{access}","token_type":"Bearer","expires_in":3600"#);
    if let Some(refresh) = refresh {
        body.push_str(&format!(r#","refresh_token":"{refresh}""#));
    }
    body.push('}');
    body.into_bytes()
}

fn signed_in_blob(project_id: &str) -> SecretObject {
    SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", &secret("refresh-1"))
        .with_text("email", "u@example.test")
        .with_text(project::PROJECT, project_id)
        .with_text(project::TIER, "free-tier")
        .with_text(project::PAID, "")
}

fn seed(host: &TestHost, id: &str, email: &str, blob: SecretObject) -> AccountId {
    let id = AccountId::try_from(id).expect("account id");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts.account(id.clone(), email).expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn posts_to(host: &TestHost, uri: &str) -> Vec<String> {
    host.http()
        .calls()
        .iter()
        .filter(|call| call.uri() == uri)
        .map(|call| String::from_utf8_lossy(call.body()).into_owned())
        .collect()
}

fn method_url(base: &str, method: &str) -> String {
    format!("{base}/v1internal:{method}")
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).expect("hex");
                out.push(u8::from_str_radix(hex, 16).expect("hex digit"));
                index += 2;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out).expect("utf-8")
}

fn query_pairs(url: &str) -> Vec<(String, String)> {
    let query = url
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or_default();
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

fn param(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.clone())
}

fn started_url(host: &TestHost, sub: &Arc<dyn Subscription>) -> String {
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::OpenUrl { url, .. } = challenge else {
        panic!("expected an address to open");
    };
    url
}

fn started_state(host: &TestHost, sub: &Arc<dyn Subscription>) -> String {
    param(&query_pairs(&started_url(host, sub)), "state").expect("state")
}

fn complete(host: &TestHost, sub: &Arc<dyn Subscription>, state: &str) -> AccountId {
    block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Callback {
            code: secret("code-1"),
            state: state.to_owned(),
            issued_client_id: None,
        },
        host.context(),
    ))
    .expect("signed in")
}

fn userinfo(email: &str) -> Vec<u8> {
    format!(r#"{{"id":"1234","email":"{email}","verified_email":true}}"#).into_bytes()
}

#[test]
fn r49c_oauth_uses_the_antigravity_client_and_localhost_callback() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let url = started_url(&host, &sub);
    let pairs = query_pairs(&url);
    assert_eq!(param(&pairs, "client_id").as_deref(), Some(TEST_CLIENT_ID));
    let redirect = param(&pairs, "redirect_uri").expect("redirect");
    let rest = redirect
        .strip_prefix("http://localhost:")
        .unwrap_or_else(|| panic!("localhost redirect: {redirect}"));
    let (port, path) = rest.split_once('/').expect("path");
    assert_ne!(port.parse::<u16>().expect("port"), 0);
    assert_eq!(path, "oauth-callback");
    assert_eq!(param(&pairs, "response_type").as_deref(), Some("code"));
    assert_eq!(param(&pairs, "access_type").as_deref(), Some("offline"));
    assert_eq!(
        param(&pairs, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert!(param(&pairs, "code_challenge").is_some_and(|value| value.len() == 43));
    let scope = param(&pairs, "scope").expect("scope");
    assert!(scope.contains("https://www.googleapis.com/auth/cclog"));
    assert!(scope.contains("https://www.googleapis.com/auth/experimentsandconfigs"));
    assert_eq!(
        SIGN_IN_WARNING,
        "Google may suspend an Antigravity account it sees used outside Antigravity. Use one you can afford to lose."
    );
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
}

#[test]
fn r49c_sign_in_stores_a_separate_google_account_and_never_an_access_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http().enqueue(token_reply("acc-1", Some("ref-1")));
    host.http().enqueue(userinfo("U@Example.test"));
    host.http().enqueue(
        br#"{"currentTier":{"id":"standard-tier","name":"Antigravity"},"paidTier":{"id":"g1-pro-tier","name":"Google AI Pro"},"cloudaicompanionProject":"proj-1"}"#
            .to_vec(),
    );
    let id = complete(&host, &sub, &state);
    assert_eq!(id, account_id_from(&["antigravity", "u@example.test"]));
    assert_ne!(id, account_id_from(&["google", "u@example.test"]));
    let blob = stored(&host, &id);
    assert_eq!(blob.text(project::PROJECT), Some("proj-1"));
    assert_eq!(blob.text(project::TIER), Some("standard-tier"));
    assert_eq!(blob.text(project::PAID), Some("g1-pro-tier"));
    assert!(blob.secret("refresh_token").is_some());
    assert!(blob.secret("access_token").is_none());
    assert_eq!(
        block_on(sub.account_status(&id, host.context())).expect("status"),
        AccountStatus::Ready
    );
    let [load] = posts_to(&host, &method_url(LOAD, "loadCodeAssist"))
        .try_into()
        .expect("one load");
    let load: Value = hofund_plugin_sdk::json::from_str(&load).expect("json");
    assert_eq!(load["metadata"]["ideType"], "ANTIGRAVITY");
    assert!(posts_to(&host, &method_url(DAILY, "onboardUser")).is_empty());

    let [form] = posts_to(&host, TOKEN_URL).try_into().expect("one exchange");
    let form = query_pairs(&format!("?{form}"));
    assert_eq!(param(&form, "client_id").as_deref(), Some(TEST_CLIENT_ID));
    assert_eq!(
        param(&form, "client_secret").as_deref(),
        Some(TEST_CLIENT_SECRET)
    );
    assert_eq!(
        param(&form, "grant_type").as_deref(),
        Some("authorization_code")
    );
}

#[test]
fn r49c_onboarding_uses_the_daily_base() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let state = started_state(&host, &sub);
    host.http().enqueue(token_reply("acc-1", Some("ref-1")));
    host.http().enqueue(userinfo("u@example.test"));
    host.http().enqueue(
        br#"{"allowedTiers":[{"id":"free-tier","name":"Free","isDefault":true}]}"#.to_vec(),
    );
    host.http().enqueue(
        br#"{"done":true,"response":{"cloudaicompanionProject":{"id":"proj-free"}}}"#.to_vec(),
    );
    let id = complete(&host, &sub, &state);
    assert_eq!(stored(&host, &id).text(project::PROJECT), Some("proj-free"));
    let [ask] = posts_to(&host, &method_url(DAILY, "onboardUser"))
        .try_into()
        .expect("one onboard");
    let ask: Value = hofund_plugin_sdk::json::from_str(&ask).expect("json");
    assert_eq!(ask["tier_id"], "free-tier");
    assert_eq!(ask["metadata"]["ide_type"], "ANTIGRAVITY");
}

#[test]
fn r49c_refresh_uses_google_and_keeps_the_access_token_in_memory() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-1"),
    );
    let before = stored(&host, &id).to_body();
    let cache = TokenCache::new(tokens::REFRESH_MARGIN);
    host.http().enqueue(token_reply("acc-1", None));
    block_on(tokens::access(
        host.context(),
        &endpoints(),
        &test_oauth(),
        &cache,
        &id,
    ))
    .expect("refresh");
    block_on(tokens::access(
        host.context(),
        &endpoints(),
        &test_oauth(),
        &cache,
        &id,
    ))
    .expect("cached");
    let [form] = posts_to(&host, TOKEN_URL).try_into().expect("one refresh");
    let form = query_pairs(&format!("?{form}"));
    assert_eq!(param(&form, "grant_type").as_deref(), Some("refresh_token"));
    assert_eq!(param(&form, "refresh_token").as_deref(), Some("refresh-1"));
    assert_eq!(param(&form, "client_id").as_deref(), Some(TEST_CLIENT_ID));
    assert_eq!(
        param(&form, "client_secret").as_deref(),
        Some(TEST_CLIENT_SECRET)
    );
    assert_eq!(stored(&host, &id).to_body(), before);
}

#[test]
fn r49c_refused_refresh_lapses_only_that_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-1"),
    );
    let sub = subscription(&host);
    host.http()
        .enqueue_response(400, &[], br#"{"error":"invalid_grant"}"#.to_vec());
    let error = block_on(sub.refresh(&id, host.context())).expect_err("refused");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(stored(&host, &id).flag(tokens::LAPSED));
    assert!(matches!(
        block_on(sub.account_status(&id, host.context())).expect("status"),
        AccountStatus::NeedsSignIn { .. }
    ));
}

const GEMINI_ONLY: &str = r#"{"models":{
    "gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)","maxTokens":1048576,"quotaInfo":{"remainingFraction":1}},
    "gemini-2.5-pro":{"displayName":"Gemini 2.5 Pro"},
    "tab_flash_lite_preview":{},
    "chat_20706":{}
}}"#;
const WITH_CLAUDE: &str = r#"{"models":{
    "gemini-3.8-flash-high":{"displayName":"Gemini 3.8 Flash (High)","maxTokens":1048576,"quotaInfo":{"remainingFraction":1}},
    "claude-opus-5-5":{"displayName":"Claude Opus 5.5","maxTokens":250000,"quotaInfo":{"remainingFraction":1}},
    "claude-sonnet-5-5":{"displayName":"Claude Sonnet 5.5","disabled":true,"quotaInfo":{"remainingFraction":1}}
}}"#;

#[test]
fn r49c_paid_tier_falls_back_and_a_400_tier_is_rested() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let blob = signed_in_blob("aicode-consumers")
        .with_text(project::TIER, "standard-tier")
        .with_text(project::PAID, "g1-pro-tier");
    let id = seed(&host, "a-0000000000000001", "pro@example.test", blob);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-1", None));
    host.http()
        .enqueue_response(400, &[], br#"{"error":{"message":"bad tier"}}"#.to_vec());
    host.http().enqueue(WITH_CLAUDE.as_bytes().to_vec());
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
    assert_eq!(ids, ["claude-opus-5-5", "gemini-3.8-flash-high"]);
    let calls = posts_to(&host, &method_url(DAILY, "fetchAvailableModels"));
    assert_eq!(calls.len(), 2);
    let first: Value = hofund_plugin_sdk::json::from_str(&calls[0]).expect("first");
    let second: Value = hofund_plugin_sdk::json::from_str(&calls[1]).expect("second");
    assert_eq!(first["entitlement"]["userTier"], "g1-pro-tier");
    assert_eq!(second["entitlement"]["userTier"], "standard-tier");

    host.http().enqueue(WITH_CLAUDE.as_bytes().to_vec());
    block_on(sub.list_models(&id, host.context())).expect("models again");
    let calls = posts_to(&host, &method_url(DAILY, "fetchAvailableModels"));
    assert_eq!(calls.len(), 3, "the refused paid tier is not asked again");
    let third: Value = hofund_plugin_sdk::json::from_str(&calls[2]).expect("third");
    assert_eq!(third["entitlement"]["userTier"], "standard-tier");
}

#[test]
fn r49c_free_tier_names_no_entitlement_and_filters_internal_models() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-1"),
    );
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-1", None));
    host.http().enqueue(GEMINI_ONLY.as_bytes().to_vec());
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    assert_eq!(list.models().len(), 1);
    assert_eq!(list.models()[0].id(), "gemini-3.8-flash-high");
    assert_eq!(list.models()[0].context(), 1_048_576);
    let [call] = posts_to(&host, &method_url(DAILY, "fetchAvailableModels"))
        .try_into()
        .expect("one model list");
    let call: Value = hofund_plugin_sdk::json::from_str(&call).expect("json");
    assert!(call.get("entitlement").is_none());
}

#[test]
fn r49c_model_pool_unions_accounts_in_picker_order() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let first = seed(
        &host,
        "a-0000000000000001",
        "old@example.test",
        signed_in_blob("project-old"),
    );
    let second_blob = signed_in_blob("project-new")
        .with_secret("refresh_token", &secret("refresh-2"))
        .with_text("email", "new@example.test");
    seed(&host, "a-0000000000000002", "new@example.test", second_blob);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-old", None));
    host.http().enqueue(
        br#"{"models":{"gemini-3.8-flash":{},"claude-opus-4-6-thinking":{"displayName":"Claude Opus 4.6 (Thinking)"},"gpt-oss-120b-medium":{}},"agentModelSorts":[{"groups":[{"modelIds":["gemini-3.8-flash","claude-opus-4-6-thinking","gpt-oss-120b-medium"]}]}]}"#
            .to_vec(),
    );
    host.http().enqueue(token_reply("acc-new", None));
    host.http().enqueue(
        br#"{"models":{"gemini-3.8-flash":{},"claude-opus-5-5-low":{"displayName":"Claude Opus 5.5 (Low)"},"claude-opus-5-5-high":{"displayName":"Claude Opus 5.5 (High)"},"gpt-oss-120b-medium":{}},"agentModelSorts":[{"groups":[{"modelIds":["gemini-3.8-flash","claude-opus-5-5-high","claude-opus-5-5-low","gpt-oss-120b-medium"]}]}]}"#
            .to_vec(),
    );
    let list = block_on(sub.list_models(&first, host.context())).expect("pool");
    let ids: Vec<&str> = list.models().iter().map(|model| model.id()).collect();
    assert_eq!(
        ids,
        [
            "gemini-3.8-flash",
            "claude-opus-5-5",
            "claude-opus-4-6-thinking",
            "gpt-oss-120b-medium"
        ]
    );
    let family = list
        .models()
        .iter()
        .find(|model| model.id() == "claude-opus-5-5")
        .expect("family");
    assert_eq!(family.display_name(), Some("Claude Opus 5.5"));
    assert_eq!(family.efforts(), ["low", "high"]);

    host.http()
        .enqueue_response(503, &[], b"unavailable".to_vec());
    host.http()
        .enqueue_response(503, &[], b"unavailable".to_vec());
    let kept = block_on(sub.list_models(&first, host.context())).expect("cached pool");
    assert_eq!(kept.models(), list.models());
}

#[test]
fn r49c_effort_folding_matches_the_reference_names() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-1"),
    );
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-1", None));
    host.http().enqueue(
        br#"{"models":{
            "gemini-3.7-flash-high":{"displayName":"Gemini 3.7 Flash (High)","maxTokens":1000000,"supportsImages":true},
            "gemini-3.7-flash-low":{"displayName":"Gemini 3.7 Flash (Low)","maxTokens":900000,"supportsImages":true},
            "gemini-3.7-flash-medium":{"displayName":"Gemini 3.7 Flash (Medium)","maxTokens":800000,"supportsImages":false},
            "gemini-3.7-flash-tiered":{}
        },"agentModelSorts":[{"groups":[{"modelIds":["gemini-3.7-flash-high","gemini-3.7-flash-low","gemini-3.7-flash-medium"]}]}]}"#
            .to_vec(),
    );
    let list = block_on(sub.list_models(&id, host.context())).expect("models");
    let family = list
        .models()
        .iter()
        .find(|model| model.id() == "gemini-3.7-flash")
        .expect("family");
    assert_eq!(family.display_name(), Some("Gemini 3.7 Flash"));
    assert_eq!(family.efforts(), ["low", "medium", "high"]);
    assert_eq!(family.context(), 800_000);
    assert_eq!(family.image_input(), Some(false));
    let tiered = list
        .models()
        .iter()
        .find(|model| model.id() == "gemini-3.7-flash-tiered")
        .expect("tiered");
    assert_eq!(tiered.display_name(), Some("Gemini 3.7 Flash (Tiered)"));
    assert_eq!(tiered.efforts(), ["low", "medium", "high"]);

    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let request: hofund_plugin_api::Request = hofund_plugin_sdk::json::from_value(json!({
        "model": "gemini-3.7-flash",
        "messages": [{"role":"user","content":[{"type":"text","text":"hello"}]}],
        "params": {"effort": "low"}
    }))
    .expect("request");
    block_on(upstream.execute(request, ProtocolId::GENERATIVE_LANGUAGE)).expect("execute");
    let [call] = posts_to(&host, &method_url(DAILY, "generateContent"))
        .try_into()
        .expect("one call");
    let call: Value = hofund_plugin_sdk::json::from_str(&call).expect("json");
    assert_eq!(call["model"], "gemini-3.7-flash-low");
    assert_eq!(
        call["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "low"
    );
}

const UNARY_REPLY: &str = r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}},"traceId":"t-1"}"#;

#[test]
fn r49c_recorded_unary_and_stream_use_the_daily_code_assist_envelope() {
    for stream in [false, true] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed(
            &host,
            "a-0000000000000001",
            "u@example.test",
            signed_in_blob("proj-9"),
        );
        let sub = subscription(&host);
        host.http().enqueue(token_reply("acc-1", None));
        if stream {
            host.http().enqueue_response(
                200,
                &[("content-type", "text/event-stream")],
                "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}}\r\n\r\n"
                    .as_bytes()
                    .to_vec(),
            );
        } else {
            host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
        }
        let upstream = sub.upstream(&id, host.context()).expect("upstream");
        let request: hofund_plugin_api::Request = hofund_plugin_sdk::json::from_value(json!({
            "model": "models/gemini-3-flash",
            "messages": [{"role":"user","content":[{"type":"text","text":"hello"}]}],
            "stream": stream
        }))
        .expect("request");
        let completion = block_on(upstream.execute(request, ProtocolId::GENERATIVE_LANGUAGE))
            .expect("recorded reply");
        if stream {
            let Completion::Raw(mut body) = completion else {
                panic!("raw same-protocol stream");
            };
            let text = block_on(async move {
                let mut out = Vec::new();
                while let Some(chunk) = body.next_chunk().await {
                    out.extend_from_slice(&chunk.expect("chunk"));
                }
                String::from_utf8(out).expect("utf-8")
            });
            assert_eq!(
                text,
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\r\n\r\n"
            );
        }
        let method = if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let calls = host.http().calls();
        let call = calls
            .iter()
            .find(|call| call.uri() == method_url(DAILY, method))
            .expect("daily call");
        let sent: Value = hofund_plugin_sdk::json::from_slice(call.body()).expect("json");
        assert_eq!(sent["model"], "gemini-3-flash");
        assert_eq!(sent["project"], "proj-9");
        assert_eq!(sent["userAgent"], "antigravity");
        assert_eq!(sent["requestType"], "agent");
        assert!(
            sent["requestId"]
                .as_str()
                .is_some_and(|id| id.starts_with("agent-"))
        );
        assert!(
            sent["request"]["sessionId"]
                .as_str()
                .is_some_and(|id| id.starts_with('-'))
        );
        assert_eq!(sent["request"]["contents"][0]["parts"][0]["text"], "hello");
        assert!(sent["request"].get("model").is_none());
        assert!(
            call.sensitive_headers()
                .iter()
                .any(|name| name == "authorization")
        );
    }
}

#[test]
fn r49c_quota_reads_model_windows_and_shared_summary() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-9"),
    );
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-1", None));
    host.http().enqueue(
        br#"{"models":{
            "gemini-3.7-flash-high":{"displayName":"Gemini 3.7 Flash (High)","quotaInfo":{"remainingFraction":0.25,"resetTime":"2026-11-01T00:00:00Z"}},
            "gemini-3.7-flash-low":{"displayName":"Gemini 3.7 Flash (Low)","quotaInfo":{}},
            "gpt-oss-120b-medium":{"displayName":"GPT-OSS 120B (Medium)"}
        }}"#
            .to_vec(),
    );
    host.http().enqueue(
        br#"{"groups":[{"displayName":"Gemini Models","buckets":[{"window":"5h","remainingFraction":0.5},{"window":"weekly","remainingFraction":0.75}]}]}"#
            .to_vec(),
    );
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 4);
    assert_eq!(windows[0].name(), "Gemini 3.7 Flash (High)");
    assert_eq!(windows[0].scope_model(), Some("gemini-3.7-flash-high"));
    assert_eq!(windows[0].used(), 7_500);
    assert_eq!(windows[0].limit(), Some(10_000));
    assert_eq!(windows[1].scope_model(), Some("gemini-3.7-flash-low"));
    assert_eq!(
        windows[1].used(),
        10_000,
        "an omitted fraction means none left"
    );
    assert_eq!(windows[2].name(), "Gemini · 5 hours");
    assert!(!windows[2].blocking());
    assert_eq!(windows[3].name(), "Gemini · 7 days");
    let [summary] = posts_to(&host, &method_url(DAILY, "retrieveUserQuotaSummary"))
        .try_into()
        .expect("one summary");
    let summary: Value = hofund_plugin_sdk::json::from_str(&summary).expect("json");
    assert_eq!(summary["project"], "proj-9");
}

#[test]
fn r49c_project_command_names_trims_and_clears_the_project() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("old-project"),
    );
    let sub = subscription(&host);
    block_on(sub.set_project(" U@Example.Test ", "  proj-7  ", host.context()))
        .expect("set project");
    assert_eq!(stored(&host, &id).text(project::PROJECT), Some("proj-7"));
    let error = block_on(sub.set_project("nobody@example.test", "other", host.context()))
        .expect_err("unknown account");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(error.message().contains("no antigravity account"));
    block_on(sub.set_project("u@example.test", " \t ", host.context())).expect("clear");
    assert_eq!(stored(&host, &id).text(project::PROJECT), None);
}

#[test]
fn r49c_cloak_changes_only_known_system_lines() {
    for system in [
        "x-anthropic-billing-header: cc_version=2.1.288;",
        "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
        "You are Codex, an agent based on GPT-5. You share one workspace.",
        "You are Hermes Agent, an intelligent AI assistant created by Nous Research.",
    ] {
        let cloaked = crate::upstream::cloak(system);
        assert_ne!(cloaked, system);
        assert_eq!(cloaked.replace('\u{200b}', ""), system);
        assert_eq!(crate::upstream::cloak(&cloaked), cloaked);
    }
    for system in [
        "You are a helpful assistant.",
        "You are Codex, based on GPT-5.",
        "You are Hermes Agent, built by Nous Research.",
    ] {
        assert_eq!(crate::upstream::cloak(system), system);
    }
}

#[test]
fn r49c_turned_away_429_is_a_non_cooling_400_not_a_quota() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-1"),
    );
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-1", None));
    host.http().enqueue_response(
        429,
        &[("content-type", "application/json")],
        br#"{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED"}}"#
            .to_vec(),
    );
    let request: hofund_plugin_api::Request = hofund_plugin_sdk::json::from_value(json!({
        "model": "gemini-3-flash",
        "messages": [
            {"role":"system","content":[{"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK."}]},
            {"role":"user","content":[{"type":"text","text":"hello"}]}
        ]
    }))
    .expect("request");
    let error = block_on(
        sub.upstream(&id, host.context())
            .expect("upstream")
            .execute(request, ProtocolId::OPENAI_CHAT),
    )
    .err()
    .expect("turned away");
    assert_eq!(error.status(), Some(400));
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert!(!error.cools());
    assert!(error.message().contains("not a quota"));
    let calls = host.http().calls();
    let sent: Value = hofund_plugin_sdk::json::from_slice(calls[1].body()).expect("json");
    let text = sent["request"]["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .expect("system text");
    assert!(text.contains('\u{200b}'));
    assert!(
        !text
            .to_ascii_lowercase()
            .contains("built on anthropic's claude agent sdk")
    );
}

#[test]
fn r49c_real_quota_429_stays_quota() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-1"),
    );
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-1", None));
    host.http().enqueue_response(
        429,
        &[("content-type", "application/json")],
        br#"{"error":{"code":429,"message":"You have exhausted your capacity on this model. Your quota will reset after 12s.","status":"RESOURCE_EXHAUSTED"}}"#
            .to_vec(),
    );
    let request: hofund_plugin_api::Request = hofund_plugin_sdk::json::from_value(json!({
        "model": "gemini-3-flash",
        "messages": [{"role":"user","content":[{"type":"text","text":"hello"}]}]
    }))
    .expect("request");
    let error = block_on(
        sub.upstream(&id, host.context())
            .expect("upstream")
            .execute(request, ProtocolId::GENERATIVE_LANGUAGE),
    )
    .err()
    .expect("quota");
    assert_eq!(error.status(), Some(429));
    assert_eq!(error.kind(), PluginErrorKind::Quota);
    assert!(error.cools());
}

#[test]
fn r49c_a_401_refreshes_once_and_resends() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(
        &host,
        "a-0000000000000001",
        "u@example.test",
        signed_in_blob("proj-1"),
    );
    let sub = subscription(&host);
    host.http().enqueue(token_reply("acc-1", None));
    host.http().enqueue_response(401, &[], b"{}".to_vec());
    host.http().enqueue(token_reply("acc-2", None));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let request: hofund_plugin_api::Request = hofund_plugin_sdk::json::from_value(json!({
        "model": "gemini-3-flash",
        "messages": [{"role":"user","content":[{"type":"text","text":"hello"}]}]
    }))
    .expect("request");
    block_on(
        sub.upstream(&id, host.context())
            .expect("upstream")
            .execute(request, ProtocolId::GENERATIVE_LANGUAGE),
    )
    .expect("retried");
    assert_eq!(posts_to(&host, TOKEN_URL).len(), 2);
    assert_eq!(
        posts_to(&host, &method_url(DAILY, "generateContent")).len(),
        2
    );
}

#[test]
fn r49c_an_empty_account_pool_needs_sign_in_without_http() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let account = AccountId::from_static("a-0000000000000001");
    let error = block_on(sub.list_models(&account, host.context())).expect_err("no accounts");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert!(host.http().calls().is_empty());
}

#[test]
fn r49c_contribution_and_config_follow_the_subscription_contract() {
    let contribution = crate::contribute();
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription");
    };
    assert_eq!(factory.spec().plugin().as_str(), "antigravity");
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let missing = factory
        .validate_and_build(&RawTable::new(), host.context())
        .err()
        .expect("client required");
    assert!(
        missing.message().contains("not configured"),
        "{}",
        missing.message()
    );
    let mut raw = RawTable::new();
    configured(&mut raw);
    factory
        .validate_and_build(&raw, host.context())
        .expect("configured");
    let typed = factory_with_endpoints(endpoints());
    let mut wrong = RawTable::new();
    wrong.insert("project", RawValue::Boolean(true));
    assert_eq!(typed.validate(&wrong).expect_err("wrong").path(), "project");
    let mut unknown = RawTable::new();
    unknown.insert("base", RawValue::String("https://example.test".to_owned()));
    assert_eq!(
        typed.validate(&unknown).expect_err("unknown").path(),
        "base"
    );
    assert_eq!(
        factory.config_schema().field_names(),
        ["project", "client_id", "client_secret"],
    );
    assert_eq!(
        Endpoints::default().load_code_assist,
        "https://cloudcode-pa.googleapis.com"
    );
    assert_eq!(
        Endpoints::default().code_assist,
        "https://daily-cloudcode-pa.googleapis.com"
    );
    assert_eq!(
        Endpoints::default().token,
        "https://oauth2.googleapis.com/token"
    );
}
