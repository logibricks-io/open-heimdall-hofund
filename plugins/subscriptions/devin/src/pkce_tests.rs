use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginError, PluginErrorKind, PluginId, PluginKind, RawTable, SecretString,
    SignInChallenge, SignInCtx, SignInProof, Subscription, SubscriptionFactory,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::{DevinFactory, sign_in};

const SESSION: &str = "0123456789abcdef0123456789abcdef";

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("devin"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let cfg = DevinFactory.validate(&RawTable::new()).expect("config");
    DevinFactory.build(&cfg, host.context()).expect("build")
}

fn ctx() -> SignInCtx {
    SignInCtx::new(PluginId::from_static("devin"))
        .with_session(SESSION)
        .expect("session")
}

fn started_url(host: &TestHost, sub: &Arc<dyn Subscription>) -> String {
    let challenge = block_on(sub.start_sign_in(&ctx(), host.context())).expect("start");
    let SignInChallenge::OpenUrl { url } = challenge else {
        panic!("expected an authorize URL");
    };
    url
}

fn parameter(url: &hofund_plugin_sdk::url::Url, key: &str) -> String {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("missing {key}"))
}

fn start(host: &TestHost, sub: &Arc<dyn Subscription>) -> hofund_plugin_sdk::url::Url {
    hofund_plugin_sdk::url::Url::parse(&started_url(host, sub)).expect("URL")
}

fn complete(
    host: &TestHost,
    sub: &Arc<dyn Subscription>,
    code: &str,
    state: &str,
) -> Result<AccountId, PluginError> {
    block_on(sub.complete_sign_in(
        &ctx(),
        SignInProof::Callback {
            code: SecretString::from(code.to_owned()),
            state: state.to_owned(),
            issued_client_id: None,
        },
        host.context(),
    ))
}

fn status(email: &str) -> Vec<u8> {
    format!(
        r#"{{"userStatus":{{"email":"{email}","teamsTier":"TEAMS_TIER_DEVIN_PRO","planStatus":{{"planInfo":{{"planName":"Pro"}}}}}}}}"#
    )
    .into_bytes()
}

fn accounts_empty(host: &TestHost) -> bool {
    block_on(host.context().accounts().list())
        .expect("accounts")
        .is_empty()
}

#[test]
fn authorize_uses_devin_loopback_pkce_and_cancel_clears_pending() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let url = start(&host, &sub);
    assert_eq!(url.as_str().split('?').next(), Some(sign_in::AUTHORIZE));
    let redirect = parameter(&url, "redirect_uri");
    let rest = redirect
        .strip_prefix("http://127.0.0.1:")
        .expect("IPv4 redirect");
    let (port, path) = rest.split_once('/').expect("callback path");
    assert_ne!(port.parse::<u16>().expect("port"), 0);
    assert_eq!(path, "callback");
    assert_eq!(parameter(&url, "prompt"), "select_account");
    assert_eq!(parameter(&url, "code_challenge_method"), "S256");
    assert_eq!(parameter(&url, "cli_pkce_marker"), "1");
    assert_eq!(parameter(&url, "state").len(), 32);
    assert_eq!(parameter(&url, "code_challenge").len(), 43);
    block_on(sub.cancel_sign_in(&ctx(), host.context())).expect("cancel");
    let error = complete(&host, &sub, "code", &parameter(&url, "state")).expect_err("cancelled");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(host.http().calls().is_empty());
}

#[test]
fn complete_without_start_and_non_callback_proofs_are_config_errors() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let error = complete(&host, &sub, "code", "state").expect_err("no pending");
    assert_eq!(error.kind(), PluginErrorKind::Config);

    let state = parameter(&start(&host, &sub), "state");
    let error = block_on(sub.complete_sign_in(&ctx(), SignInProof::Await, host.context()))
        .expect_err("wrong proof");
    assert_eq!(error.kind(), PluginErrorKind::Config);
    assert!(host.http().calls().is_empty());
    assert!(accounts_empty(&host));
    assert!(!state.is_empty());
}

#[test]
fn mismatched_state_and_missing_code_do_not_exchange_or_persist() {
    for missing_code in [false, true] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        let url = start(&host, &sub);
        let state = parameter(&url, "state");
        let error = complete(
            &host,
            &sub,
            if missing_code { "" } else { "code" },
            if missing_code { &state } else { "wrong-state" },
        )
        .expect_err("invalid callback");
        assert_eq!(
            error.kind(),
            if missing_code {
                PluginErrorKind::Config
            } else {
                PluginErrorKind::Auth
            }
        );
        assert!(host.http().calls().is_empty());
        assert!(accounts_empty(&host));
    }
}

#[test]
fn exchange_posts_json_then_persists_only_server_and_status_identity() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let url = start(&host, &sub);
    let state = parameter(&url, "state");
    let redirect = parameter(&url, "redirect_uri");
    host.http().enqueue_response(
        200,
        &[("content-type", "application/json")],
        br#"{"sessionToken":"devin-session-token$signed","devinApiUrl":"https://ignored-api.example","devinWebappHost":"ignored-web.example"}"#,
    );
    host.http()
        .enqueue_response(200, &[], status("browser@example.test"));
    let id = complete(&host, &sub, "callback-code", &state).expect("signed in");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].method(), "POST");
    assert_eq!(
        calls[0].uri(),
        "https://server.codeium.com/exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode"
    );
    assert_eq!(calls[0].header("content-type"), Some("application/json"));
    assert_eq!(calls[0].header("accept"), Some("application/json"));
    let body: Value = hofund_plugin_sdk::json::from_slice(calls[0].body()).expect("exchange JSON");
    assert_eq!(body["code"], "callback-code");
    assert_eq!(body["redirect_uri"], redirect);
    let verifier = body["code_verifier"].as_str().expect("verifier");
    assert_eq!(verifier.len(), 64);
    assert!(
        verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    );

    let secret = block_on(host.context().accounts().secret(&id))
        .expect("secret")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(blob.text("api_server"), Some(sign_in::SERVER));
    assert_eq!(blob.text("devin_api"), Some("https://ignored-api.example"));
    assert_eq!(blob.text("devin_webapp"), Some("ignored-web.example"));
    assert_eq!(blob.text("label"), Some("browser@example.test"));
    assert_eq!(blob.text("plan"), Some("Devin Pro"));
    assert_eq!(blob.text("cli"), None);
    assert_eq!(
        blob.secret("api_key").as_ref().map(secret_text),
        Some("devin-session-token$signed")
    );
    let accounts = block_on(host.context().accounts().list()).expect("accounts");
    assert_eq!(accounts[0].label(), "browser@example.test");
}

#[test]
fn exchange_failure_truncates_body_and_never_writes_an_account() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let url = start(&host, &sub);
    host.http().enqueue_response(400, &[], "z".repeat(400));
    let error = complete(&host, &sub, "code", &parameter(&url, "state")).expect_err("exchange");
    assert!(error.to_string().contains("Devin's exchange: 400"));
    assert_eq!(
        error.to_string().chars().filter(|ch| *ch == 'z').count(),
        300
    );
    assert_eq!(host.http().calls().len(), 1);
    assert!(accounts_empty(&host));
}

#[test]
fn missing_exchange_token_and_rejected_identity_write_nothing() {
    for rejected in [false, true] {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let sub = subscription(&host);
        let url = start(&host, &sub);
        host.http().enqueue_response(
            200,
            &[],
            if rejected {
                br#"{"session_token":"devin-session-token$rejected"}"#.as_slice()
            } else {
                br#"{"devinApiUrl":"https://ignored.example"}"#.as_slice()
            },
        );
        if rejected {
            host.http().enqueue_response(
                401,
                &[],
                br#"{"code":"resource_exhausted","message":"quota"}"#,
            );
        }
        let error =
            complete(&host, &sub, "code", &parameter(&url, "state")).expect_err("no persist");
        assert!(matches!(error.kind(), PluginErrorKind::Auth));
        assert!(accounts_empty(&host));
    }
}

#[test]
fn two_unknown_identity_sources_still_persist_with_generic_label() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let url = start(&host, &sub);
    host.http().enqueue_response(
        200,
        &[],
        br#"{"sessionToken":"devin-session-token$unknown"}"#,
    );
    host.http().enqueue_response(503, &[], b"unavailable");
    let id = complete(&host, &sub, "code", &parameter(&url, "state")).expect("generic sign-in");
    let accounts = block_on(host.context().accounts().list()).expect("accounts");
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id(), &id);
    assert_eq!(accounts[0].label(), "Devin");
}
