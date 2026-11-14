use std::time::Duration;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, ProtocolId, RawTable, SecretString,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{Endpoints, factory_with_endpoints, project};

const TOKEN_URL: &str = "https://oauth.test/token";
const USERINFO_URL: &str = "https://oauth.test/userinfo";
const CODE_ASSIST: &str = "https://code-assist.test";

fn endpoints() -> Endpoints {
    Endpoints {
        authorize: "https://accounts.test/o/oauth2/v2/auth".to_owned(),
        token: TOKEN_URL.to_owned(),
        userinfo: USERINFO_URL.to_owned(),
        code_assist: CODE_ASSIST.to_owned(),
        onboard_poll: Duration::from_millis(1),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("gemini"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> std::sync::Arc<dyn hofund_plugin_api::Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let config = factory.validate(&RawTable::new()).expect("config");
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
    let id = account_id_from(&["google", &email.to_ascii_lowercase()]);
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
fn r49d_imports_manager_and_cliproxy_exports_with_project_fallback() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-a"));
    host.http().enqueue(userinfo("a@example.test"));
    host.http().enqueue(
        br#"{"currentTier":{"id":"standard-tier","name":"Gemini Code Assist"},"cloudaicompanionProject":"found-a"}"#
            .to_vec(),
    );
    host.http().enqueue(token_reply("access-b"));
    host.http().enqueue_response(401, &[], Vec::new());
    host.http().enqueue_response(503, &[], Vec::new());
    let text = r#"[
        {"email":"a@example.test","token":{"refresh_token":"refresh-a","project_id":"file-a"}},
        {"type":"gemini","email":"b@example.test","refresh_token":"refresh-b","project_id":"file-b"}
    ]"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("import");
    assert_eq!(
        ids,
        [
            account_id_from(&["google", "a@example.test"]),
            account_id_from(&["google", "b@example.test"]),
        ]
    );
    assert_eq!(
        stored(&host, &ids[0]).text(project::PROJECT),
        Some("found-a")
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
    assert!(calls[0].uri().starts_with(TOKEN_URL));
    assert!(calls[1].uri().starts_with(USERINFO_URL));
    assert!(calls[3].uri().starts_with(TOKEN_URL));
    assert!(calls[4].uri().starts_with(USERINFO_URL));
}

#[test]
fn r49d_existing_refresh_is_not_refreshed_and_a_new_one_replaces_it() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(&host, "u@example.test", "refresh-old");
    let text = r#"{"email":"u@example.test","refresh_token":"refresh-old"}"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("same import");
    assert_eq!(ids, vec![id.clone()]);
    assert!(host.http().calls().is_empty());

    host.http().enqueue(token_reply("access-new"));
    host.http().enqueue(userinfo("u@example.test"));
    host.http().enqueue_response(503, &[], Vec::new());
    let text = r#"{"email":"u@example.test","refresh_token":"refresh-new"}"#;
    let ids = block_on(sub.import_accounts(text, host.context())).expect("replacement");
    assert_eq!(ids, vec![id.clone()]);
    assert_eq!(
        stored(&host, &id)
            .secret("refresh_token")
            .map(|token| hofund_plugin_sdk::oauth::secret_text(&token).to_owned())
            .as_deref(),
        Some("refresh-new")
    );
}

#[test]
fn r49d_wrong_app_and_bad_entries_fail_before_http_without_token_text() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let error = block_on(sub.import_accounts(
        r#"{"type":"antigravity","refresh_token":"secret-refresh","email":"a@example.test"}"#,
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
fn r49d_recorded_tool_image_and_signature_history_matches_code_assist() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, "u@example.test", "refresh-old");
    let sub = subscription(&host);
    host.http().enqueue(token_reply("access-1"));
    host.http().enqueue(br#"{"flags":[]}"#.to_vec());
    host.http().enqueue(UNARY_REPLY.as_bytes().to_vec());
    let request: hofund_plugin_api::Request = hofund_plugin_sdk::json::from_value(
        hofund_plugin_sdk::json::json!({
            "model": "gemini-3.8-flash",
            "messages": [
                {"role":"user","content":[
                    {"type":"text","text":"look"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"SU5QVVQ="}},
                    {"type":"image","source":{"type":"url","url":"https://example.test/input.png"},"extensions":{"mimeType":"image/png"}}
                ]},
                {"role":"assistant","content":[
                    {"type":"thinking","text":"hidden","signature":"thinking-sig"},
                    {"type":"text","text":"signed text","extensions":{"thoughtSignature":"text-sig"}},
                    {"type":"tool_call","id":"call-a","name":"read","arguments":{"form":"json","value":{"path":"a"}},"extensions":{"thoughtSignature":"call-sig"}},
                    {"type":"tool_call","id":"call-b","name":"write","arguments":{"form":"json","value":{}}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","id":"call-a","content":[
                        {"type":"text","text":"first-result"},
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"RklSU1Q="}},
                        {"type":"image","source":{"type":"url","url":"https://example.test/result.png"},"extensions":{"mimeType":"image/png"}}
                    ],"extensions":{"name":"read"}},
                    {"type":"tool_result","id":"call-a","content":[
                        {"type":"text","text":"duplicate-result"},
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"RFVQ"}}
                    ],"extensions":{"name":"read"}},
                    {"type":"text","text":"continue"}
                ]}
            ],
            "tools": [{"name":"read","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}]
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
    let sent: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(calls[2].body()).expect("json");
    let contents = sent["request"]["contents"].as_array().expect("contents");
    assert_eq!(contents.len(), 4);
    assert_eq!(contents[0]["parts"][0]["text"], "look");
    assert_eq!(contents[0]["parts"][1]["inlineData"]["data"], "SU5QVVQ=");
    assert_eq!(
        contents[0]["parts"][2]["fileData"]["fileUri"],
        "https://example.test/input.png"
    );
    let model = contents[1]["parts"].as_array().expect("model");
    assert_eq!(model.len(), 3, "thinking text is not replayed");
    assert_eq!(model[0]["thoughtSignature"], "text-sig");
    assert_eq!(model[1]["thoughtSignature"], "call-sig");
    assert!(model[2].get("thoughtSignature").is_none());
    let answers = contents[2]["parts"].as_array().expect("answers");
    assert!(
        answers[0]["functionResponse"]["response"]["output"]
            .as_str()
            .is_some_and(|text| text.contains("turn was interrupted"))
    );
    assert_eq!(
        answers[1]["functionResponse"]["response"]["output"],
        "first-result"
    );
    assert_eq!(answers[2]["inlineData"]["data"], "RklSU1Q=");
    assert_eq!(
        answers[3]["fileData"]["fileUri"],
        "https://example.test/result.png"
    );
    assert!(!sent.to_string().contains("duplicate-result"));
    assert!(!sent.to_string().contains("RFVQ"));
    assert!(!sent.to_string().contains("thinking-sig"));
    assert_eq!(contents[3]["parts"][0]["text"], "continue");
    assert!(
        sent["request"]["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"].is_object()
    );
}
