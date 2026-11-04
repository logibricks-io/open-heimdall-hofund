//! Unit tests against recording host backends; no network.

use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, Credential, TOKEN_FILE};
use crate::{Endpoints, KiroConfig, discover, factory_with_endpoints};

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
            r#"{{"accessToken":"{access}","refreshToken":"{refresh}","expiresAt":"{exp}","region":"us-east-1","profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/P"}}"#
        ),
    )
    .expect("write");
}

fn seed(host: &TestHost, access: &str, refresh: &str, exp: &str, label: &str) -> AccountId {
    let expires_at = hofund_plugin_sdk::time::parse_rfc3339(exp)
        .or_else(|| Some(UNIX_EPOCH + Duration::from_secs(1)));
    let credential = Credential {
        access: SecretString::from(access.to_owned()),
        refresh: SecretString::from(refresh.to_owned()),
        expires_at,
        region: "us-east-1".to_owned(),
        profile_arn: "arn:aws:codewhisperer:us-east-1:1:profile/P".to_owned(),
        label: label.to_owned(),
    };
    let id = creds::account_id(&credential);
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
    let home = dir.path().join("agent");
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
    let id = seed(
        &host,
        "old-access",
        "kiro-refresh-1",
        "1970-01-01T00:00:01Z",
        "k@example.test",
    );
    host.http().enqueue(
        br#"{"accessToken":"new-access","refreshToken":"new-refresh","expiresIn":3600,"profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/P"}"#.to_vec(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let access = SecretObject::parse(&secret)
        .expect("blob")
        .secret("access_token")
        .expect("access");
    assert_eq!(secret_text(&access), "new-access");
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
    host.http().enqueue(
        br#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-sonnet-4.5","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#.to_vec(),
    );
    let id = seed(
        &host,
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
    assert!(call.uri().contains("amazonaws.com"));
}

#[test]
fn a2b_25_usage_parsing() {
    let body = br#"{"usageBreakdownList":[{"displayName":"Credits","currentUsageWithPrecision":40,"usageLimitWithPrecision":100}]}"#;
    let windows = crate::quota::windows(body).expect("windows");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].name(), "Credits");
    assert_eq!(windows[0].used(), 40);
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
    };
    assert!(!format!("{credential:?}").contains(token));
}

#[test]
fn a2b_27_agent_sign_in_writes_token_file() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed(
        &host,
        "agent-access",
        "agent-refresh",
        "2099-01-01T00:00:00Z",
        "k@example.test",
    );
    let home = dir.path().join("kiro-home");
    block_on(sub.write_agent_sign_in(&id, &home, host.context())).expect("write");
    let path = creds::default_token_path(&home);
    let body = std::fs::read_to_string(path).expect("read");
    assert!(body.contains("agent-access"));
    assert!(body.contains("us-east-1"));
}
