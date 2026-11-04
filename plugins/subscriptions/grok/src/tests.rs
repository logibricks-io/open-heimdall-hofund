//! Unit tests against recording host backends; no network.

use std::sync::Arc;

use hofund_kit_subscription::{PipedScript, TestHost, block_on};
use hofund_plugin_api::{
    AccountStatus, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, RawValue, SecretString,
    SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::{Map, Value, json};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::creds::{self, Credential, Stored};
use crate::discover::{self, REFERENCE};
use crate::quota;
use crate::shape;
use crate::{Endpoints, factory_with_endpoints};

const BASE: &str = "https://grok.test/v1";
const TOKEN: &str = "access-example-token";
const SESSION: &str = "0123456789abcdef0123456789abcdef";
#[cfg(not(windows))]
const PROGRAM: &str = "/opt/grok/bin/grok";
#[cfg(windows)]
const PROGRAM: &str = r"C:\grok\bin\grok.exe";

fn endpoints() -> Endpoints {
    Endpoints {
        base: BASE.to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("grok"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let mut raw = RawTable::new();
    raw.insert("command", RawValue::String(PROGRAM.to_owned()));
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&raw).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(PluginId::from_static("grok"))
        .with_session(SESSION)
        .expect("session")
}

fn write_auth(home: &std::path::Path, token: &str, email: &str, expires: &str) {
    std::fs::create_dir_all(home).expect("home");
    let text = format!(
        r#"{{
  "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {{
    "key": "{token}",
    "email": "{email}",
    "expires_at": "{expires}"
  }}
}}"#
    );
    std::fs::write(home.join("auth.json"), text).expect("auth");
}

fn seed(
    host: &TestHost,
    home: &std::path::Path,
    token: &str,
    expires: &str,
) -> hofund_plugin_api::AccountId {
    write_auth(home, token, "user@example.test", expires);
    let credential = creds::read_sync(home).expect("credential");
    let id = creds::account_id(&credential);
    let secret = creds::secret_blob(home, &credential).to_secret();
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "user@example.test")
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, secret)
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn a2a_2_discover_import_fixture_and_reimport() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("grok-home");
    write_auth(
        &home,
        TOKEN,
        "user@example.test",
        "2099-01-01T00:00:00.000000000Z",
    );
    let rows = discover::list(&home).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reference(), REFERENCE);

    let host = host(&dir);
    let id = block_on(discover::import(&home, REFERENCE, host.context())).expect("import");
    write_auth(
        &home,
        "rotated-token",
        "user@example.test",
        "2099-06-01T00:00:00.000000000Z",
    );
    block_on(discover::import(&home, REFERENCE, host.context())).expect("reimport");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("access_token")
            .map(|t| hofund_plugin_sdk::oauth::secret_text(&t).to_owned()),
        Some("rotated-token".to_owned())
    );
}

#[test]
fn a2a_3_refresh_on_expiry_runs_grok_models_with_home() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let home = dir.path().join("account-home");
    let id = seed(&host, &home, "token-old", "1970-01-01T00:01:00.000000000Z");
    write_auth(
        &home,
        "token-new",
        "user@example.test",
        "2099-01-01T00:00:00.000000000Z",
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh again");
    let spawns = host.process().programs();
    assert_eq!(spawns.len(), 1, "refresh runs grok models");
    assert_eq!(spawns[0].to_string_lossy(), PROGRAM);
    let request = crate::cli::request(
        std::path::Path::new(PROGRAM),
        &crate::cli::MODELS_ARGS,
        &home,
    )
    .expect("request");
    assert_eq!(
        request
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        vec!["models".to_owned()]
    );
    assert_eq!(
        request
            .env()
            .iter()
            .find(|(key, _)| key == crate::cli::HOME_VAR)
            .map(|(_, value)| value.to_string_lossy().into_owned()),
        Some(home.to_string_lossy().into_owned())
    );
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("access_token")
            .map(|token| hofund_plugin_sdk::oauth::secret_text(&token).to_owned()),
        Some("token-new".to_owned())
    );
}

#[test]
fn a2a_4_model_list_parsing() {
    let body = br#"{"data":[{"id":"grok-4.7","name":"Grok 4.7","api_backend":"responses","context_window":256000,"context_windows":[256000,500000],"reasoning_efforts":[{"value":"high"},{"value":"low"}]}]}"#;
    let models = crate::models::parse_list(body).expect("parse");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id(), "grok-4.7");
    assert_eq!(models[1].id(), "grok-4.7@500k");
}

#[test]
fn a2a_5_upstream_round_trip_with_recording_http() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let home = dir.path().join("account-home");
    let id = seed(&host, &home, TOKEN, "2099-01-01T00:00:00.000000000Z");
    host.http().enqueue(
        b"event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n".to_vec(),
    );
    host.process()
        .script_piped(PipedScript::new().exits(|_| vec![]));
    let upstream = sub.upstream(&id, host.context()).expect("upstream");
    let mut request = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "grok-4.7@500k")
        .expect("query")
        .minimal_request();
    request.stream = false;
    block_on(upstream.execute(request, ProtocolId::OPENAI_RESPONSES)).expect("execute");
    let calls = host.http().calls();
    let call = calls.last().expect("call");
    assert_eq!(call.method(), "POST");
    assert!(call.uri().starts_with(&format!("{BASE}/responses")));
    let body = std::str::from_utf8(call.body()).expect("utf8");
    assert!(body.contains("\"model\":\"grok-4.7\""));
}

#[test]
fn a2a_6_usage_parsing() {
    let body = br#"{"config":{"creditUsagePercent":42.5,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2099-06-01T00:00:00Z"},"onDemandCap":{"val":10},"onDemandUsed":{"val":2.5}}}"#;
    let windows = quota::windows(body).expect("windows");
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].name(), "7 days");
    assert_eq!(windows[0].used(), 43);
    assert_eq!(windows[1].name(), "On-demand");
    assert!(!windows[1].blocking());
}

#[test]
fn a2a_7_secrets_not_in_stored_debug_or_auth_errors() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("grok-home");
    write_auth(
        &home,
        TOKEN,
        "user@example.test",
        "2099-01-01T00:00:00.000000000Z",
    );
    let stored = Stored {
        home: home.display().to_string(),
        credential: Credential {
            key: SecretString::from(TOKEN.to_owned()),
            email: "user@example.test".to_owned(),
            who: "user@example.test".to_owned(),
            expires_ms: 4_000_000_000_000,
        },
    };
    let debug = format!("{stored:?}");
    assert!(!debug.contains(TOKEN));
    let host = host(&dir);
    block_on(async {
        let status = subscription(&host)
            .account_status(
                &account_id_from(&["grok", "missing@example.test"]),
                host.context(),
            )
            .await
            .expect("status");
        assert!(matches!(status, AccountStatus::NeedsSignIn { .. }));
    });
}

#[test]
fn a2a_8_shape_rewrites_agent_message_and_tools() {
    let mut body: Map<String, Value> = json!({
        "tools": [{"type":"custom_tool","name":"drop-me"}],
        "input": [{
            "type":"agent_message",
            "author":"a",
            "recipient":"b",
            "content":[{"type":"input_text","text":"hello"}]
        }]
    })
    .as_object()
    .expect("object")
    .clone();
    shape::body(&mut body, false);
    let tools = body.get("tools").and_then(Value::as_array).expect("tools");
    assert!(tools.is_empty());
    let input = body.get("input").and_then(Value::as_array).expect("input");
    assert_eq!(
        input[0].get("type").and_then(Value::as_str),
        Some("message")
    );
}

#[test]
fn a2a_9_near_margin_account_stays_ready() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let home = dir.path().join("account-home");
    write_auth(
        &home,
        TOKEN,
        "user@example.test",
        "1970-01-01T00:03:00.000000000Z",
    );
    let credential = creds::read_sync(&home).expect("credential");
    let stored = Stored {
        home: home.display().to_string(),
        credential,
    };
    let now = host.context().clock().now();
    assert!(stored.ready_at(now));
    assert!(!stored.fresh_at(now));
}

#[test]
fn a2a_10_sign_in_start_complete_and_cancel() {
    let dir = tempfile::tempdir().expect("dir");
    let test_host = host(&dir);
    let grok_home = test_host
        .context()
        .dirs()
        .state_dir()
        .expect("state")
        .join("grok-homes")
        .join(SESSION);
    let sub = subscription(&test_host);
    test_host.process().script_piped(
        PipedScript::new()
            .line("Visit https://auth.test/device")
            .exits(|_| vec![]),
    );
    let challenge = block_on(sub.start_sign_in(&ctx(), test_host.context())).expect("start");
    assert!(matches!(
        challenge,
        hofund_plugin_api::SignInChallenge::OpenUrl { .. }
            | hofund_plugin_api::SignInChallenge::Instructions { .. }
    ));
    write_auth(
        &grok_home,
        "signed-in-token",
        "signin@example.test",
        "2099-01-01T00:00:00.000000000Z",
    );
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, test_host.context()))
        .expect("complete");
    assert_eq!(id, account_id_from(&["grok", "signin@example.test"]));
    assert!(
        grok_home.is_dir(),
        "sign-in home stays on disk after a successful import"
    );

    let dir2 = tempfile::tempdir().expect("dir2");
    let test_host2 = host(&dir2);
    let sub2 = subscription(&test_host2);
    let grok_home2 = test_host2
        .context()
        .dirs()
        .state_dir()
        .expect("state")
        .join("grok-homes")
        .join(SESSION);
    test_host2
        .process()
        .script_piped(PipedScript::new().line("Visit https://auth.test/device-2"));
    block_on(sub2.start_sign_in(&ctx(), test_host2.context())).expect("start2");
    assert!(grok_home2.is_dir(), "login creates an isolated home");
    block_on(sub2.cancel_sign_in(&ctx(), test_host2.context())).expect("cancel");
    assert!(
        !grok_home2.exists(),
        "cancel removes the isolated sign-in home"
    );
    let err = block_on(sub2.complete_sign_in(&ctx(), SignInProof::Await, test_host2.context()))
        .expect_err("cancelled");
    assert_eq!(err.kind(), hofund_plugin_api::PluginErrorKind::Config);
}

#[test]
fn a2a_11_missing_expiry_skips_grok_models_on_refresh() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let home = dir.path().join("account-home");
    write_auth(
        &home,
        TOKEN,
        "user@example.test",
        "2099-01-01T00:00:00.000000000Z",
    );
    let mut credential = creds::read_sync(&home).expect("credential");
    credential.expires_ms = 0;
    let id = creds::account_id(&credential);
    let secret = creds::secret_blob(&home, &credential).to_secret();
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "user@example.test")
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, secret)
            .await
            .expect("upsert");
    });
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert!(
        host.process().programs().is_empty(),
        "missing expiry must not run grok models on every refresh"
    );
}

#[test]
fn a2a_12_refresh_margin_uses_strictly_after_boundary() {
    let margin = creds::REFRESH_MARGIN;
    let margin_ms = margin.as_millis() as u64;
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_000_000);
    assert!(
        creds::expires_after(now, 1_000_000 + margin_ms + 1, margin),
        "one millisecond past the margin should still be fresh"
    );
    assert!(
        !creds::expires_after(now, 1_000_000 + margin_ms, margin),
        "exactly at the margin should not count as fresh"
    );
}
