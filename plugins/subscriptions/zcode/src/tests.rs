//! Deterministic tests with a recording HTTP backend.

use std::path::PathBuf;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable,
    SecretString, SubscriptionFactory,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, PlanAuth, Site};
use crate::discover;
use crate::{BIGMODEL_ANTHROPIC_BASE, Endpoints, ZAI_ANTHROPIC_BASE, factory_with_endpoints};

fn endpoints() -> Endpoints {
    Endpoints {
        portal: "https://zcode.test".to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("zcode"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> std::sync::Arc<dyn hofund_plugin_api::Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn state_device_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path()
        .join("state")
        .join("subscription")
        .join("zcode")
        .join("zcode-device-mid")
}

fn write_agent_credentials(home: &std::path::Path, key_plain: &str, user_id: &str) {
    std::fs::create_dir_all(home.join("v2")).expect("dir");
    let secret = creds::secret_seed(home);
    let key_name =
        "account-provider:coding-plan:account:zai-individual-coding-plan:account:abc:api-key";
    let enc_key = creds::encrypt_for_test(&secret, key_plain);
    let store = json!({
        key_name: enc_key,
        "oauth:zai:user_info": creds::encrypt_for_test(
            &secret,
            &format!(r#"{{"user_id":"{user_id}","email":"user@zcode.test","name":"User"}}"#)
        ),
    });
    std::fs::write(home.join("v2").join("credentials.json"), store.to_string()).expect("write");
}

fn seed_plan_key(host: &TestHost, key: &str, label: &str) -> AccountId {
    let auth = PlanAuth {
        api_key: Some(SecretString::from(key.to_owned())),
        jwt: None,
        team_token: None,
        org: String::new(),
        project: String::new(),
        base: ZAI_ANTHROPIC_BASE.to_owned(),
        site: Site::Zai,
        uid: "u-plan".to_owned(),
        label: label.to_owned(),
        follow_agent: false,
        agent_credentials: None,
    };
    let id = creds::account_id_for(&auth);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), label.to_owned())
            .expect("account");
        accounts
            .upsert(
                account,
                creds::secret_blob(&auth, "00000000-0000-4000-8000-000000000001").to_secret(),
            )
            .await
            .expect("upsert");
    });
    id
}

fn seed_team(
    host: &TestHost,
    site: Site,
    base: &str,
    token: &str,
    org: &str,
    project: &str,
) -> AccountId {
    let auth = PlanAuth {
        api_key: None,
        jwt: None,
        team_token: Some(SecretString::from(token.to_owned())),
        org: org.to_owned(),
        project: project.to_owned(),
        base: base.to_owned(),
        site,
        uid: "u-team".to_owned(),
        label: "team@zcode.test".to_owned(),
        follow_agent: false,
        agent_credentials: None,
    };
    let id = creds::account_id_for(&auth);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), auth.label.clone())
            .expect("account");
        accounts
            .upsert(
                account,
                creds::secret_blob(&auth, "00000000-0000-4000-8000-000000000001").to_secret(),
            )
            .await
            .expect("upsert");
    });
    id
}

fn jwt_with_exp(exp_secs: u64) -> SecretString {
    let middle = match exp_secs {
        1 => "eyJleHAiOjF9",
        4_102_444_800 => "eyJleHAiOjQxMDI0NDQ4MDB9",
        other => panic!("unsupported test jwt exp {other}"),
    };
    SecretString::from(format!("h.{middle}.s"))
}

#[test]
fn a2d_3_discover_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_agent_credentials(&home, "plan.key.secret", "u-1");
    let rows = discover::list(&home).expect("list");
    assert_eq!(rows.len(), 1);
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("plan.key.secret")
    );
    let device_path = state_device_file(&dir);
    assert!(device_path.starts_with(dir.path()));
    assert!(device_path.is_file());
}

#[test]
fn a2d_4_refresh_reimports_agent_credentials_on_jwt_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_agent_credentials(&home, "rotated.plan.key", "u-1");
    let host = host(&dir);
    let mut auth = creds::parse_agent_home(&home).expect("parse");
    auth.follow_agent = true;
    auth.agent_credentials = Some(home.join("v2").join("credentials.json"));
    auth.jwt = Some(SecretString::from("jwt.expired".to_owned()));
    auth.api_key = None;
    let id = creds::account_id_for(&auth);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), auth.label.clone())
            .expect("account");
        accounts
            .upsert(
                account,
                creds::secret_blob(&auth, "00000000-0000-4000-8000-000000000001").to_secret(),
            )
            .await
            .expect("upsert");
    });
    let sub = subscription(&host);
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("rotated.plan.key")
    );
    assert!(host.http().calls().is_empty());
}

#[test]
fn a2d_4b_refresh_rejects_jwt_expiry_without_follow_agent() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let past = 1_u64;
    let auth = PlanAuth {
        api_key: None,
        jwt: Some(jwt_with_exp(past)),
        team_token: None,
        org: String::new(),
        project: String::new(),
        base: ZAI_ANTHROPIC_BASE.to_owned(),
        site: Site::Zai,
        uid: "u-jwt".to_owned(),
        label: "jwt@zcode.test".to_owned(),
        follow_agent: false,
        agent_credentials: None,
    };
    let id = creds::account_id_for(&auth);
    block_on(async {
        host.context()
            .accounts()
            .upsert(
                host.context()
                    .accounts()
                    .account(id.clone(), auth.label.clone())
                    .expect("account"),
                creds::secret_blob(&auth, "00000000-0000-4000-8000-000000000001").to_secret(),
            )
            .await
            .expect("upsert");
    });
    let sub = subscription(&host);
    let error = block_on(sub.refresh(&id, host.context())).expect_err("expired");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
}

#[test]
fn a2d_4c_refresh_accepts_unexpired_jwt() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let future = 4_102_444_800_u64;
    let auth = PlanAuth {
        api_key: None,
        jwt: Some(jwt_with_exp(future)),
        team_token: None,
        org: String::new(),
        project: String::new(),
        base: ZAI_ANTHROPIC_BASE.to_owned(),
        site: Site::Zai,
        uid: "u-jwt".to_owned(),
        label: "jwt@zcode.test".to_owned(),
        follow_agent: false,
        agent_credentials: None,
    };
    let id = creds::account_id_for(&auth);
    let before = creds::secret_blob(&auth, "00000000-0000-4000-8000-000000000001").to_secret();
    block_on(async {
        host.context()
            .accounts()
            .upsert(
                host.context()
                    .accounts()
                    .account(id.clone(), auth.label.clone())
                    .expect("account"),
                before.clone(),
            )
            .await
            .expect("upsert");
    });
    let sub = subscription(&host);
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let after = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert_eq!(secret_text(&before), secret_text(&after));
}

#[test]
fn a2d_5_model_list_parsing() {
    let body = br#"{"data":[{"id":"glm-5.3"},{"id":"glm-5.3-flash"}]}"#;
    let models = crate::models::parse_list(body).expect("parse");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id(), "glm-5.3");
}

#[test]
fn a2d_6_upstream_round_trip_records_plan_key() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let key = "glm.plan.key.secret";
    let id = seed_plan_key(&host, key, "user@zcode.test");
    host.http().enqueue(
        br#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"glm-5.3","stop_reason":"end_turn"}"#
            .to_vec(),
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "glm-5.3").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    assert!(
        calls
            .iter()
            .any(|call| call.uri().contains("/v1/messages") && {
                call.sensitive_headers()
                    .contains(&"authorization".to_owned())
            })
    );
}

#[test]
fn a2d_7_quota_reads_personal_monitor_usage() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_plan_key(&host, "glm.plan.key.secret", "user@zcode.test");
    host.http().enqueue(
        json!({
            "success": true,
            "data": {
                "limits": [
                    {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 55.0, "nextResetTime": 1_758_800_000_000_i64}
                ]
            }
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].used(), 55);
    let calls = host.http().calls();
    let call = calls.last().expect("call");
    assert!(call.uri().contains("/api/monitor/usage/quota/limit"));
    assert!(!call.uri().contains("type=2"));
}

#[test]
fn a2d_8_secrets_not_in_debug_or_errors() {
    let key = "zcode-secret-plan-key";
    let auth = PlanAuth {
        api_key: Some(SecretString::from(key.to_owned())),
        jwt: None,
        team_token: None,
        org: String::new(),
        project: String::new(),
        base: ZAI_ANTHROPIC_BASE.to_owned(),
        site: Site::Zai,
        uid: String::new(),
        label: "user@zcode.test".to_owned(),
        follow_agent: false,
        agent_credentials: None,
    };
    let debug = format!("{auth:?}");
    assert!(!debug.contains(key));
    let error = creds::parse_agent_home(std::path::Path::new("/no/such/.zcode"));
    let text = error.expect_err("missing").to_string();
    assert!(!text.contains(key));
}

#[test]
fn a2d_10_refresh_is_no_op_for_plan_api_key() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_plan_key(&host, "static-plan-key", "user@zcode.test");
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    assert!(host.http().calls().is_empty());
}

#[test]
fn a2d_11_refresh_rejects_agent_account_mismatch() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_agent_credentials(&home, "rotated.plan.key", "u-1");
    let host = host(&dir);
    let mut auth = creds::parse_agent_home(&home).expect("parse");
    auth.follow_agent = true;
    auth.agent_credentials = Some(home.join("v2").join("credentials.json"));
    auth.jwt = Some(SecretString::from("jwt.expired".to_owned()));
    auth.api_key = None;
    let id = creds::account_id_for(&auth);
    let before = creds::secret_blob(&auth, "00000000-0000-4000-8000-000000000001").to_secret();
    block_on(async {
        host.context()
            .accounts()
            .upsert(
                host.context()
                    .accounts()
                    .account(id.clone(), auth.label.clone())
                    .expect("account"),
                before.clone(),
            )
            .await
            .expect("upsert");
    });
    write_agent_credentials(&home, "rotated.plan.key", "u-other");
    let sub = subscription(&host);
    let error = block_on(sub.refresh(&id, host.context())).expect_err("mismatch");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    let after = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert_eq!(secret_text(&before), secret_text(&after));
}

#[test]
fn a2d_12_team_zai_bearer_auth_and_monitor_quota() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_team(
        &host,
        Site::Zai,
        ZAI_ANTHROPIC_BASE,
        "zai-biz-token",
        "t1",
        "tp1",
    );
    host.http().enqueue(
        json!({"code": 200, "data": [{"name": "zcode-team-api-key", "apiKey": "tk", "keyType": 2}]})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 200, "data": {"secretKey": "tk.ts"}})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({
            "success": true,
            "data": {"limits": [{"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 12.0}]}
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].used(), 12);
    let calls = host.http().calls();
    assert!(calls[0].uri().contains("/api_keys"));
    assert!(
        calls[0]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
    assert!(
        calls[0]
            .headers()
            .iter()
            .any(|(name, value)| name == "bigmodel-organization" && value == "t1")
    );
    assert!(calls[1].uri().contains("/api_keys/copy/tk"));
    assert!(
        calls[1]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
    assert!(calls[2].uri().contains("type=2"));
    assert!(
        calls[2]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
}

#[test]
fn a2d_13_team_bigmodel_bare_auth_and_monitor_quota() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_team(
        &host,
        Site::Bigmodel,
        BIGMODEL_ANTHROPIC_BASE,
        "bm-tok",
        "o1",
        "p1",
    );
    host.http().enqueue(
        json!({"code": 200, "data": [{"name": "zcode-team-api-key", "apiKey": "pk", "keyType": 2}]})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 200, "data": {"secretKey": "pk.ps"}})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({
            "success": true,
            "data": {"limits": [{"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 33.0}]}
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    block_on(sub.quota(&id, host.context())).expect("quota");
    let calls = host.http().calls();
    assert!(
        calls[0]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
    assert!(
        calls[1]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
    assert!(calls[2].uri().contains("open.bigmodel.cn"));
    assert!(calls[2].uri().contains("type=2"));
    assert!(
        calls[2]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
}
