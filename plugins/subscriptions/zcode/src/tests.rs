//! Deterministic tests with a recording HTTP backend.

use std::path::PathBuf;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, SecretString, SignInChallenge, SignInCtx, SignInProof, SubscriptionFactory,
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

fn sign_in_ctx(site: &str) -> SignInCtx {
    SignInCtx::new(PluginId::from_static("zcode"))
        .with_session("00000000000000000000000000000001")
        .expect("session")
        .with_option("site", site)
        .expect("site")
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

fn write_team_agent_credentials(
    home: &std::path::Path,
    biz_token: &str,
    org: &str,
    project: &str,
    user_id: &str,
) {
    std::fs::create_dir_all(home.join("v2")).expect("dir");
    let secret = creds::secret_seed(home);
    let store = json!({
        "oauth:zai:access_token": creds::encrypt_for_test(&secret, biz_token),
        "oauth:zai:user_info": creds::encrypt_for_test(
            &secret,
            &format!(r#"{{"user_id":"{user_id}","email":"team@zcode.test","name":"Team"}}"#)
        ),
    });
    std::fs::write(home.join("v2").join("credentials.json"), store.to_string()).expect("write");
    let setting = json!({
        "providerFamilyDomain": "zai",
        "providerFamilyConnectionSelections": {
            "zai": {
                "kind": "team-coding-plan",
                "organizationId": org,
                "projectId": project
            }
        }
    });
    std::fs::write(home.join("v2").join("setting.json"), setting.to_string()).expect("write");
}

fn write_jwt_only_credentials(home: &std::path::Path, user_id: &str) {
    std::fs::create_dir_all(home.join("v2")).expect("dir");
    let secret = creds::secret_seed(home);
    let store = json!({
        "zcodejwttoken": creds::encrypt_for_test(&secret, "session.jwt.only.token"),
        "oauth:zai:user_info": creds::encrypt_for_test(
            &secret,
            &format!(r#"{{"user_id":"{user_id}","email":"jwt@zcode.test","name":"Jwt"}}"#)
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
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::Ready));
}

#[test]
fn a2d_17_refresh_reimports_rotated_team_business_token() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_team_agent_credentials(&home, "team-biz-old", "org-a", "proj-a", "u-team-refresh");
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    write_team_agent_credentials(&home, "team-biz-new", "org-a", "proj-a", "u-team-refresh");
    let sub = subscription(&host);
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("team_token").as_ref().map(secret_text),
        Some("team-biz-new")
    );
}

#[test]
fn a2d_18_business_api_401_reports_needs_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_team(
        &host,
        Site::Zai,
        ZAI_ANTHROPIC_BASE,
        "expired-biz",
        "t1",
        "tp1",
    );
    host.http().enqueue_response(
        401,
        &[],
        json!({"code": 401, "msg": "token expired"}).to_string(),
    );
    let sub = subscription(&host);
    let quota_error = block_on(sub.quota(&id, host.context())).expect_err("quota");
    assert_eq!(quota_error.kind(), PluginErrorKind::Auth);
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert!(blob.flag(creds::LAPSED));
    let calls_after_quota = host.http().calls().len();
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::NeedsSignIn { .. }));
    assert_eq!(host.http().calls().len(), calls_after_quota);
}

#[test]
fn a2d_23_team_status_is_offline_and_project_key_is_cached() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_team(
        &host,
        Site::Zai,
        ZAI_ANTHROPIC_BASE,
        "team-biz",
        "t1",
        "tp1",
    );
    let sub = subscription(&host);
    block_on(sub.account_status(&id, host.context())).expect("status 1");
    block_on(sub.account_status(&id, host.context())).expect("status 2");
    assert!(host.http().calls().is_empty());
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
            "data": {"limits": [{"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 1.0}]}
        })
        .to_string()
        .into_bytes(),
    );
    block_on(sub.quota(&id, host.context())).expect("quota 1");
    assert_eq!(host.http().calls().len(), 3);
    host.http().enqueue(
        json!({
            "success": true,
            "data": {"limits": [{"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 2.0}]}
        })
        .to_string()
        .into_bytes(),
    );
    block_on(sub.quota(&id, host.context())).expect("quota 2");
    assert_eq!(host.http().calls().len(), 4);
}

#[test]
fn r57b_start_plan_jwt_only_imports_and_serves_dressed_messages() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_jwt_only_credentials(&home, "u-jwt-only");
    assert_eq!(discover::list(&home).expect("list").len(), 1);
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("jwt only");
    host.http().enqueue(
        br#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"GLM-5.3-Flash","stop_reason":"end_turn"}"#
            .to_vec(),
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "GLM-5.3-Flash").expect("query");
    block_on(upstream.probe(&query)).expect("probe");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0]
            .uri()
            .starts_with("https://zcode.test/api/v1/zcode-plan/anthropic/v1/messages")
    );
    let headers = calls[0].sensitive_headers();
    for name in [
        "authorization",
        "x-zcode-app-version",
        "x-title",
        "x-session-id",
    ] {
        assert!(headers.contains(&name.to_owned()), "{name}: {headers:?}");
    }
    let body: hofund_plugin_sdk::json::Value =
        hofund_plugin_sdk::json::from_slice(calls[0].body()).expect("request body");
    assert_eq!(body["system"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        body["system"][0]["text"],
        "You are ZCode, an interactive agent"
    );
    assert!(
        body["system"][2]["text"]
            .as_str()
            .expect("dynamic prompt")
            .contains("You are powered by the model named zai-api/GLM-5.3-Flash.")
    );
    assert_eq!(body["messages"][0]["role"], "user");
    assert!(
        body["messages"][0]["content"][0]["text"]
            .as_str()
            .expect("date reminder")
            .contains("Today's date is")
    );
    assert_eq!(
        body["metadata"]["user_id"]
            .as_str()
            .map(|id| id.contains("session_id")),
        Some(true)
    );
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

#[test]
fn a2d_24_lapsed_team_seat_recovers_after_fresh_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_team_agent_credentials(&home, "team-biz-old", "org-a", "proj-a", "u-team-lapse");
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let sub = subscription(&host);
    host.http().enqueue_response(
        401,
        &[],
        json!({"code": 401, "msg": "token expired"}).to_string(),
    );
    let quota_error = block_on(sub.quota(&id, host.context())).expect_err("quota");
    assert_eq!(quota_error.kind(), PluginErrorKind::Auth);
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert!(
        SecretObject::parse(&secret)
            .expect("blob")
            .flag(creds::LAPSED)
    );
    write_team_agent_credentials(&home, "team-biz-new", "org-a", "proj-a", "u-team-lapse");
    let calls_before = host.http().calls().len();
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert!(!blob.flag(creds::LAPSED));
    assert_eq!(
        blob.secret("team_token").as_ref().map(secret_text),
        Some("team-biz-new")
    );
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::Ready));
    assert_eq!(host.http().calls().len(), calls_before);
}

#[test]
fn r57_refresh_keeps_lapsed_when_the_team_token_is_unchanged() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_team_agent_credentials(&home, "team-biz-same", "org-a", "proj-a", "u-team-same");
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let sub = subscription(&host);
    host.http().enqueue_response(
        401,
        &[],
        json!({"code": 401, "msg": "token expired"}).to_string(),
    );
    let error = block_on(sub.quota(&id, host.context())).expect_err("refused");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert!(
        SecretObject::parse(&secret)
            .expect("blob")
            .flag(creds::LAPSED)
    );
    let status = block_on(sub.account_status(&id, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::NeedsSignIn { .. }));
}

#[test]
fn r57_refresh_reimport_writes_through_the_account_lease() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_team_agent_credentials(&home, "team-biz-lease", "org-a", "proj-a", "u-team-lease");
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let lapsed = SecretObject::parse(&secret)
        .expect("blob")
        .with_flag(creds::LAPSED, true)
        .to_secret();
    block_on(
        host.context().accounts().upsert(
            host.context()
                .accounts()
                .account(id.clone(), "team@zcode.test (team)".to_owned())
                .expect("account"),
            lapsed,
        ),
    )
    .expect("mark lapsed");
    let lease = block_on(
        host.context()
            .accounts()
            .lease(&id, std::time::Duration::from_secs(1)),
    )
    .expect("lease");
    let held = lease.secret().clone();
    let worker = {
        let sub = subscription(&host);
        let context = host.context().clone();
        let id = id.clone();
        std::thread::spawn(move || block_on(sub.refresh(&id, &context)))
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!worker.is_finished(), "refresh bypassed the held lease");
    block_on(lease.commit(None, held)).expect("release lease");
    worker.join().expect("refresh thread").expect("refresh");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    assert!(
        SecretObject::parse(&secret)
            .expect("blob")
            .flag(creds::LAPSED)
    );
}

#[test]
fn r57_team_accounts_have_a_separate_label() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_team_agent_credentials(&home, "team-biz-label", "org-a", "proj-a", "u-team-label");
    let auth = creds::parse_agent_home(&home).expect("parse");
    assert_eq!(auth.label, "team@zcode.test (team)");
    let rows = discover::list(&home).expect("discover");
    assert_eq!(rows[0].label(), "team@zcode.test (team)");
}

#[test]
fn r57_3012_has_a_risk_control_hint_and_cooldown() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_plan_key(&host, "glm.plan.key.secret", "user@zcode.test");
    host.http().enqueue_response(
        405,
        &[],
        json!({
            "type": "error",
            "error": {
                "type": "permission_error",
                "code": "3012",
                "message": "request has been blocked due to unusual activity"
            }
        })
        .to_string(),
    );
    let upstream = subscription(&host)
        .upstream(&id, host.context())
        .expect("upstream");
    let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "glm-5.3").expect("query");
    let error = block_on(upstream.probe(&query)).expect_err("restricted");
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(405));
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(2 * 60 * 60))
    );
    assert!(error.message().contains("request has been blocked"));
    assert!(
        error
            .message()
            .contains("risk control blocked this account")
    );
    assert!(error.message().contains("Z Code support"));
    let calls = host.http().calls();
    assert_eq!(calls.len(), 1);
    let body = String::from_utf8_lossy(calls[0].body());
    assert!(!body.contains("hofund"));
    assert!(!body.contains(&("mag".to_owned() + "pie")));
}

#[test]
fn r57_other_405_errors_do_not_get_the_3012_hint() {
    let error = crate::http_auth::classify_upstream_error(
        hofund_plugin_api::PluginError::status_error(405, "method not allowed (30120)"),
    );
    assert_eq!(error.message(), "method not allowed (30120)");
    assert_eq!(error.retry_after(), None);
}

#[test]
fn r57b_team_key_is_created_with_the_reference_body_and_version() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let id = seed_team(
        &host,
        Site::Zai,
        ZAI_ANTHROPIC_BASE,
        "team-biz",
        "t1",
        "tp1",
    );
    host.http()
        .enqueue(json!({"code": 200, "data": []}).to_string().into_bytes());
    host.http().enqueue(
        json!({"code": 200, "data": {"apiKey": "new-key"}})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 200, "data": {"secretKey": "secret"}})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({
            "success": true,
            "data": {"limits": [{"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "percentage": 1.0}]}
        })
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    block_on(sub.quota(&id, host.context())).expect("quota");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[1].method(), "POST");
    assert_eq!(
        hofund_plugin_sdk::json::from_slice::<hofund_plugin_sdk::json::Value>(calls[1].body())
            .expect("body"),
        json!({"name": "zcode-team-api-key", "keyType": 2})
    );
    for call in calls {
        assert_eq!(
            call.header("user-agent"),
            Some("ZCode/3.17.2"),
            "{}",
            call.uri()
        );
    }
}

#[test]
fn r57b_start_balance_drives_quota_and_gift_models() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_jwt_only_credentials(&home, "u-jwt-balance");
    let host = host(&dir);
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let balance = json!({
        "code": 0,
        "data": {
            "server_time": 4_102_444_800_i64,
            "plans": [{"plan_id": "start", "name": "Start Plan", "status": "active", "starts_at": 1, "ends_at": 4_102_500_000_i64}],
            "balances": [{
                "plan_id": "start",
                "show_name": "GLM-5.3-Flash",
                "capabilities": ["model:glm-5.3-flash"],
                "total_units": 1000,
                "used_units": 250,
                "remaining_units": 750,
                "expires_at": 4_102_448_400_i64
            }]
        }
    });
    host.http().enqueue(balance.to_string().into_bytes());
    let sub = subscription(&host);
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert!(windows.iter().any(|window| window.used() == 25));
    host.http().enqueue(balance.to_string().into_bytes());
    let models = block_on(sub.list_models(&id, host.context())).expect("models");
    let ids: Vec<&str> = models.models().iter().map(|model| model.id()).collect();
    assert!(ids.contains(&"GLM-5.3-Flash"));
    assert!(ids.contains(&"GLM-5.3-Flash-Trial"));
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert!(call.uri().contains("app_version=3.17.2"));
        assert_eq!(call.header("user-agent"), Some("ZCode/3.17.2"));
        assert!(call.header("x-device-mid").is_some());
    }
}

#[test]
fn r57b_bigmodel_browser_sign_in_resolves_the_coding_plan() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    host.http().enqueue(
        json!({"code": 0, "data": {
            "flow_id": "flow-1",
            "authorize_url": "https://open.bigmodel.cn/login",
            "poll_interval_sec": 0,
            "expires_at": 4_102_444_800_i64
        }})
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 0, "data": {
            "status": "ready",
            "bigmodel": {"access_token": "site-token"},
            "token": "session.jwt.value",
            "user": {"user_id": "u-browser", "email": "browser@zcode.test"}
        }})
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 200, "data": {"organizations": [{
            "organizationId": "own-org",
            "organizationName": "default",
            "projects": [{"projectId": "own-project", "projectName": "default", "projectType": 1}]
        }]}})
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 200, "data": [{"name": "zcode-api-key", "apiKey": "own"}]})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 200, "data": {"secretKey": "own-secret"}})
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json!({"code": 200, "data": [{"status": "VALID", "productName": "GLM Coding Pro"}]})
            .to_string()
            .into_bytes(),
    );
    let sub = subscription(&host);
    let challenge =
        block_on(sub.start_sign_in(&sign_in_ctx("bigmodel"), host.context())).expect("start");
    let SignInChallenge::OpenUrl { url } = challenge else {
        panic!("open URL challenge");
    };
    assert!(url.contains("redirect="));
    assert!(!url.contains("redirect_uri="));
    assert!(url.contains("app_version%3D3.17.2") || url.contains("app_version=3.17.2"));
    let id = block_on(sub.complete_sign_in(
        &sign_in_ctx("bigmodel"),
        SignInProof::Await,
        host.context(),
    ))
    .expect("complete");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(blob.text("plan"), Some("GLM Coding Pro"));
    assert_eq!(blob.text("uid"), Some("u-browser"));
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("own.own-secret")
    );
    let calls = host.http().calls();
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[0].method(), "POST");
    assert!(calls[0].uri().ends_with("/api/v1/oauth/cli/init"));
    assert_eq!(
        hofund_plugin_sdk::json::from_slice::<hofund_plugin_sdk::json::Value>(calls[0].body())
            .expect("init body"),
        json!({"provider": "bigmodel"})
    );
    assert!(
        calls[0]
            .sensitive_headers()
            .contains(&"authorization".to_owned())
    );
    assert!(calls[1].uri().contains("/api/v1/oauth/cli/poll/flow-1"));
}

#[test]
fn r57b_browser_sign_in_rejects_an_insecure_authorize_url() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    host.http().enqueue(
        json!({"code": 0, "data": {
            "flow_id": "flow-1",
            "authorize_url": "http://open.bigmodel.cn/login"
        }})
        .to_string()
        .into_bytes(),
    );
    let sub = subscription(&host);
    let error = block_on(sub.start_sign_in(&sign_in_ctx("bigmodel"), host.context()))
        .expect_err("insecure");
    assert!(error.message().contains("no sign-in page"));
    assert_eq!(host.http().calls().len(), 1);
}
