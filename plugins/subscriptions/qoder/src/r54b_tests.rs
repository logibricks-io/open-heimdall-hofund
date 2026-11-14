//! R-54b reference wire tests against the recording host.

use std::sync::Arc;
use std::time::SystemTime;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString,
    SignInChallenge, SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};
use hofund_plugin_sdk::{cosy as crypto, digest};

use crate::creds::{self, Credential, Site, Stored};
use crate::{Endpoints, cosy, factory_with_endpoints};

fn endpoints() -> Endpoints {
    Endpoints {
        web_global: "https://qoder.test".to_owned(),
        web_cn: "https://qoder-cn.test".to_owned(),
        openapi_global: "https://openapi.qoder.test".to_owned(),
        openapi_cn: "https://openapi-cn.qoder.test".to_owned(),
        chat_global: "https://chat.qoder.test".to_owned(),
        chat_cn: "https://chat-cn.qoder.test".to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_at(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        dir.path(),
        SystemTime::UNIX_EPOCH,
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let config = factory.validate(&RawTable::new()).expect("config");
    factory
        .build(&config, host.context())
        .expect("subscription")
}

fn context(site: Option<&str>, vpc: Option<&str>) -> SignInCtx {
    let mut ctx = SignInCtx::new(PluginId::from_static("qoder"))
        .with_session("0123456789abcdef0123456789abcdef")
        .expect("session");
    if let Some(site) = site {
        ctx = ctx.with_option("site", site).expect("site");
    }
    if let Some(vpc) = vpc {
        ctx = ctx.with_option("vpc", vpc).expect("vpc");
    }
    ctx
}

fn open_url(challenge: SignInChallenge) -> hofund_plugin_sdk::url::Url {
    let SignInChallenge::OpenUrl { url } = challenge else {
        panic!("device page expected");
    };
    hofund_plugin_sdk::url::Url::parse(&url).expect("URL")
}

fn parameter(url: &hofund_plugin_sdk::url::Url, name: &str) -> String {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .expect("query parameter")
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("secret")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

#[test]
fn r54b_global_device_pkce_polls_exchanges_and_stores_both_pairs() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let challenge =
        block_on(sub.start_sign_in(&context(None, None), host.context())).expect("start");
    let page = open_url(challenge);
    assert_eq!(page.origin().ascii_serialization(), "https://qoder.test");
    assert_eq!(page.path(), "/device/selectAccounts");
    assert_eq!(parameter(&page, "challenge_method"), "S256");
    assert_eq!(
        parameter(&page, "client_id"),
        "732aef47-9cf2-46a2-95fe-4cebb5d0d1fa"
    );
    assert!(!page.query_pairs().any(|(key, _)| key == "redirect_uri"));

    host.http().enqueue_response(404, &[], Vec::new());
    host.http().enqueue(
        json::json!({"token": "device-token", "refresh_token": "device-refresh", "user_id": "uid-1"})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json::json!({"token": "job-token", "refresh_token": "job-refresh", "expires_in": 3_600_000})
            .to_string()
            .into_bytes(),
    );
    host.http()
        .enqueue(br#"{"email":"me@example.test","name":"Example"}"#);
    let id =
        block_on(sub.complete_sign_in(&context(None, None), SignInProof::Await, host.context()))
            .expect("complete");
    let blob = stored(&host, &id);
    assert_eq!(blob.text("uid"), Some("uid-1"));
    assert_eq!(
        blob.secret("access_token").as_ref().map(secret_text),
        Some("job-token")
    );
    assert_eq!(
        blob.secret("device_token").as_ref().map(secret_text),
        Some("device-token")
    );
    assert_eq!(
        blob.text("machine_id"),
        Some(parameter(&page, "machine_id").as_str())
    );
    let calls = host.http().calls();
    assert_eq!(calls.len(), 4);
    assert!(calls[0].uri().contains("/api/v1/deviceToken/poll"));
    assert!(calls[1].uri().contains("/api/v1/deviceToken/poll"));
    assert!(calls[2].uri().contains("/api/v1/me/jobToken"));
    assert!(calls[3].uri().contains("/api/v1/userinfo"));
    let poll = hofund_plugin_sdk::url::Url::parse(calls[0].uri()).expect("poll URL");
    assert_eq!(parameter(&poll, "nonce"), parameter(&page, "nonce"));
    assert_eq!(parameter(&poll, "challenge_method"), "S256");
    let verifier = parameter(&poll, "verifier");
    let challenge = parameter(&page, "challenge");
    let expected = base64url(&digest::sha256(verifier.as_bytes()));
    assert_eq!(challenge, expected);
}

#[test]
fn r54b_cn_vpc_sign_in_uses_instance_hosts_and_device_chat() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let ctx = context(Some("qoder-cn"), Some("https://acme.vpc.qoder.com.cn/"));
    let page = open_url(block_on(sub.start_sign_in(&ctx, host.context())).expect("start"));
    assert_eq!(
        page.origin().ascii_serialization(),
        "https://acme.vpc.qoder.com.cn"
    );
    assert_eq!(
        parameter(&page, "client_id"),
        "e883ade2-e6e3-4d6d-adf7-f92ceff5fdcb"
    );
    host.http().enqueue(
        json::json!({"token": "vpc-device", "refresh_token": "vpc-refresh", "expires_in": 7200})
            .to_string()
            .into_bytes(),
    );
    host.http()
        .enqueue_response(403, &[], br#"{"message":"client not allowed"}"#.to_vec());
    host.http()
        .enqueue(br#"{"id":"vpc-user","email":"vpc@example.test","name":"VPC"}"#);
    let id =
        block_on(sub.complete_sign_in(&ctx, SignInProof::Await, host.context())).expect("complete");
    let blob = stored(&host, &id);
    assert!(blob.flag("device_chat"));
    assert_eq!(blob.text("uid"), Some("vpc-user"));
    assert_eq!(blob.text("vpc"), Some("acme"));
    let calls = host.http().calls();
    assert!(calls.iter().all(|call| {
        call.uri()
            .starts_with("https://acme-openapi.vpc.qoder.com.cn/")
    }));
    assert!(
        !calls
            .iter()
            .any(|call| call.uri().contains("openapi.qoder.com.cn"))
    );
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for index in 0..chunk.len() + 1 {
            out.push(char::from(
                ALPHABET[((value >> (18 - 6 * index)) & 63) as usize],
            ));
        }
    }
    out
}

fn seed(host: &TestHost, site: Site, vpc: &str) -> AccountId {
    let credential = Credential {
        uid: "uid-1".to_owned(),
        email: "me@example.test".to_owned(),
        name: "Example".to_owned(),
        token: SecretString::from("job-token".to_owned()),
        refresh_token: SecretString::from("job-refresh".to_owned()),
        expires_at: 4_000_000_000_000,
        machine_id: "machine-id".to_owned(),
        site,
        vpc: vpc.to_owned(),
        device_chat: false,
        device_token: SecretString::from("device-token".to_owned()),
        device_refresh_token: SecretString::from("device-refresh".to_owned()),
        pat_auth: false,
        pat: SecretString::from(String::new()),
    };
    let id = creds::account_id(&credential);
    block_on(async {
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
    });
    id
}

fn envelope(chunk: Value) -> String {
    format!(
        "data: {}\n\n",
        json::json!({"statusCodeValue": 200, "body": chunk.to_string()})
    )
}

fn rich_sse() -> Vec<u8> {
    let reasoning =
        envelope(json::json!({"choices": [{"index": 0, "delta": {"reasoning_content": "think"}}]}));
    let call = envelope(
        json::json!({"choices": [{"index": 0, "delta": {"content": "Read.\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a.go\"}}</tool_call>"}}]}),
    );
    let finish =
        envelope(json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}));
    let usage = envelope(
        json::json!({"choices": [], "usage": {"prompt_tokens": 8, "completion_tokens": 3}}),
    );
    format!(
        "{reasoning}{call}{finish}{usage}data: {{\"statusCodeValue\":200,\"body\":\"[DONE]\"}}\n\n"
    )
    .into_bytes()
}

fn raw_text(completion: Completion) -> String {
    let Completion::Raw(mut body) = completion else {
        panic!("same-protocol stream expected");
    };
    block_on(async move {
        let mut out = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        String::from_utf8(out).expect("UTF-8")
    })
}

#[test]
fn r54b_selected_display_name_replaces_upstream_auto_in_unary_and_streaming_replies() {
    for streaming in [false, true] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let id = seed(&host, Site::Global, "");
        host.http()
            .enqueue(include_bytes!("fixtures/models.json").as_slice());
        let chunk = json::json!({
            "model": "auto",
            "choices": [{"index": 0, "delta": {"content": "OK"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10}
        });
        host.http().enqueue(
            format!(
                "{}data: {{\"statusCodeValue\":200,\"body\":\"[DONE]\"}}\n\n",
                envelope(chunk)
            )
            .into_bytes(),
        );
        let upstream = subscription(&host)
            .upstream(&id, host.context())
            .expect("upstream");
        let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
        let mut request = query.minimal_request();
        request.stream = streaming;
        let completion =
            block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("execute");
        if streaming {
            let output = raw_text(completion);
            let mut events = 0;
            for data in output
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter(|data| *data != "[DONE]")
            {
                let event: Value = json::from_str(data).expect("stream event");
                assert_eq!(event["model"], "Qwen3.8-Flash");
                events += 1;
            }
            assert!(events >= 2, "{output}");
        } else {
            let Completion::Unary(answer) = completion else {
                panic!("unary response expected");
            };
            assert_eq!(answer.model.as_deref(), Some("Qwen3.8-Flash"));
        }
    }
}

#[test]
fn r54b_cosy_body_signature_and_incremental_reply_match_reference() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, Site::Global, "");
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    host.http().enqueue(rich_sse());
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
    let mut request = query.minimal_request();
    request.stream = true;
    let output =
        raw_text(block_on(upstream.execute(request, ProtocolId::OPENAI_CHAT)).expect("execute"));
    assert!(output.contains("reasoning_content"), "{output}");
    assert!(output.contains("\"name\":\"read\""), "{output}");
    assert!(output.contains("tool_calls"), "{output}");
    assert!(output.contains("\"prompt_tokens\":8"), "{output}");
    assert!(output.trim_end().ends_with("data: [DONE]"));
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].uri().contains(
        "agent_chat_generation?FetchKeys=llm_model_result&AgentId=agent_common&Encode=1"
    ));
    let body = String::from_utf8(calls[1].body().to_vec()).expect("body");
    let plain = cosy::decode(&body).expect("decode");
    let body: Value = json::from_str(&plain).expect("body JSON");
    assert_eq!(body["model_config"]["key"], "qfmodel");
    assert_eq!(body["parameters"]["context_length"], 200_000);
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );

    let stored = block_on(Stored::load(host.context(), &id)).expect("stored");
    let signature = cosy::sign(
        calls[1].uri(),
        &stored.credential,
        &body_text(calls[1].body()),
        SystemTime::UNIX_EPOCH,
    )
    .expect("signature");
    let payload = signature
        .authorization
        .strip_prefix("Bearer COSY.")
        .and_then(|value| value.split('.').next())
        .expect("payload");
    let expected = crypto::md5_hex(
        format!(
            "{payload}\n{}\n{}\n{}\n/api/v2/service/pro/sse/agent_chat_generation",
            signature.key,
            signature.date,
            body_text(calls[1].body())
        )
        .as_bytes(),
    );
    assert!(signature.authorization.ends_with(&format!(".{expected}")));
}

fn body_text(body: &[u8]) -> String {
    String::from_utf8(body.to_vec()).expect("wire body")
}

#[test]
fn r54b_vpc_account_never_falls_back_to_public_chat_hosts() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, Site::China, "acme");
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    host.http().enqueue(rich_sse());
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "qfmodel").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    assert!(calls.iter().all(|call| {
        call.uri()
            .starts_with("https://acme-gateway.vpc.qoder.com.cn/")
    }));
    assert!(
        !calls
            .iter()
            .any(|call| call.uri().contains("gateway.qoder.com.cn"))
    );
}

#[test]
fn r54b_usage_rotates_only_the_device_pair_after_a_refusal() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, Site::Global, "");
    host.http().enqueue_response(401, &[], Vec::new());
    host.http().enqueue(
        json::json!({"device_token": "device-renewed", "refresh_token": "device-refresh-renewed"})
            .to_string()
            .into_bytes(),
    );
    host.http()
        .enqueue(br#"{"displayMode":"qoder","qoderUsage":{"userQuota":{"total":100,"used":40}}}"#);
    let windows = block_on(subscription(&host).quota(&id, host.context())).expect("usage");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].used(), 40);
    let calls = host.http().calls();
    assert_eq!(calls.len(), 4);
    assert!(calls[0].uri().contains("/sash/api/v2/me/usage"));
    assert!(calls[1].uri().contains("/api/v1/deviceToken/refresh"));
    assert!(calls[2].uri().contains("/sash/api/v2/me/usage"));
    let blob = stored(&host, &id);
    assert_eq!(
        blob.secret("access_token").as_ref().map(secret_text),
        Some("job-token")
    );
    assert_eq!(
        blob.secret("device_token").as_ref().map(secret_text),
        Some("device-renewed")
    );
}

#[test]
fn r54b_vpc_checkin_uses_the_instance_openapi_host() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, Site::China, "acme");
    host.http().enqueue(br#"{"campaigns":[]}"#);
    let result =
        block_on(subscription(&host).daily_checkin(&id, host.context())).expect("check-in");
    assert_eq!(
        result.outcome(),
        hofund_plugin_api::CheckinOutcome::Inactive,
        "{}",
        result.message()
    );
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .uri()
            .starts_with("https://acme-openapi.vpc.qoder.com.cn/sash/api/v1/me/campaigns")
    );
}

#[test]
fn r54b_free_models_are_excluded_from_credit_windows() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed(&host, Site::Global, "");
    host.http()
        .enqueue(br#"{"displayMode":"qoder","qoderUsage":{"userQuota":{"total":100,"used":100}}}"#);
    host.http()
        .enqueue(include_bytes!("fixtures/models.json").as_slice());
    let windows = block_on(subscription(&host).quota(&id, host.context())).expect("usage");
    assert_eq!(windows.len(), 1);
    assert!(
        windows[0]
            .excluded_models()
            .iter()
            .any(|id| id == "qfmodel")
    );
    assert!(
        windows[0]
            .excluded_models()
            .iter()
            .any(|id| id == "dfmodel")
    );
}
