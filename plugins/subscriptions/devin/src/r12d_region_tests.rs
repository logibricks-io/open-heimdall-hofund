//! R-12d: stored API server hosts and WINDSURF_API_SERVER_URL override.

use std::path::Path;
use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, Completion, PluginId, PluginKind, ProbeQuery, ProtocolId, RawTable, Request,
    SecretString, SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_plugin_sdk::protobuf::{FLAG_END, ProtoBuf, frame};

use crate::connect;
use crate::creds::{self, Credential};
use crate::{DevinFactory, discover, sign_in};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("devin"),
        dir.path(),
    )
}

fn seed(host: &TestHost, credential: &Credential, cli_path: Option<&Path>) -> AccountId {
    let id = creds::account_id(credential);
    let blob = match cli_path {
        Some(path) => creds::cli_secret_blob(credential, path),
        None => creds::secret_blob(credential),
    };
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), credential.label.clone())
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, blob.to_secret())
            .await
            .expect("upsert");
    });
    id
}

fn upstream(host: &TestHost, id: &AccountId) -> Arc<dyn UpstreamProvider> {
    let cfg = DevinFactory.validate(&RawTable::new()).expect("config");
    DevinFactory
        .build(&cfg, host.context())
        .expect("build")
        .upstream(id, host.context())
        .expect("upstream")
}

fn enqueue_chat(host: &TestHost) {
    host.http().enqueue_response(
        200,
        &[("content-type", "application/proto")],
        connect::testdata::MODEL_CONFIGS,
    );
    let text = ProtoBuf::new().string(3, "ok").finish();
    let body = [
        frame(0, &text).expect("text"),
        frame(FLAG_END, b"{}").expect("end"),
    ]
    .concat();
    host.http().enqueue(body);
}

fn chat_request() -> Request {
    let mut request = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "swe-2")
        .expect("query")
        .minimal_request();
    request.stream = true;
    request
}

fn status_body(email: &str) -> Vec<u8> {
    format!(
        r#"{{"userStatus":{{"email":"{email}","teamsTier":"TEAMS_TIER_DEVIN_PRO","planStatus":{{"planInfo":{{"planName":"Pro"}}}}}}}}"#
    )
    .into_bytes()
}

#[test]
fn r12d_eu_import_persists_host_and_uses_it_on_rpc() {
    let dir = tempfile::tempdir().expect("dir");
    let home = dir.path().join("agent-home");
    let path = home.join("devin").join("credentials.toml");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(
        &path,
        "windsurf_api_key = \"eu-key\"\napi_server_url = \"https://eu.codeium.com/\"\n",
    )
    .expect("credentials");
    let host = host(&dir);
    host.http()
        .enqueue_response(200, &[], status_body("eu@example.test"));
    let id =
        block_on(discover::import(&home, discover::REFERENCE, host.context())).expect("import");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("secret")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(blob.text("api_server"), Some("https://eu.codeium.com"));
    assert_eq!(blob.text("cli"), Some("true"));

    assert!(
        host.http().calls()[0]
            .uri()
            .starts_with("https://eu.codeium.com/")
    );
    enqueue_chat(&host);
    let completion =
        block_on(upstream(&host, &id).execute(chat_request(), ProtocolId::OPENAI_CHAT))
            .expect("execute");
    assert!(matches!(completion, Completion::Stream(_)));
    let calls = host.http().calls();
    assert!(calls[1].uri().starts_with("https://eu.codeium.com/"));
    assert!(calls[2].uri().starts_with("https://eu.codeium.com/"));
}

#[test]
fn r12d_windsurf_env_override_moves_rpc_without_rewriting_store() {
    let dir = tempfile::tempdir().expect("dir");
    creds::set_windsurf_override_for_tests(Some("https://env-override.test///"));
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            creds::set_windsurf_override_for_tests(None);
        }
    }
    let _guard = Guard;

    let host = host(&dir);
    let credential = Credential {
        key: SecretString::from("saved-key".to_owned()),
        api_server: "https://eu.codeium.com".to_owned(),
        devin_webapp: creds::DEFAULT_DEVIN_WEBAPP.to_owned(),
        devin_api: creds::DEFAULT_DEVIN_API.to_owned(),
        label: "eu@example.test".to_owned(),
        plan: String::new(),
        org_id: String::new(),
        team_id: String::new(),
    };
    let id = seed(&host, &credential, None);
    enqueue_chat(&host);
    drop(block_on(
        upstream(&host, &id).execute(chat_request(), ProtocolId::OPENAI_CHAT),
    ));
    assert_eq!(
        host.http().calls()[0].uri(),
        "https://env-override.test/exa.api_server_pb.ApiServerService/GetCliModelConfigs"
    );
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("secret")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(blob.text("api_server"), Some("https://eu.codeium.com"));
}

#[test]
fn r12d_parse_toml_trims_api_server_trailing_slashes() {
    let parsed = creds::parse_credentials_toml(
        "windsurf_api_key = \"k\"\napi_server_url = \"https://eu.codeium.com///\"\n",
    )
    .expect("parse");
    assert_eq!(parsed.api_server, "https://eu.codeium.com");
}

#[test]
fn r12d_agent_home_write_emits_api_server_url() {
    let dir = tempfile::tempdir().expect("dir");
    let agent_home = dir.path().join("opencode");
    let host = host(&dir);
    let credential = Credential {
        key: SecretString::from("write-key".to_owned()),
        api_server: "https://eu.codeium.com".to_owned(),
        devin_webapp: "eu.app.devin.ai".to_owned(),
        devin_api: "https://api.eu.devin.ai".to_owned(),
        label: "eu@example.test".to_owned(),
        plan: String::new(),
        org_id: String::new(),
        team_id: String::new(),
    };
    let id = seed(&host, &credential, None);
    let subscription = DevinFactory
        .build(
            &DevinFactory.validate(&RawTable::new()).expect("config"),
            host.context(),
        )
        .expect("build");
    block_on(subscription.write_agent_sign_in(&id, &agent_home, host.context())).expect("write");
    let written =
        std::fs::read_to_string(creds::agent_credentials_path(&agent_home)).expect("credentials");
    assert!(written.contains("api_server_url = \"https://eu.codeium.com\""));
    assert!(written.contains("devin_webapp_host = \"eu.app.devin.ai\""));
    assert!(written.contains("devin_api_url = \"https://api.eu.devin.ai\""));
}

#[test]
fn r12d_identity_lookup_honors_env_override() {
    let dir = tempfile::tempdir().expect("dir");
    creds::set_windsurf_override_for_tests(Some("https://identity-env.test"));
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            creds::set_windsurf_override_for_tests(None);
        }
    }
    let _guard = Guard;
    let host = host(&dir);
    host.http()
        .enqueue_response(200, &[], status_body("env@example.test"));
    let credential = Credential {
        key: SecretString::from("key".to_owned()),
        api_server: sign_in::SERVER.to_owned(),
        devin_webapp: creds::DEFAULT_DEVIN_WEBAPP.to_owned(),
        devin_api: creds::DEFAULT_DEVIN_API.to_owned(),
        label: "Devin".to_owned(),
        plan: String::new(),
        org_id: String::new(),
        team_id: String::new(),
    };
    block_on(sign_in::who_by_key(host.context(), &credential));
    assert_eq!(
        host.http().calls()[0].uri(),
        "https://identity-env.test/exa.seat_management_pb.SeatManagementService/GetUserStatus"
    );
}
