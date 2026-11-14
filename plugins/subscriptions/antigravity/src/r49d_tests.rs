use std::time::{Duration, SystemTime};

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, ProtocolId, RawTable, RawValue, SecretString,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{Endpoints, client, factory_with_endpoints, project, version};

const TOKEN_URL: &str = "https://oauth.test/token";
const USERINFO_URL: &str = "https://oauth.test/userinfo";
const LOAD: &str = "https://code-assist-prod.test";
const DAILY: &str = "https://code-assist-daily.test";
const MANIFEST: &str = "https://antigravity-version.test/manifest/latest-arm64-mac.yml";
const TEST_CLIENT_ID: &str = "hofund-test-antigravity-client";
const TEST_CLIENT_SECRET: &str = "hofund-test-antigravity-secret";

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
        token: TOKEN_URL.to_owned(),
        userinfo: USERINFO_URL.to_owned(),
        load_code_assist: LOAD.to_owned(),
        code_assist: DAILY.to_owned(),
        version_manifest: MANIFEST.to_owned(),
        onboard_poll: Duration::from_millis(1),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    version::warm_cache_for_test(version::FALLBACK_VERSION, SystemTime::UNIX_EPOCH);
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("antigravity"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> std::sync::Arc<dyn hofund_plugin_api::Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let mut raw = RawTable::new();
    raw.insert(client::ID_KEY, RawValue::String(TEST_CLIENT_ID.to_owned()));
    raw.insert(
        client::SECRET_KEY,
        RawValue::String(TEST_CLIENT_SECRET.to_owned()),
    );
    let config = factory.validate(&raw).expect("config");
    factory.build(&config, host.context()).expect("build")
}

fn token_reply(access: &str) -> Vec<u8> {
    format!(r#"{{"access_token":"{access}","token_type":"Bearer","expires_in":3600}}"#).into_bytes()
}

fn userinfo(email: &str) -> Vec<u8> {
    format!(r#"{{"email":"{email}"}}"#).into_bytes()
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn seed(host: &TestHost, email: &str, refresh: &str) -> AccountId {
    let id = account_id_from(&["antigravity", &email.to_ascii_lowercase()]);
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", &SecretString::from(refresh.to_owned()))
        .with_text("email", email)
        .with_text(project::PROJECT, "stored-project");
    let account = host
        .context()
        .accounts()
        .account(id.clone(), email)
        .expect("account");
    block_on(host.context().accounts().upsert(account, blob.to_secret())).expect("seed");
    id
}

#[test]
fn r49d_imports_refresh_exports_with_the_antigravity_client() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-a"));
    host.http().enqueue(userinfo("a@example.test"));
    host.http().enqueue(
        br#"{"currentTier":{"id":"standard-tier","name":"Antigravity"},"paidTier":{"id":"g1-pro-tier","name":"Google AI Pro"},"cloudaicompanionProject":"found-a"}"#
            .to_vec(),
    );
    host.http().enqueue(token_reply("access-b"));
    host.http().enqueue_response(401, &[], Vec::new());
    host.http().enqueue_response(503, &[], Vec::new());
    let text = r#"{"accounts":[
        {"email":"a@example.test","token":{"refresh_token":"refresh-a","project_id":"file-a"}},
        {"type":"antigravity","email":"b@example.test","refresh_token":"refresh-b","project_id":"file-b"}
    ]}"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("import");
    assert_eq!(
        ids,
        [
            account_id_from(&["antigravity", "a@example.test"]),
            account_id_from(&["antigravity", "b@example.test"]),
        ]
    );
    assert_ne!(ids[0], account_id_from(&["google", "a@example.test"]));
    assert_eq!(
        stored(&host, &ids[0]).text(project::PROJECT),
        Some("found-a")
    );
    assert_eq!(
        stored(&host, &ids[0]).text(project::PAID),
        Some("g1-pro-tier")
    );
    assert_eq!(
        stored(&host, &ids[1]).text(project::PROJECT),
        Some("file-b")
    );
    assert_eq!(stored(&host, &ids[1]).text("email"), Some("b@example.test"));
    assert!(stored(&host, &ids[0]).secret("access_token").is_none());
    assert!(stored(&host, &ids[1]).secret("access_token").is_none());

    let calls = host.http().calls();
    assert_eq!(calls.len(), 6);
    for call in calls
        .iter()
        .filter(|call| call.uri().starts_with(TOKEN_URL))
    {
        let body = String::from_utf8_lossy(call.body());
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("client_id=hofund-test-antigravity-client"));
        assert!(body.contains("client_secret=hofund-test-antigravity-secret"));
    }
}

#[test]
fn r49d_existing_refresh_is_not_refreshed() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, "u@example.test", "refresh-old");
    let text = r#"{"email":"u@example.test","refresh_token":"refresh-old"}"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("same import");
    assert_eq!(ids, vec![id]);
    assert!(host.http().calls().is_empty());
}

#[test]
fn r49d_wrong_app_and_duplicates_fail_before_http_without_token_text() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let error = block_on(sub.import_accounts(
        r#"{"type":"gemini","refresh_token":"secret-refresh","email":"a@example.test"}"#,
        host.context(),
    ))
    .expect_err("wrong app");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(!error.message().contains("secret-refresh"));
    assert!(host.http().calls().is_empty());

    let error = block_on(sub.import_accounts(
        r#"[{"email":"a@example.test","refresh_token":"duplicate"},{"email":"A@example.test","refresh_token":"other"}]"#,
        host.context(),
    ))
    .expect_err("duplicate");
    assert!(error.message().contains("in the file twice"));
    assert!(!error.message().contains("duplicate"));
    assert!(!error.message().contains("other"));
    assert!(host.http().calls().is_empty());
}

const UNARY_REPLY: &str = r#"{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4}}}"#;

#[test]
fn r49d_recorded_claude_tool_images_and_signatures_match_antigravity() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, "u@example.test", "refresh-old");
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-1"));
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let request: hofund_plugin_api::Request = hofund_plugin_sdk::json::from_value(
        hofund_plugin_sdk::json::json!({
            "model": "claude-opus-4-6-thinking",
            "messages": [
                {"role":"user","content":[{"type":"text","text":"look"}]},
                {"role":"assistant","content":[
                    {"type":"text","text":"answer","extensions":{"thoughtSignature":"foreign-text"}},
                    {"type":"tool_call","id":"toolu_01:x","name":"Read","arguments":{"form":"json","value":{"path":"a"}},"extensions":{"thoughtSignature":"foreign-call"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","id":"toolu_01:x","content":[
                        {"type":"text","text":"first-result"},
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUFB"}}
                    ],"extensions":{"name":"Read"}},
                    {"type":"tool_result","id":"toolu_02:y","is_error":true,"content":[
                        {"type":"text","text":"second-result"},
                        {"type":"image","source":{"type":"url","url":"https://example.test/b.png"},"extensions":{"mimeType":"image/png"}}
                    ],"extensions":{"name":"Read"}},
                    {"type":"text","text":"which?"}
                ]}
            ],
            "tools": [{"name":"Read","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false}}]
        }),
    )
    .expect("request");
    block_on(
        sub.upstream(&id, host.context())
            .expect("upstream")
            .execute(request, ProtocolId::GENERATIVE_LANGUAGE),
    )
    .expect("execute");
    let calls = host.http().calls();
    let call = calls
        .iter()
        .find(|call| call.uri().contains("generateContent"))
        .expect("generate");
    let sent: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(call.body()).expect("json");
    let contents = sent["request"]["contents"].as_array().expect("contents");
    assert_eq!(contents.len(), 3);
    let model = contents[1]["parts"].as_array().expect("model");
    assert!(model[0].get("thoughtSignature").is_none());
    assert_eq!(model[1]["functionCall"]["id"], "toolu_01_x");
    assert_eq!(
        model[1]["thoughtSignature"],
        "skip_thought_signature_validator"
    );
    let answers = contents[2]["parts"].as_array().expect("answers");
    assert_eq!(
        answers[0]["functionResponse"]["response"]["result"],
        "first-result"
    );
    assert_eq!(
        answers[1]["functionResponse"]["response"]["result"],
        "second-result"
    );
    assert_eq!(answers[1]["functionResponse"]["id"], "toolu_02_y");
    assert!(
        answers[2]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Image returned"))
    );
    assert_eq!(answers[3]["inlineData"]["data"], "QUFB");
    assert!(
        answers[4]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Image returned"))
    );
    assert_eq!(
        answers[5]["fileData"]["fileUri"],
        "https://example.test/b.png"
    );
    assert_eq!(answers[6]["text"], "which?");
    assert!(!sent.to_string().contains("foreign-call"));
    assert!(!sent.to_string().contains("foreign-text"));
    assert_eq!(
        sent["request"]["toolConfig"]["functionCallingConfig"]["mode"],
        "VALIDATED"
    );
    assert!(
        sent["request"]["tools"][0]["functionDeclarations"][0]["parameters"]
            .get("additionalProperties")
            .is_none()
    );
}
