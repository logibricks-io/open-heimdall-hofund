//! D-222: agy homes, the account move, quota and verification rests, and the Gemini key reader.
//! Scripted children stand in for `agy`. No network and no real binary.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;

use hofund_kit_subscription::{PipedScript, TestHost, block_on};
use hofund_plugin_api::{
    Completion, ContentPart, PluginId, PluginKind, ProtocolId, RawTable, RawValue, Request, Role,
    SecretString,
};
use hofund_plugin_sdk::oauth::secret_text;

use crate::gemini_key::{GeminiKeySkip, GeminiKeyStatus, gemini_key_status, read_gemini_key};
use crate::move_accounts::{MoveRow, account_proxy_to_store, provider_after_carry, select_moves};
use crate::{AgyCliFactory, contribute};
use hofund_plugin_api::SubscriptionFactory;

fn host(dir: &std::path::Path) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("agy-cli"),
        dir,
    )
}

fn factory_config(raw_command: &str) -> crate::AgyCliConfig {
    let mut raw = RawTable::new();
    raw.insert("command", RawValue::String(raw_command.to_owned()));
    AgyCliFactory.validate(&raw).expect("config")
}

fn row(key: &str, email: &str, refresh: &str, proxy: &str) -> MoveRow {
    MoveRow {
        key: key.to_owned(),
        email: email.to_owned(),
        refresh: SecretString::from(refresh.to_owned()),
        proxy: proxy.to_owned(),
    }
}

#[test]
fn d222_move_orders_bare_keys_and_skips_duplicates() {
    let rows = vec![
        row("antigravity#b", "b@example.test", "refresh-b|extra", ""),
        row("antigravity", "a@example.test", "refresh-a", ""),
        row("antigravity#a", "A@example.test", "other-refresh", ""),
        row("plain", "c@example.test", "refresh-a|again", ""),
        row("later", "kept@example.test", "refresh-kept", ""),
    ];
    let mut have = HashSet::new();
    have.insert("kept@example.test".to_owned());
    let selected = select_moves(rows, &have);
    let emails: Vec<_> = selected.iter().map(|row| row.email.as_str()).collect();
    assert_eq!(emails, ["a@example.test", "b@example.test"]);
}

#[test]
fn d222_proxy_carry_keeps_a_user_proxy() {
    assert_eq!(
        provider_after_carry(0, "", "http://source.test:8080"),
        "http://source.test:8080"
    );
    assert_eq!(
        provider_after_carry(0, "direct", "http://source.test:8080"),
        "direct"
    );
    assert_eq!(provider_after_carry(2, "", "http://source.test:8080"), "");
    assert_eq!(
        account_proxy_to_store("direct", "http://source.test:8080", ""),
        "http://source.test:8080"
    );
    assert_eq!(
        account_proxy_to_store("http://source.test:8080", "http://source.test:8080", ""),
        ""
    );
    assert_eq!(
        account_proxy_to_store("direct", "http://source.test:8080", "http://account.test:9"),
        "http://account.test:9"
    );
}

#[test]
fn d222_import_writes_a_private_home_and_keeps_direct() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(dir.path());
    host.process().script_piped(
        PipedScript::new()
            .line(r#"{"models":[{"id":"gemini-3-flash","name":"Flash"}]}"#)
            .exits(|_| Vec::new()),
    );
    host.process().script_piped(
        PipedScript::new()
            .stderr("import refused")
            .status(1)
            .exits(|_| Vec::new()),
    );
    let subscription = AgyCliFactory
        .build(&factory_config("/opt/agy/agy"), host.context())
        .expect("build");
    let payload = r#"{
        "agy_proxy": "direct",
        "from_proxy": {
            "provider": "http://source.test:8080",
            "accounts": {"moved@example.test": "http://account.test:9"}
        },
        "accounts": [
            {"key": "antigravity", "accountId": "Moved@Example.Test", "refresh": "refresh-moved"},
            {"key": "antigravity#z", "accountId": "failed@example.test", "refresh": "refresh-failed"}
        ]
    }"#;
    let error = block_on(subscription.import_accounts(payload, host.context()))
        .expect_err("one account fails");
    let error = error.to_string();
    assert!(error.contains("failed@example.test"), "{error}");
    assert!(!error.contains("refresh-failed"), "{error}");
    let accounts = block_on(host.context().accounts().list()).expect("list");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].label(), "moved@example.test");
    let id = accounts[0].id().clone();
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("secret")
        .expect("stored");
    let object = hofund_plugin_sdk::oauth::SecretObject::parse(&secret).expect("object");
    assert_eq!(object.text("proxy"), Some("http://account.test:9"));
    assert_eq!(object.text("email"), Some("moved@example.test"));
    let home = object.text("home").expect("home");
    let sign_in = std::path::Path::new(home).join("sign-in.json");
    let mode = std::fs::metadata(&sign_in)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let body = std::fs::read_to_string(&sign_in).expect("sign-in");
    assert!(
        body.contains("refresh-moved"),
        "sign-in file is missing its refresh"
    );
    assert!(
        !format!("{accounts:?}").contains("refresh-moved"),
        "account debug must not show the refresh"
    );
    let proxy = std::fs::read_to_string(
        host.context()
            .dirs()
            .state_dir()
            .expect("state")
            .join("provider-proxy.txt"),
    )
    .expect("proxy file");
    assert_eq!(proxy.trim(), "direct");
    let spawns = host.process().piped_spawns();
    assert_eq!(spawns.len(), 2);
    assert_eq!(
        spawns[0]
            .env_var(crate::cli::PROXY_VAR)
            .and_then(|value| value.to_str()),
        Some("direct")
    );
    assert!(spawns[0].env_var("HTTPS_PROXY").is_none());
    assert!(spawns[0].env_var(crate::cli::HOME_VAR).is_some());
}

#[test]
fn d222_import_copies_the_source_proxy_when_agy_has_none() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(dir.path());
    host.process().script_piped(
        PipedScript::new()
            .line(r#"{"models":[{"id":"gemini-3-flash","name":"Flash"}]}"#)
            .exits(|_| Vec::new()),
    );
    let subscription = AgyCliFactory
        .build(&factory_config("/opt/agy/agy"), host.context())
        .expect("build");
    let payload = r#"[{
        "type": "antigravity",
        "key": "antigravity",
        "metadata": {"email": "meta@example.test"},
        "refresh": "refresh-meta|tail"
    }]"#;
    // No source proxy on a bare array: nothing is written.
    let ids = block_on(subscription.import_accounts(payload, host.context())).expect("import");
    assert_eq!(ids.len(), 1);
    let proxy_path = host
        .context()
        .dirs()
        .state_dir()
        .expect("state")
        .join("provider-proxy.txt");
    assert!(!proxy_path.exists());
    let spawn = &host.process().piped_spawns()[0];
    assert_eq!(
        spawn
            .env_var(crate::cli::PROXY_VAR)
            .and_then(|value| value.to_str()),
        Some("")
    );
}

#[test]
fn d222_quota_scopes_the_model_and_verification_keeps_one_link() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(dir.path());
    let home = dir.path().join("home");
    let (id, blob) = crate::matrix_sign_in(&home, "quota@example.test").expect("matrix");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "quota@example.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("seed");
    });
    host.process().script_piped(PipedScript::new().on_stdin(|_, _| {
        vec![
            r#"{"error":"quota","message":"Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h18m40s."}"#
                .to_owned(),
        ]
    }));
    let subscription = AgyCliFactory
        .build(&factory_config("/opt/agy/agy"), host.context())
        .expect("build");
    let upstream = subscription
        .upstream(&id, host.context())
        .expect("upstream");
    let error = match block_on(upstream.execute(request(false, false), ProtocolId::OPENAI_CHAT)) {
        Ok(_) => panic!("expected a quota error"),
        Err(error) => error,
    };
    assert!(error.scopes_model());
    assert_eq!(
        error.reset_after(),
        Some(std::time::Duration::from_secs(4_720))
    );
    assert!(
        error
            .message()
            .contains("agy bridge: Antigravity quota reached:")
    );

    host.process().script_piped(PipedScript::new().on_stdin(|_, _| {
        vec![
            r#"{"error":"verify","message":"Verification Required","link":"https://accounts.example/verify"}"#
                .to_owned(),
        ]
    }));
    let error = match block_on(upstream.execute(request(false, false), ProtocolId::OPENAI_CHAT)) {
        Ok(_) => panic!("expected a verification error"),
        Err(error) => error,
    };
    assert!(error.verifies_account());
    assert!(!error.scopes_model());
    assert_eq!(error.message().matches("needs to be verified").count(), 1);
    assert!(error.message().contains("https://accounts.example/verify"));
    assert!(error.message().contains("agy: \"Verification Required\""));

    host.process()
        .script_piped(PipedScript::new().on_stdin(|_, stdin| {
            let text = String::from_utf8_lossy(stdin);
            assert!(text.contains("lookup"), "prompt must name the tool");
            vec![
                r#"{"tool":{"name":"lookup","id":"call-1","arguments":"{\"query\":\"fixture\"}"}}"#
                    .to_owned(),
            ]
        }));
    let completion =
        block_on(upstream.execute(request(true, true), ProtocolId::ANTHROPIC_MESSAGES))
            .expect("tool");
    match completion {
        Completion::Stream(mut stream) => {
            let mut saw_tool = false;
            block_on(async {
                use hofund_plugin_sdk::futures::StreamExt as _;
                while let Some(event) = stream.next().await {
                    if let hofund_plugin_api::StreamEvent::ContentBlockStart(start) =
                        event.expect("event")
                        && matches!(start.block, hofund_plugin_api::BlockStart::ToolCall { .. })
                    {
                        saw_tool = true;
                    }
                }
            });
            assert!(saw_tool);
        }
        _ => panic!("expected a stream"),
    }
}

#[test]
fn d222_other_quota_words_do_not_scope_the_model() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(dir.path());
    let home = dir.path().join("home");
    let (id, blob) = crate::matrix_sign_in(&home, "other@example.test").expect("matrix");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "other@example.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("seed");
    });
    host.process().script_piped(
        PipedScript::new().on_stdin(|_, _| vec!["Quota exceeded. Resets in 1h.".to_owned()]),
    );
    let subscription = AgyCliFactory
        .build(&factory_config("/opt/agy/agy"), host.context())
        .expect("build");
    let upstream = subscription
        .upstream(&id, host.context())
        .expect("upstream");
    let error = match block_on(upstream.execute(request(false, false), ProtocolId::OPENAI_CHAT)) {
        Ok(_) => panic!("expected an account quota error"),
        Err(error) => error,
    };
    assert!(!error.scopes_model());
    assert_eq!(
        error.reset_after(),
        Some(std::time::Duration::from_hours(1))
    );
}

#[test]
fn d222_gemini_key_is_offered_only_for_the_gemini_provider() {
    let offered = read_gemini_key(
        "{\n  // comment\n  \"modelProvider\": \"Gemini\",\n  \"GOOGLE_GEMINI_BASE_URL\": \"https://generativelanguage.googleapis.com\"\n}\n",
        Some("env-key"),
        None,
        &[],
    )
    .expect("offer")
    .expect("key");
    assert!(
        secret_text(offered.key()) == "env-key",
        "the key did not come from the environment"
    );
    assert_eq!(
        offered.base(),
        "https://generativelanguage.googleapis.com/v1beta"
    );
    assert_eq!(
        gemini_key_status(&offered, &["env-key"]),
        GeminiKeyStatus::Same
    );
    assert_eq!(
        gemini_key_status(&offered, &["other-key"]),
        GeminiKeyStatus::New
    );
    assert!(
        read_gemini_key("{\"modelProvider\":\"vertex\"}", Some("env-key"), None, &[]).is_none()
    );
    assert!(read_gemini_key("{}", Some("env-key"), None, &[]).is_none());
    assert!(matches!(
        read_gemini_key("{\"modelProvider\":\"gemini\"}", None, None, &[]),
        Some(Err(GeminiKeySkip::NoKey))
    ));
    assert!(matches!(
        read_gemini_key(
            "{\"modelProvider\":\"gemini\"}",
            Some("env-key"),
            Some("http://127.0.0.1:8080"),
            &["127.0.0.1:8080"],
        ),
        Some(Err(GeminiKeySkip::PointsAtGateway))
    ));
    let kept = read_gemini_key(
        "{\"modelProvider\":\"gemini\"}",
        Some("env-key"),
        Some("https://example.test/v1beta"),
        &[],
    )
    .expect("offer")
    .expect("key");
    assert_eq!(kept.base(), "https://example.test/v1beta");
    let _ = format!("{kept:?}");
    assert!(!format!("{kept:?}").contains("env-key"));
}

#[test]
fn d222_contributes_a_bridge_and_quota_is_empty() {
    let contribution = contribute();
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription factory");
    };
    assert_eq!(factory.spec().plugin().as_str(), "agy-cli");
    assert_eq!(
        factory.spec().flow(),
        hofund_plugin_api::SignInFlowKind::Bridge
    );
    let dir = tempfile::tempdir().expect("dir");
    let host = host(dir.path());
    let built = factory
        .validate_and_build(
            &{
                let mut raw = RawTable::new();
                raw.insert("command", RawValue::String("/opt/agy/agy".to_owned()));
                raw
            },
            host.context(),
        )
        .expect("build");
    let home = dir.path().join("home");
    let (id, blob) = crate::matrix_sign_in(&home, "ready@example.test").expect("matrix");
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), "ready@example.test")
            .expect("account");
        accounts
            .upsert(account, blob.to_secret())
            .await
            .expect("seed");
    });
    let windows = block_on(built.quota(&id, host.context())).expect("quota");
    assert!(windows.is_empty());
    let status = block_on(built.account_status(&id, host.context())).expect("status");
    assert!(matches!(status, hofund_plugin_api::AccountStatus::Ready));
}

fn request(stream: bool, tools: bool) -> Request {
    let mut request = Request::new("gemini-3-flash");
    request.stream = stream;
    let mut message = hofund_plugin_api::Message::new(Role::User);
    message.content.push(ContentPart::text("conformance"));
    request.messages.push(message);
    if tools {
        request.tools.push(hofund_plugin_api::Tool {
            name: "lookup".to_owned(),
            description: None,
            input_schema: hofund_plugin_sdk::json::json!({"type": "object"}),
            extensions: hofund_plugin_api::Extensions::new(),
        });
    }
    request
}
