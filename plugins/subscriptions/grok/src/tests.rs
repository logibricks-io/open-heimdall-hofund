//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::Duration;

use hofund_kit_subscription::{PipedScript, TestHost, advance_time, block_on, block_on_paused};
use hofund_plugin_api::{
    PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, RawValue, SecretString, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::json::{Map, Value, json};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::creds::{self, Credential};
use crate::discover::{self, REFERENCE};
use crate::quota;
use crate::shape;
use crate::{Endpoints, factory_with_endpoints};

const BASE: &str = "https://grok.test/v1";
const TOKEN: &str = "access-example-token";
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
fn a2a_2_discover_import_fixture() {
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
    assert_eq!(rows[0].label(), "user@example.test");
    let text = format!("{rows:?}");
    assert!(!text.contains(TOKEN));

    let host = host(&dir);
    let id = block_on(discover::import(&home, REFERENCE, host.context())).expect("import");
    assert_eq!(id, account_id_from(&["grok", "user@example.test"]));
}

#[test]
fn a2a_3_refresh_on_expiry_runs_grok_models() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let home = dir.path().join("account-home");
    let id = seed(&host, &home, "token-old", "1970-01-01T00:05:00.000000000Z");
    block_on_paused(async {
        advance_time(Duration::from_secs(3600)).await;
        // The CLI rewrites auth.json when it renews; simulate that on disk.
        write_auth(
            &home,
            "token-new",
            "user@example.test",
            "2099-01-01T00:00:00.000000000Z",
        );
        sub.refresh(&id, host.context()).await.expect("refresh");
    });
    assert_eq!(
        host.process().programs().len(),
        1,
        "refresh runs grok models"
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
fn a2a_7_secrets_not_in_debug_or_errors() {
    let credential = Credential {
        key: SecretString::from(TOKEN.to_owned()),
        email: "user@example.test".to_owned(),
        who: "user@example.test".to_owned(),
        expires_ms: 0,
    };
    let text = format!("{credential:?}");
    assert!(!text.contains(TOKEN));
    let error = creds::read_sync(std::path::Path::new("/no/such/home")).expect_err("missing");
    assert!(!error.message().contains(TOKEN));
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
fn a2a_9_import_fixture_matches_discover_reference() {
    let fixture = include_str!("fixtures/auth.json");
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("grok-home");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(home.join("auth.json"), fixture).expect("auth");
    let host = host(&dir);
    let id = block_on(discover::import(&home, REFERENCE, host.context())).expect("import");
    assert_eq!(id, account_id_from(&["grok", "user@example.test"]));
}
