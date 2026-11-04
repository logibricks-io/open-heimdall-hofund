//! Deterministic tests with a recording HTTP backend.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, SecretString,
    SignInChallenge, SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::creds::{self, PlanAuth, Site};
use crate::discover;
use crate::{Endpoints, ZAI_ANTHROPIC_BASE, factory_with_endpoints};

const SESSION: &str = "0123456789abcdef0123456789abcdef";

fn endpoints() -> Endpoints {
    Endpoints {
        device_authorize: "https://zcode.test/api/v1/oauth/device/code".to_owned(),
        device_token: "https://zcode.test/api/v1/oauth/device/token".to_owned(),
        device_page: "https://zcode.test/device".to_owned(),
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

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(PluginId::from_static("zcode"))
        .with_session(SESSION)
        .expect("session")
}

fn write_agent_credentials(home: &std::path::Path, key_plain: &str) {
    std::fs::create_dir_all(home.join("v2")).expect("dir");
    let secret = creds::secret_seed(home);
    let key_name =
        "account-provider:coding-plan:account:zai-individual-coding-plan:account:abc:api-key";
    let enc_key = creds::encrypt_for_test(&secret, key_plain);
    let store = json!({
        key_name: enc_key,
        "oauth:zai:user_info": creds::encrypt_for_test(
            &secret,
            r#"{"user_id":"u-1","email":"user@zcode.test","name":"User"}"#
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

#[test]
fn a2d_3_discover_import_fixture_credentials() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_agent_credentials(&home, "plan.key.secret");
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
}

#[test]
fn a2d_4_refresh_reimports_agent_credentials_on_jwt_expiry() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join(".zcode");
    write_agent_credentials(&home, "rotated.plan.key");
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
fn a2d_7_quota_parses_personal_resets() {
    let body = json!({
        "code": 0,
        "data": {
            "fiveHourResets": [{"available": true, "expireTime": "2099-01-01 12:00:00"}],
            "weekResets": []
        }
    });
    let windows = crate::quota::windows_from_resets(body.get("data").unwrap()).expect("parse");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].name(), "five hours");
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
fn a2d_9_device_sign_in_registers_account() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json!({
            "device_code": "dev-zcode",
            "user_code": "ZCDE-1234",
            "verification_uri": "https://zcode.test/device",
            "expires_in": 600,
            "interval": 1
        })
        .to_string()
        .into_bytes(),
    );
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::DeviceCode { user_code, .. } = challenge else {
        panic!("device challenge");
    };
    assert_eq!(user_code, "ZCDE-1234");
    host.http().enqueue(
        br#"{"access_token":"glm.jwt.token.with.dots","token_type":"Bearer","expires_in":3600}"#
            .to_vec(),
    );
    let id = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect("complete");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert!(blob.secret("api_key").is_some() || blob.secret("jwt").is_some());
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
