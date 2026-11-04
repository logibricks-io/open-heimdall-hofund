//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, ContentPart, Message, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, Request, Role, SecretString, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential, TOKEN_FILE};
use crate::{Endpoints, KiroConfig, discover, factory_with_endpoints, kiro_stream};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("kiro"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(Endpoints::default());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn write_token(dir: &std::path::Path, access: &str, refresh: &str, exp: &str) {
    let path = dir.join(TOKEN_FILE);
    std::fs::write(
        path,
        format!(
            r#"{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":"{exp}","region":"us-east-1","profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/P","email":"k@example.test"}}"#
        ),
    )
    .expect("write");
}

fn seed(
    host: &TestHost,
    home: &std::path::Path,
    access: &str,
    refresh: &str,
    exp: &str,
    label: &str,
) -> AccountId {
    let expires_at = hofund_plugin_sdk::time::parse_rfc3339(exp)
        .or_else(|| Some(UNIX_EPOCH + Duration::from_secs(1)));
    let credential = Credential {
        access: SecretString::from(access.to_owned()),
        refresh: SecretString::from(refresh.to_owned()),
        expires_at,
        region: "us-east-1".to_owned(),
        profile_arn: "arn:aws:codewhisperer:us-east-1:1:profile/P".to_owned(),
        label: label.to_owned(),
        method: "social".to_owned(),
        provider: String::new(),
        client_id: String::new(),
        client_secret: SecretString::from(String::new()),
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
    };
    let id = creds::account_id_for_home(home, &credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), label.to_owned())
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2b_21_discover_import_fixture_token() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("cache");
    std::fs::create_dir_all(&home).expect("dir");
    write_token(
        &home,
        "kiro-access-1",
        "kiro-refresh-1",
        "2099-01-01T00:00:00Z",
    );
    let rows = discover::list(&home).expect("list");
    assert_eq!(rows.len(), 1);
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    assert!(
        block_on(host.context().accounts().secret(&id))
            .expect("read")
            .is_some()
    );
}

#[test]
fn a2b_22_refresh_on_expiry_posts_to_refresh_url() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let endpoints = Endpoints::default();
    let sub = factory_with_endpoints(endpoints.clone())
        .build(&KiroConfig, host.context())
        .expect("build");
    let home = dir.path().join("seed-home");
    let id = seed(
        &host,
        &home,
        "old-access",
        "kiro-refresh-1",
        "1970-01-01T00:00:01Z",
        "k@example.test",
    );
    host.http().enqueue(
        br#"{"accessToken":"new-access","refreshToken":"new-refresh","expiresIn":3600,"profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/P"}"#.to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    let call = calls.last().expect("refresh call");
    assert!(call.uri().contains("auth.desktop.kiro.dev/refreshToken"));
    let body = std::str::from_utf8(call.body()).expect("utf8");
    assert!(body.contains("kiro-refresh-1"));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        parsed.secret("access_token").as_ref().map(secret_text),
        Some("new-access")
    );
    assert_eq!(
        parsed.secret("refresh_token").as_ref().map(secret_text),
        Some("new-refresh")
    );
}

#[test]
fn a2b_23_model_catalog_lists_anthropic_models() {
    let list = crate::models::catalog().expect("catalog");
    assert!(
        list.models()
            .iter()
            .any(|model| model.id() == "claude-sonnet-4.5")
    );
}

#[test]
fn a2b_24_upstream_round_trip_with_recording_http() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    host.http().enqueue(br#"{"content":"ok"}"#.to_vec());
    let home = dir.path().join("seed-home");
    let id = seed(
        &host,
        &home,
        "kiro-access-up",
        "kiro-refresh",
        "2099-01-01T00:00:00Z",
        "k@example.test",
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let request = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude-sonnet-4.5")
        .expect("query")
        .minimal_request();
    block_on(upstream.execute(request, ProtocolId::ANTHROPIC_MESSAGES)).expect("execute");
    let calls = host.http().calls();
    let call = calls.last().expect("call");
    assert!(call.uri().contains("runtime.us-east-1.kiro.dev"));
    assert!(call.uri().contains("generateAssistantResponse"));
    let body = std::str::from_utf8(call.body()).expect("utf8");
    assert!(body.contains("profileArn"));
    assert!(body.contains("claude-sonnet-4.5"));
    assert!(
        call.sensitive_headers()
            .contains(&"authorization".to_owned())
    );
}

#[test]
fn a2b_25_usage_parsing() {
    let body = br#"{"usageBreakdownList":[{"displayName":"Credits","currentUsageWithPrecision":40,"usageLimitWithPrecision":100}]}"#;
    let windows = crate::quota::windows(body).expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].name(), "Credits");
    assert_eq!(windows[0].used(), 40);
    assert_eq!(windows[0].limit(), Some(100));
    let body2 = br#"{"usageBreakdownList":[{"displayName":"Credits","currentUsageWithPrecision":50,"usageLimitWithPrecision":100}]}"#;
    let windows2 = crate::quota::windows(body2).expect("windows2");
    assert_eq!(windows2[0].used(), 50);
}

#[test]
fn a2b_26_secrets_not_in_debug_or_errors() {
    let token = "kiro-secret-access-token";
    let credential = Credential {
        access: SecretString::from(token.to_owned()),
        refresh: SecretString::from("r".to_owned()),
        expires_at: None,
        region: "us-east-1".to_owned(),
        profile_arn: String::new(),
        label: "k@example.test".to_owned(),
        method: "social".to_owned(),
        provider: String::new(),
        client_id: String::new(),
        client_secret: SecretString::from(String::new()),
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
    };
    assert!(!format!("{credential:?}").contains(token));
    let error = creds::read_token_file(std::path::Path::new("/no/such/kiro-auth-token.json"));
    assert!(!error.expect_err("missing").to_string().contains(token));
}

#[test]
fn a2b_27_agent_sign_in_writes_token_file() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let home = dir.path().join("seed-home");
    let id = seed(
        &host,
        &home,
        "agent-access",
        "agent-refresh",
        "2099-01-01T00:00:00Z",
        "k@example.test",
    );
    let home = dir.path().join("sso-cache");
    block_on(sub.write_agent_sign_in(&id, &home, host.context())).expect("write");
    let body = std::fs::read_to_string(home.join(TOKEN_FILE)).expect("read");
    assert!(body.contains("agent-access"));
    assert!(body.contains("k@example.test"));
}

#[test]
fn a2b_35_reimport_replaces_stored_access_token() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("cache");
    std::fs::create_dir_all(&home).expect("dir");
    write_token(
        &home,
        "kiro-access-v1",
        "kiro-refresh-v1",
        "2099-01-01T00:00:00Z",
    );
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    write_token(
        &home,
        "kiro-access-v2",
        "kiro-refresh-v2",
        "2099-01-01T00:00:00Z",
    );
    block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("reimport");
    assert_eq!(
        block_on(host.context().accounts().secret(&id))
            .expect("read")
            .and_then(|secret| {
                SecretObject::parse(&secret)
                    .ok()?
                    .secret("access_token")
                    .map(|token| secret_text(&token).to_owned())
            })
            .as_deref(),
        Some("kiro-access-v2")
    );
}

#[test]
fn a2b_30_agent_home_resolution_matches_daemon_rule() {
    let decl = creds::AGENT_SIGN_IN;
    assert_eq!(decl.home_env(), "KIRO_IDE_CACHE");
    let override_home =
        creds::resolve_agent_home_from(Some("/tmp/kiro-cache-override"), Some("/ignored"), &decl)
            .expect("override");
    assert_eq!(
        override_home,
        std::path::PathBuf::from("/tmp/kiro-cache-override")
    );
    let default_home =
        creds::resolve_agent_home_from(None, Some("/tmp/kiro-test-home"), &decl).expect("default");
    assert_eq!(
        default_home,
        std::path::PathBuf::from("/tmp/kiro-test-home/.aws/sso/cache")
    );
}

#[test]
fn a2b_31_idc_refresh_posts_to_oidc_token() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let endpoints = Endpoints::default();
    let sub = factory_with_endpoints(endpoints.clone())
        .build(&KiroConfig, host.context())
        .expect("build");
    let home = dir.path().join("seed-home");
    let credential = Credential {
        access: SecretString::from("old-access".to_owned()),
        refresh: SecretString::from("idc-refresh".to_owned()),
        expires_at: Some(UNIX_EPOCH + Duration::from_secs(1)),
        region: "us-east-1".to_owned(),
        profile_arn: "arn:aws:codewhisperer:us-east-1:1:profile/P".to_owned(),
        label: "Kiro".to_owned(),
        method: "idc".to_owned(),
        provider: "BuilderId".to_owned(),
        client_id: "idc-client".to_owned(),
        client_secret: SecretString::from("idc-secret".to_owned()),
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
    };
    let id = creds::account_id_for_home(&home, &credential);
    block_on(async {
        host.context()
            .accounts()
            .upsert(
                host.context()
                    .accounts()
                    .account(id.clone(), "Kiro".to_owned())
                    .expect("account"),
                creds::secret_blob(&credential).to_secret(),
            )
            .await
            .expect("upsert");
    });
    host.http().enqueue(
        br#"{"accessToken":"new-idc","refreshToken":"new-idc-r","expiresIn":3600}"#.to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    let call = calls.last().expect("call");
    assert!(call.uri().contains("oidc.us-east-1.amazonaws.com/token"));
}

#[test]
fn a2b_41_idc_token_file_parsing_and_refresh() {
    let dir = tempfile::tempdir().expect("dir");
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).expect("dir");
    std::fs::write(
        cache.join("deadbeef.json"),
        r#"{"clientId":"reg-client","clientSecret":"reg-secret"}"#,
    )
    .expect("write");
    std::fs::write(
        cache.join(TOKEN_FILE),
        r#"{"accessToken":"old-access","refreshToken":"idc-refresh","authMethod":"IdC","clientIdHash":"deadbeef","region":"us-east-1","expiresAt":"1970-01-01T00:00:00Z"}"#,
    )
    .expect("write");
    let credential = creds::read_token_file(&cache.join(TOKEN_FILE)).expect("parse");
    assert_eq!(credential.method, "idc");
    assert_eq!(credential.client_id, "reg-client");
    let host = host(&dir);
    let sub = subscription(&host);
    let home = dir.path().join("seed-home");
    let mut stored = credential;
    stored.ide_token_path = cache.join(TOKEN_FILE).to_string_lossy().into_owned();
    let id = creds::account_id_for_home(&home, &stored);
    block_on(async {
        host.context()
            .accounts()
            .upsert(
                host.context()
                    .accounts()
                    .account(id.clone(), "Kiro".to_owned())
                    .expect("account"),
                creds::secret_blob(&stored).to_secret(),
            )
            .await
            .expect("upsert");
    });
    host.http().enqueue(
        br#"{"accessToken":"new-idc","refreshToken":"new-idc-r","expiresIn":3600}"#.to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let call = host.http().calls().last().expect("call");
    assert!(call.uri().contains("oidc.us-east-1.amazonaws.com/token"));
    let body = String::from_utf8_lossy(call.body());
    assert!(body.contains("reg-client"));
    let on_disk = std::fs::read_to_string(cache.join(TOKEN_FILE)).expect("read");
    assert!(on_disk.contains("new-idc"));
}

#[test]
fn a2b_32_usage_url_includes_profile_and_eu_region() {
    let endpoints = Endpoints::default();
    let url = endpoints.usage_url(
        "arn:aws:codewhisperer:eu-central-1:1:profile/P",
        "eu-west-1",
    );
    assert!(url.contains("Get-Usage-Limits"));
    assert!(url.contains("profileArn="));
    assert!(url.contains("management.eu-central-1.kiro.dev"));
}

#[test]
fn a2b_33_event_stream_joins_multiple_frames() {
    let frame1 = br#"{"content":"hel"}"#;
    let frame2 = br#"{"content":"lo"}"#;
    let mut body = Vec::new();
    for payload in [frame1.as_slice(), frame2.as_slice()] {
        let headers = b"\x0b:event-type\x07\x00\x16assistantResponseEvent";
        let total = 12 + headers.len() + payload.len() + 4;
        body.extend_from_slice(&(total as u32).to_be_bytes());
        body.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(headers);
        body.extend_from_slice(payload);
        body.extend_from_slice(&[0, 0, 0, 0]);
    }
    assert_eq!(kiro_stream::assistant_text(&body).expect("text"), "hello");
}

#[test]
fn a2b_34_history_and_system_prompt_are_encoded() {
    let mut sys = Message::new(Role::System);
    sys.content.push(ContentPart::text("sys-rule"));
    let mut user1 = Message::new(Role::User);
    user1.content.push(ContentPart::text("first"));
    let mut assistant = Message::new(Role::Assistant);
    assistant.content.push(ContentPart::text("mid"));
    let mut user2 = Message::new(Role::User);
    user2.content.push(ContentPart::text("last"));
    let request =
        Request::new("claude-sonnet-4.5").with_messages(vec![sys, user1, assistant, user2]);
    let credential = Credential {
        access: SecretString::from("a".to_owned()),
        refresh: SecretString::from("r".to_owned()),
        expires_at: None,
        region: "us-east-1".to_owned(),
        profile_arn: "arn:aws:codewhisperer:us-east-1:1:profile/P".to_owned(),
        label: "Kiro".to_owned(),
        method: "social".to_owned(),
        provider: String::new(),
        client_id: String::new(),
        client_secret: SecretString::from(String::new()),
        client_id_hash: String::new(),
        token_url: String::new(),
        ide_token_path: String::new(),
    };
    let encoded = crate::kiro_body::encode(&request, &credential).expect("encode");
    let body = std::str::from_utf8(&encoded).expect("utf8");
    assert!(body.contains("history"));
    assert!(body.contains("assistantResponseMessage"));
    assert!(body.contains("sys-rule"));
    assert!(body.contains("last"));
    let mut streaming = request.clone();
    streaming.stream = true;
    assert!(crate::kiro_body::reject_unsupported(&streaming).is_err());
}
