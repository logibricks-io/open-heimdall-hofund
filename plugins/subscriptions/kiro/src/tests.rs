//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use hofund_kit_subscription::{Http1Response, Http1Server, TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, ContentPart, Extensions, ImagePart, ImageSource, Message, PluginId,
    PluginKind, ProbeQuery, ProtocolId, RawTable, Request, Role, SecretString, StreamEvent,
    Subscription, SubscriptionFactory, Tool, ToolArguments, ToolCallPart, ToolResultPart,
};
use hofund_plugin_sdk::json::{self, Value, json};
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
        db_key: String::new(),
        cli_db_path: String::new(),
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
    let rows = discover::list_at(&home, None).expect("list");
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
        db_key: String::new(),
        cli_db_path: String::new(),
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
fn a2b_36_reimport_replaces_stored_access_token() {
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
    let factory = factory_with_endpoints(Endpoints::default());
    let decl = factory
        .spec()
        .agent_sign_in()
        .expect("kiro declares agent sign-in");
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
        db_key: String::new(),
        cli_db_path: String::new(),
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
        r#"{"accessToken":"old-access","refreshToken":"idc-refresh","authMethod":"IdC","clientIdHash":"deadbeef","region":"us-east-1","expiresAt":"1970-01-01T00:00:01Z"}"#,
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
    let calls = host.http().calls();
    let call = calls.last().expect("call");
    assert!(call.uri().contains("oidc.us-east-1.amazonaws.com/token"));
    let body = String::from_utf8_lossy(call.body());
    assert!(body.contains("reg-client"));
    let on_disk = std::fs::read_to_string(cache.join(TOKEN_FILE)).expect("read");
    assert!(on_disk.contains("new-idc"));
}

#[test]
fn a2b_43_refresh_adopts_ide_account_without_overwriting_token_file() {
    let dir = tempfile::tempdir().expect("dir");
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).expect("dir");
    write_token(
        &cache,
        "hofund-access-a",
        "hofund-refresh-a",
        "1970-01-01T00:00:01Z",
    );
    let host = host(&dir);
    let sub = subscription(&host);
    let home = dir.path().join("seed-home");
    let token_path = cache.join(TOKEN_FILE);
    let mut credential = creds::read_token_file(&token_path).expect("read");
    credential.ide_token_path = token_path.to_string_lossy().into_owned();
    let id = creds::account_id_for_home(&home, &credential);
    block_on(async {
        host.context()
            .accounts()
            .upsert(
                host.context()
                    .accounts()
                    .account(id.clone(), "k@example.test".to_owned())
                    .expect("account"),
                creds::secret_blob(&credential).to_secret(),
            )
            .await
            .expect("upsert");
    });
    write_token(
        &cache,
        "ide-access-b",
        "ide-refresh-b",
        "2099-01-01T00:00:00Z",
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert!(host.http().calls().is_empty());
    let on_disk = std::fs::read_to_string(&token_path).expect("read");
    assert!(on_disk.contains("ide-access-b"));
    assert!(on_disk.contains("ide-refresh-b"));
    assert!(!on_disk.contains("hofund-access-a"));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let parsed = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        parsed.secret("access_token").as_ref().map(secret_text),
        Some("ide-access-b")
    );
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
        db_key: String::new(),
        cli_db_path: String::new(),
    };
    let encoded = crate::kiro_body::encode(&request, &credential).expect("encode");
    let body = std::str::from_utf8(&encoded).expect("utf8");
    assert!(body.contains("history"));
    assert!(body.contains("assistantResponseMessage"));
    assert!(body.contains("sys-rule"));
    assert!(body.contains("last"));
    let mut streaming = request.clone();
    streaming.stream = true;
    crate::kiro_body::reject_unsupported(&streaming).expect("streaming is supported");
}

#[test]
fn a2b_44_kiro_region_prefers_profile_arn_then_eu_fallback() {
    assert_eq!(
        creds::kiro_region("arn:aws:codewhisperer:us-west-2:1:profile/P", "eu-west-1"),
        "us-west-2"
    );
    assert_eq!(creds::kiro_region("", "eu-west-1"), "eu-central-1");
    assert_eq!(creds::kiro_region("", "ap-south-1"), "us-east-1");
}

fn test_credential() -> Credential {
    Credential {
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
        db_key: String::new(),
        cli_db_path: String::new(),
    }
}

fn encoded(request: &Request) -> Value {
    let bytes = crate::kiro_body::encode(request, &test_credential()).expect("encode");
    json::from_slice(&bytes).expect("request JSON")
}

fn message(role: Role, content: Vec<ContentPart>) -> Message {
    Message {
        role,
        content,
        extensions: Extensions::new(),
    }
}

fn call(id: &str, name: &str, input: Value) -> ContentPart {
    ContentPart::ToolCall(ToolCallPart {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments: ToolArguments::Json { value: input },
        extensions: Extensions::new(),
    })
}

fn result(id: &str, content: Vec<ContentPart>) -> ContentPart {
    ContentPart::ToolResult(ToolResultPart {
        id: id.to_owned(),
        is_error: false,
        content,
        extensions: Extensions::new(),
    })
}

fn image(media_type: &str, data: &str) -> ContentPart {
    ContentPart::Image(ImagePart {
        source: ImageSource::Base64 {
            media_type: media_type.to_owned(),
            data: data.to_owned(),
        },
        detail: None,
        extensions: Extensions::new(),
    })
}

fn tool(name: &str, input_schema: Value) -> Tool {
    Tool {
        name: name.to_owned(),
        description: Some(format!("Use {name}")),
        input_schema,
        extensions: Extensions::new(),
    }
}

fn frame(event_type: &str, payload: Value) -> Vec<u8> {
    let payload = json::to_vec(&payload).expect("payload");
    let headers = [
        b"\x0b:event-type\x07".as_slice(),
        &(event_type.len() as u16).to_be_bytes(),
        event_type.as_bytes(),
    ]
    .concat();
    let total = 12 + headers.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame
}

#[test]
fn r51_1_tool_names_ids_and_history_match_the_reference_shape() {
    const LONG: &str = "mcp__plugin_cloudflare_cloudflare-docs__search_cloudflare_documentation";
    const LONG2: &str = "mcp__plugin_cloudflare_cloudflare-docs__migrate_pages_to_workers_guide";
    let request = Request::new("claude-opus-4.5")
        .with_messages(vec![
            message(Role::User, vec![ContentPart::text("search")]),
            message(
                Role::Assistant,
                vec![call("odd id/1", LONG, json!({"q": "workers"}))],
            ),
            message(
                Role::User,
                vec![result("odd id/1", vec![ContentPart::text("found")])],
            ),
        ])
        .with_tools(vec![
            tool(LONG, json!({"type": "object", "properties": {}})),
            tool(LONG2, json!({"properties": {"q": {"type": "string"}}})),
            tool("Read", json!({})),
        ]);
    let body = encoded(&request);
    let state = &body["conversationState"];
    let tools = state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["tools"]
        .as_array()
        .expect("tools");
    let names: Vec<_> = tools
        .iter()
        .map(|tool| tool["toolSpecification"]["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names.len(), 3);
    assert_ne!(names[0], names[1]);
    assert!(names[0].starts_with("mcp__plugin_cloudflare_cloudflare-docs__"));
    assert!(names.iter().all(|name| {
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    }));
    assert_eq!(names[2], "Read");
    assert_eq!(
        tools[1]["toolSpecification"]["inputSchema"]["json"],
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    );
    assert_eq!(
        tools[2]["toolSpecification"]["inputSchema"]["json"],
        json!({"type": "object", "properties": {}})
    );
    let history_call = &state["history"][1]["assistantResponseMessage"]["toolUses"][0];
    assert_eq!(history_call["name"], names[0]);
    assert!(
        history_call["toolUseId"]
            .as_str()
            .expect("normalized id")
            .starts_with("t_")
    );
    assert_eq!(
        state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"][0]["toolUseId"],
        history_call["toolUseId"]
    );
}

#[test]
fn r51_2_split_calls_keep_real_and_missing_results() {
    let mut request = Request::new("claude-sonnet-4.5").with_messages(vec![
        message(Role::User, vec![ContentPart::text("read a and b")]),
        message(
            Role::Assistant,
            vec![call("toolu_a", "read", json!({"path": "a"}))],
        ),
        message(
            Role::Assistant,
            vec![call("toolu_b", "read", json!({"path": "b"}))],
        ),
        message(
            Role::User,
            vec![result("toolu_b", vec![ContentPart::text("B!")])],
        ),
    ]);
    let body = encoded(&request);
    let state = &body["conversationState"];
    assert_eq!(
        state["history"][1]["assistantResponseMessage"]["toolUses"]
            .as_array()
            .expect("calls")
            .len(),
        2
    );
    let results =
        state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"]
            .as_array()
            .expect("results");
    assert_eq!(results[0]["content"][0]["text"], "B!");
    assert_eq!(results[1]["toolUseId"], "toolu_a");
    assert_eq!(
        results[1]["content"][0]["text"],
        "Tool use was interrupted and did not produce a result."
    );
    request.messages[3]
        .content
        .insert(0, result("toolu_a", vec![ContentPart::text("A!")]));
    let body = encoded(&request);
    let results =
        body["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
            ["toolResults"]
            .as_array()
            .expect("results");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["content"][0]["text"], "A!");
    assert_eq!(results[1]["content"][0]["text"], "B!");
}

#[test]
fn r51_3_images_in_results_and_latest_image_rule_are_encoded() {
    const PNG: &str = "aVBORw0KGgo=";
    const JPG: &str = "/9j/4AAQSkZJRg==";
    let request = Request::new("claude-sonnet-4.5")
        .with_messages(vec![
            message(Role::User, vec![ContentPart::text("look")]),
            message(
                Role::Assistant,
                vec![call("call_view1", "view_image", json!({"path": "a.png"}))],
            ),
            message(
                Role::User,
                vec![result(
                    "call_view1",
                    vec![
                        image("image/png", PNG),
                        ContentPart::text("Screenshot"),
                        image("image/jpeg", JPG),
                    ],
                )],
            ),
        ])
        .with_tools(vec![tool(
            "view_image",
            json!({"type": "object", "properties": {}}),
        )]);
    let body = encoded(&request);
    let user = &body["conversationState"]["currentMessage"]["userInputMessage"];
    assert_eq!(user["images"][0]["format"], "png");
    assert_eq!(user["images"][0]["source"]["bytes"], PNG);
    assert_eq!(user["images"][1]["format"], "jpeg");
    assert_eq!(
        user["userInputMessageContext"]["toolResults"][0]["content"][0]["text"],
        "Screenshot"
    );

    let followed = Request::new("claude-sonnet-4.5").with_messages(vec![
        request.messages[0].clone(),
        request.messages[1].clone(),
        request.messages[2].clone(),
        message(Role::Assistant, vec![ContentPart::text("seen")]),
        message(Role::User, vec![ContentPart::text("next")]),
    ]);
    let body = encoded(&followed);
    let state = &body["conversationState"];
    assert_eq!(
        state["history"][2]["userInputMessage"]["images"][0]["source"]["bytes"],
        PNG
    );
    assert!(
        state["currentMessage"]["userInputMessage"]
            .get("images")
            .is_none()
    );
    let historical_image = encoded(&Request::new("claude-sonnet-4.5").with_messages(vec![
        request.messages[0].clone(),
        request.messages[1].clone(),
        request.messages[2].clone(),
        message(Role::Assistant, vec![ContentPart::text("seen")]),
        message(Role::Assistant, vec![ContentPart::text("again")]),
        message(Role::User, vec![ContentPart::text("next")]),
    ]));
    assert_eq!(
        historical_image["conversationState"]["history"][2]["userInputMessage"]["images"][0]["source"]
            ["bytes"],
        PNG
    );
}

#[test]
fn r51_4_image_limit_is_exact_and_unreadable_oversize_becomes_a_note() {
    let at_limit = "a".repeat(crate::kiro_body::IMAGE_LIMIT);
    let request = Request::new("claude-sonnet-4.5").with_messages(vec![message(
        Role::User,
        vec![image("image/png", &at_limit), ContentPart::text("look")],
    )]);
    let body = encoded(&request);
    assert_eq!(
        body["conversationState"]["currentMessage"]["userInputMessage"]["images"][0]["source"]
            ["bytes"]
            .as_str()
            .expect("bytes")
            .len(),
        crate::kiro_body::IMAGE_LIMIT
    );
    let oversized = Request::new("claude-sonnet-4.5").with_messages(vec![message(
        Role::User,
        vec![image("image/png", &format!("{at_limit}bbbb"))],
    )]);
    let body = encoded(&oversized);
    let user = &body["conversationState"]["currentMessage"]["userInputMessage"];
    assert!(user.get("images").is_none());
    assert!(
        user["content"]
            .as_str()
            .expect("note")
            .contains("it couldn't be read as an image")
    );
}

#[test]
fn r51_5_context_thinking_and_structured_output_are_encoded() {
    let mut request = Request::new("claude-opus-4.5").with_messages(vec![
        message(
            Role::System,
            vec![ContentPart::text("SESSION_INSTRUCTIONS")],
        ),
        message(Role::User, vec![ContentPart::text("USER_QUESTION")]),
        message(Role::System, vec![ContentPart::text("ENV_CONTEXT")]),
    ]);
    request
        .extensions
        .insert("thinking", json!({"type": "adaptive"}));
    request.extensions.insert(
        "output_config",
        json!({
            "effort": "xhigh",
            "format": {"type": "json_schema", "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}}
        }),
    );
    let body = encoded(&request);
    let state = &body["conversationState"];
    let context = state["history"][0]["userInputMessage"]["content"]
        .as_str()
        .expect("context");
    assert!(context.starts_with(
        "<thinking_mode>enabled</thinking_mode><max_thinking_length>50000</max_thinking_length>\nSESSION_INSTRUCTIONS"
    ));
    assert!(context.contains("Respond with a single JSON value and nothing else"));
    assert!(context.ends_with("ENV_CONTEXT"));
    assert_eq!(
        state["history"][1]["assistantResponseMessage"]["content"],
        "Understood."
    );
    assert_eq!(
        state["currentMessage"]["userInputMessage"]["content"],
        "USER_QUESTION"
    );
    assert_eq!(state["chatTriggerType"], "MANUAL");
    assert_eq!(body["agentMode"], "vibe");

    let no_initial_context = encoded(&Request::new("claude-opus-4.5").with_messages(vec![
        message(Role::User, vec![ContentPart::text("question")]),
        message(Role::System, vec![ContentPart::text("context")]),
    ]));
    assert_eq!(
        no_initial_context["conversationState"]["history"][0]["userInputMessage"]["content"],
        "context"
    );
    assert_eq!(
        no_initial_context["conversationState"]["currentMessage"]["userInputMessage"]["content"],
        "question"
    );
}

#[test]
fn r51_6_unary_event_stream_returns_tools_and_accepts_images() {
    const LONG: &str = "mcp__plugin_cloudflare_cloudflare-docs__search_cloudflare_documentation";
    let request = Request::new("claude-sonnet-4.5")
        .with_messages(vec![message(
            Role::User,
            vec![ContentPart::text("search"), image("image/png", "aGVsbG8=")],
        )])
        .with_tools(vec![tool(LONG, json!({}))]);
    let sent = encoded(&request);
    let short =
        sent["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
            ["tools"][0]["toolSpecification"]["name"]
            .as_str()
            .expect("short name");
    let mut body = frame("assistantResponseEvent", json!({"content": "working"}));
    body.extend_from_slice(&frame(
        "toolUseEvent",
        json!({"toolUseId": "tooluse_x", "name": short, "input": "{\"q\":"}),
    ));
    body.extend_from_slice(&frame(
        "toolUseEvent",
        json!({"toolUseId": "tooluse_x", "input": "\"r2\"}"}),
    ));
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    host.http().enqueue(body);
    let id = seed(
        &host,
        &dir.path().join("seed-home"),
        "kiro-access-unary",
        "kiro-refresh",
        "2099-01-01T00:00:00Z",
        "k@example.test",
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let completion =
        block_on(upstream.execute(request, ProtocolId::ANTHROPIC_MESSAGES)).expect("unary execute");
    let Completion::Unary(response) = completion else {
        panic!("expected unary response");
    };
    assert_eq!(response.message.content.len(), 2);
    let ContentPart::ToolCall(call) = &response.message.content[1] else {
        panic!("expected tool call");
    };
    assert_eq!(call.name, LONG);
    assert_eq!(call.arguments.to_text(), "{\"q\":\"r2\"}");
    assert_eq!(
        response.stop.as_ref().expect("stop").reason,
        hofund_plugin_api::StopReason::ToolUse
    );
    let recorded = host.http().calls();
    let outbound: Value = json::from_slice(recorded.last().expect("call").body()).expect("JSON");
    assert_eq!(
        outbound["conversationState"]["currentMessage"]["userInputMessage"]["images"][0]["source"]
            ["bytes"],
        "aGVsbG8="
    );
}

#[test]
fn r51_7_loopback_stream_decodes_chunked_text_and_tool_deltas() {
    const LONG: &str = "mcp__plugin_cloudflare_cloudflare-docs__search_cloudflare_documentation";
    let mut request = Request::new("claude-sonnet-4.5")
        .with_messages(vec![message(Role::User, vec![ContentPart::text("search")])])
        .with_tools(vec![tool(LONG, json!({}))])
        .with_stream(true);
    request.stream = true;
    let sent = encoded(&request);
    let short =
        sent["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
            ["tools"][0]["toolSpecification"]["name"]
            .as_str()
            .expect("short name")
            .to_owned();
    let mut body = frame("assistantResponseEvent", json!({"content": "working"}));
    body.extend_from_slice(&frame(
        "toolUseEvent",
        json!({"toolUseId": "tooluse_x", "name": short, "input": "{\"q\":"}),
    ));
    body.extend_from_slice(&frame(
        "toolUseEvent",
        json!({"toolUseId": "tooluse_x", "input": "\"r2\"}"}),
    ));
    body.extend_from_slice(&frame(
        "toolUseEvent",
        json!({"toolUseId": "tooluse_y", "name": "Read", "input": {"path": "a"}}),
    ));
    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for_with_pooled_http_and_tokio_monotonic(
        PluginKind::Subscription,
        PluginId::from_static("kiro"),
        dir.path(),
    );
    let id = seed(
        &host,
        &dir.path().join("seed-home"),
        "kiro-access-stream",
        "kiro-refresh",
        "2099-01-01T00:00:00Z",
        "k@example.test",
    );
    let events = block_on(async {
        let server = Http1Server::start(vec![Http1Response::new(200).body(body)]).await;
        let endpoints = Endpoints {
            runtime: server.base_url().to_owned(),
            ..Endpoints::default()
        };
        let sub = factory_with_endpoints(endpoints)
            .build(&KiroConfig, host.context())
            .expect("build");
        let upstream = sub.upstream(&id, host.context()).expect("upstream");
        let Completion::Stream(mut stream) = upstream
            .execute(request, ProtocolId::ANTHROPIC_MESSAGES)
            .await
            .expect("stream execute")
        else {
            panic!("expected stream");
        };
        let mut events = Vec::new();
        while let Some(event) =
            std::future::poll_fn(|context| stream.as_mut().poll_next(context)).await
        {
            events.push(event.expect("event"));
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path(), "/generateAssistantResponse");
        assert_eq!(requests[0].header("content-type"), Some("application/json"));
        assert_eq!(
            requests[0].header("accept"),
            Some("application/vnd.amazon.eventstream")
        );
        server.shutdown().await;
        events
    });
    assert!(matches!(events.first(), Some(StreamEvent::MessageStart(_))));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::TextDelta(delta) if delta.text == "working"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ContentBlockStart(start)
            if start.index == 1 && matches!(&start.block, hofund_plugin_api::BlockStart::ToolCall { id, name } if id == "tooluse_x" && name == LONG)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ContentBlockStart(start)
            if start.index == 2 && matches!(&start.block, hofund_plugin_api::BlockStart::ToolCall { id, name } if id == "tooluse_y" && name == "Read")
    )));
    let arguments: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolArgumentsDelta(delta) => Some(delta.partial_json.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(arguments, ["{\"q\":", "\"r2\"}", "{\"path\":\"a\"}"]);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::MessageDelta(delta)
            if delta.stop.as_ref().is_some_and(|stop| stop.reason == hofund_plugin_api::StopReason::ToolUse)
    )));
    assert!(matches!(events.last(), Some(StreamEvent::MessageStop(_))));
}

#[test]
fn r51_8_decoder_handles_every_byte_boundary_and_rejects_partial_tail() {
    let mut body = frame("assistantResponseEvent", json!({"content": "hel"}));
    body.extend_from_slice(&frame("assistantResponseEvent", json!({"content": "lo"})));
    body.extend_from_slice(&frame(
        "toolUseEvent",
        json!({"toolUseId": "call-1", "name": "read", "input": "{\"path\":"}),
    ));
    body.extend_from_slice(&frame(
        "toolUseEvent",
        json!({"toolUseId": "call-1", "input": "\"a\"}"}),
    ));
    let mut decoder = crate::kiro_stream::Decoder::default();
    let mut texts = String::new();
    let mut arguments = String::new();
    let mut tools = 0;
    for byte in &body {
        decoder.push(&[*byte]).expect("byte");
        while let Some(event) = decoder.next_event() {
            match event {
                crate::kiro_stream::Event::Text(text) => texts.push_str(&text),
                crate::kiro_stream::Event::Thinking(_)
                | crate::kiro_stream::Event::Signature(_) => {
                    panic!("unexpected reasoning")
                }
                crate::kiro_stream::Event::ToolStart { .. } => tools += 1,
                crate::kiro_stream::Event::ToolArguments(part) => arguments.push_str(&part),
                crate::kiro_stream::Event::Stop(_) => panic!("early stop"),
            }
        }
    }
    decoder.finish().expect("finish");
    assert_eq!(texts, "hello");
    assert_eq!(arguments, "{\"path\":\"a\"}");
    assert_eq!(tools, 1);
    assert!(matches!(
        decoder.next_event(),
        Some(crate::kiro_stream::Event::Stop(
            hofund_plugin_api::StopReason::ToolUse
        ))
    ));
    let mut partial = crate::kiro_stream::Decoder::default();
    partial.push(&body[..body.len() - 1]).expect("partial");
    assert!(partial.finish().is_err());
}

#[test]
fn r51c_native_effort_fields_follow_the_recorded_model_schema() {
    let schema = json!({"properties": {
        "thinking": {"properties": {"type": {"enum": ["adaptive", "disabled"]}}},
        "output_config": {"properties": {"effort": {"enum": ["low", "medium", "high", "xhigh", "max"]}}}
    }});
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(
        &host,
        &dir.path().join("seed-home"),
        "kiro-access-effort",
        "kiro-refresh",
        "2099-01-01T00:00:00Z",
        "k@example.test",
    );
    host.http().enqueue(
        json::to_vec(&json!({
            "defaultModel": {"modelId": "claude-opus-5.5"},
            "models": [
                {"modelId": "claude-sonnet-4.5", "modelName": "Claude Sonnet", "additionalModelRequestFieldsSchema": null},
                {"modelId": "claude-opus-5.5", "modelName": "Claude Opus", "tokenLimits": {"maxInputTokens": 200000, "maxOutputTokens": 64000}, "additionalModelRequestFieldsSchema": schema}
            ]
        }))
        .expect("models JSON"),
    );
    let list = block_on(sub.list_models(&id, host.context())).expect("model list");
    assert_eq!(list.models()[0].id(), "claude-opus-5.5");
    assert_eq!(
        list.models()[0].efforts(),
        ["low", "medium", "high", "xhigh", "max"]
    );
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let request = |model: &str, effort: &str, thinking: Option<Value>| {
        let mut request = Request::new(model).with_messages(vec![message(
            Role::User,
            vec![ContentPart::text("question")],
        )]);
        if let Some(thinking) = thinking {
            request.extensions.insert("thinking", thinking);
        }
        request
            .extensions
            .insert("output_config", json!({"effort": effort}));
        request
    };
    for effort in ["xhigh", "max"] {
        host.http().enqueue(br#"{"content":"ok"}"#.to_vec());
        block_on(upstream.execute(
            request("claude-opus-5.5", effort, Some(json!({"type": "adaptive"}))),
            ProtocolId::ANTHROPIC_MESSAGES,
        ))
        .expect("native effort request");
        let calls = host.http().calls();
        let body: Value = json::from_slice(calls.last().expect("request").body()).expect("JSON");
        assert_eq!(
            body["additionalModelRequestFields"],
            json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": effort}})
        );
        assert!(!body.to_string().contains("max_thinking_length"));
    }
    for (model, effort, thinking, budget) in [
        (
            "claude-opus-5.5",
            "ultra",
            Some(json!({"type": "adaptive"})),
            20000,
        ),
        (
            "claude-sonnet-4.5",
            "xhigh",
            Some(json!({"type": "enabled"})),
            50000,
        ),
    ] {
        host.http().enqueue(br#"{"content":"ok"}"#.to_vec());
        block_on(upstream.execute(
            request(model, effort, thinking),
            ProtocolId::ANTHROPIC_MESSAGES,
        ))
        .expect("budget request");
        let calls = host.http().calls();
        let body: Value = json::from_slice(calls.last().expect("request").body()).expect("JSON");
        assert!(body.get("additionalModelRequestFields").is_none());
        assert!(body.to_string().contains(&format!(
            "<max_thinking_length>{budget}</max_thinking_length>"
        )));
    }
    host.http().enqueue(br#"{"content":"ok"}"#.to_vec());
    block_on(upstream.execute(
        request("claude-opus-5.5", "high", None),
        ProtocolId::ANTHROPIC_MESSAGES,
    ))
    .expect("thinking-off request");
    let calls = host.http().calls();
    let body: Value = json::from_slice(calls.last().expect("request").body()).expect("JSON");
    assert!(body.get("additionalModelRequestFields").is_none());
    assert!(!body.to_string().contains("thinking_mode"));
}

#[test]
fn r51c_reasoning_events_fold_and_stream_with_signatures() {
    let mut body = frame("reasoningContentEvent", json!({"text": "consider"}));
    body.extend_from_slice(&frame("reasoningContentEvent", json!({"text": "ing"})));
    body.extend_from_slice(&frame(
        "reasoningContentEvent",
        json!({"signature": "sig-"}),
    ));
    body.extend_from_slice(&frame("reasoningContentEvent", json!({"signature": "one"})));
    body.extend_from_slice(&frame(
        "assistantResponseEvent",
        json!({"content": "answer"}),
    ));
    let response = kiro_stream::unary(&body, "claude-opus-5.5", &std::collections::HashMap::new())
        .expect("unary reasoning");
    assert_eq!(response.message.content.len(), 2);
    let ContentPart::Thinking(thinking) = &response.message.content[0] else {
        panic!("expected reasoning first");
    };
    assert_eq!(thinking.text, "considering");
    assert_eq!(thinking.signature.as_deref(), Some("sig-one"));
    assert_eq!(response.message.content[1], ContentPart::text("answer"));

    let Completion::Stream(mut stream) = kiro_stream::stream(
        hofund_plugin_api::BodyStream::from_bytes(body),
        "claude-opus-5.5".to_owned(),
        std::collections::HashMap::new(),
    ) else {
        panic!("expected stream");
    };
    let events = block_on(async {
        let mut events = Vec::new();
        while let Some(event) =
            std::future::poll_fn(|context| stream.as_mut().poll_next(context)).await
        {
            events.push(event.expect("event"));
        }
        events
    });
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ContentBlockStart(start)
            if start.index == 0 && matches!(start.block, hofund_plugin_api::BlockStart::Thinking { redacted: false, .. })
    )));
    let reasoning: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ThinkingDelta(delta) => Some((delta.index, delta.text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, [(0, "consider"), (0, "ing")]);
    let signatures: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::SignatureDelta(delta) => Some((delta.index, delta.signature.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(signatures, [(0, "sig-"), (0, "one")]);
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::TextDelta(delta) if delta.index == 1 && delta.text == "answer"
    )));
    assert!(matches!(events.last(), Some(StreamEvent::MessageStop(_))));
}
