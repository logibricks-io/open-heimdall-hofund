//! PAT exchange sign-in and lapsed refresh re-exchange (R-54c, row a3).

use std::sync::Arc;
use std::time::SystemTime;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, PluginId, PluginKind, RawTable, SecretString, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::json;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, secret_text};

use crate::creds::{self, Credential, Site};
use crate::pat;
use crate::{Endpoints, factory_with_endpoints};

const OPENAPI: &str = "https://qoder.test";
const OPENAPI_CN: &str = "https://qoder-cn.test";

fn endpoints() -> Endpoints {
    Endpoints {
        web_global: "https://qoder.test".to_owned(),
        web_cn: "https://qoder-cn.test".to_owned(),
        openapi_global: OPENAPI.to_owned(),
        openapi_cn: OPENAPI_CN.to_owned(),
        chat_global: "https://qoder-chat.test".to_owned(),
        chat_cn: "https://qoder-cn-chat.test".to_owned(),
    }
}

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for_at(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        dir.path(),
        SystemTime::UNIX_EPOCH,
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(endpoints());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn ctx_site(site: Option<&str>) -> SignInCtx {
    let mut ctx = SignInCtx::new(PluginId::from_static("qoder"))
        .with_session("0123456789abcdef0123456789abcdef")
        .expect("session");
    if let Some(site) = site {
        ctx = ctx.with_option("site", site).expect("site");
    }
    ctx
}

fn seed_pat(
    host: &TestHost,
    site: Site,
    expires_at: i64,
    pat: &str,
    with_pat_secret: bool,
) -> AccountId {
    let credential = Credential {
        uid: "user-re".to_owned(),
        email: "re@example.test".to_owned(),
        name: String::new(),
        token: SecretString::from("jt-old".to_owned()),
        refresh_token: SecretString::from("jrt-old".to_owned()),
        expires_at,
        machine_id: "machine-1".to_owned(),
        site,
        vpc: String::new(),
        device_chat: false,
        device_token: SecretString::from(String::new()),
        device_refresh_token: SecretString::from(String::new()),
        pat_auth: true,
        pat: SecretString::from(if with_pat_secret {
            pat.to_owned()
        } else {
            String::new()
        }),
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), creds::label(&credential))
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

#[test]
fn r54c_pat_format_validation_matches_reference() {
    assert!(pat::validate_format("pt-abc123").is_ok());
    let bad = pat::validate_format("invalid").expect_err("bad");
    assert!(bad.to_string().contains("pt-"));
    let empty = pat::validate_format("").expect_err("empty");
    assert!(empty.to_string().contains("pt-"));
}

#[test]
fn r54c_global_pat_sign_in_exchanges_on_openapi_host() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json::json!({
            "token": "jt-pat",
            "refresh_token": "jrt-pat",
            "expires_in": 86400000_i64
        })
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        json::json!({
            "id": "user-123",
            "email": "test@example.test",
            "name": "Test User"
        })
        .to_string()
        .into_bytes(),
    );
    let id = block_on(sub.complete_sign_in(
        &ctx_site(None),
        SignInProof::Token(SecretString::from("pt-test123".to_owned())),
        host.context(),
    ))
    .expect("sign in");
    let calls = host.http().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].uri().starts_with(OPENAPI));
    assert!(calls[0].uri().contains("/api/v1/jobToken/exchange"));
    assert!(calls[1].uri().contains("/api/v1/userinfo"));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert!(blob.flag("pat_auth"));
    assert_eq!(
        blob.secret("pat").as_ref().map(secret_text),
        Some("pt-test123")
    );
    assert_eq!(
        blob.secret("access_token").as_ref().map(secret_text),
        Some("jt-pat")
    );
    assert!(
        blob.secret("device_token")
            .as_ref()
            .map(secret_text)
            .is_none_or(str::is_empty)
    );
}

#[test]
fn r54c_cn_pat_sign_in_uses_cn_openapi_only() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json::json!({
            "token": "jt-cn",
            "refresh_token": "jrt-cn",
            "expires_in": 86400000_i64
        })
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        json::json!({ "id": "cn-user-1", "name": "CN User" })
            .to_string()
            .into_bytes(),
    );
    let id = block_on(sub.complete_sign_in(
        &ctx_site(Some("qoder-cn")),
        SignInProof::Token(SecretString::from("pt-cn123".to_owned())),
        host.context(),
    ))
    .expect("sign in");
    let calls = host.http().calls();
    assert!(calls.iter().all(|call| call.uri().starts_with(OPENAPI_CN)));
    assert!(!calls.iter().any(|call| call.uri().starts_with(OPENAPI)));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(blob.text("site").unwrap_or(""), creds::CN_SITE);
}

#[test]
fn r54c_pat_account_uses_email_as_label() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json::json!({ "access_token": "jt-g", "refreshToken": "jrt-g", "expires_in": 86400000_i64 })
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json::json!({ "id": "g-1", "email": "g@example.test", "name": "G" })
            .to_string()
            .into_bytes(),
    );
    let id = block_on(sub.complete_sign_in(
        &ctx_site(None),
        SignInProof::Token(SecretString::from("pt-g".to_owned())),
        host.context(),
    ))
    .expect("sign in");
    assert_eq!(id, account_id_from(&["qoder", "g-1"]));
    let entry = block_on(host.context().accounts().list())
        .expect("list")
        .into_iter()
        .find(|account| account.id() == &id)
        .expect("account");
    assert_eq!(entry.label(), "g@example.test");
}

#[test]
fn r54c_cn_pat_account_without_email_uses_uid() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json::json!({ "token": "jt-1", "refresh_token": "jrt-1", "expires_in": 86400000_i64 })
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json::json!({ "id": "same-user", "name": "Someone" })
            .to_string()
            .into_bytes(),
    );
    let id = block_on(sub.complete_sign_in(
        &ctx_site(Some("cn")),
        SignInProof::Token(SecretString::from("pt-x".to_owned())),
        host.context(),
    ))
    .expect("sign in");
    assert_eq!(id, account_id_from(&[creds::CN_SITE, "same-user"]));
}

#[test]
fn r54c_pat_sign_in_fails_when_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http()
        .enqueue_response(400, &[], b"Bad request".to_vec());
    let err = block_on(sub.complete_sign_in(
        &ctx_site(None),
        SignInProof::Token(SecretString::from("pt-bad".to_owned())),
        host.context(),
    ))
    .expect_err("refused");
    assert!(err.to_string().contains("refused the PAT"));
}

#[test]
fn r54c_pat_sign_in_fails_without_user_id() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    host.http().enqueue(
        json::json!({ "token": "jt-x", "refresh_token": "jrt-x", "expires_in": 86400000_i64 })
            .to_string()
            .into_bytes(),
    );
    host.http().enqueue(
        json::json!({ "email": "no-id@example.test" })
            .to_string()
            .into_bytes(),
    );
    let err = block_on(sub.complete_sign_in(
        &ctx_site(None),
        SignInProof::Token(SecretString::from("pt-no-id".to_owned())),
        host.context(),
    ))
    .expect_err("no uid");
    assert!(err.to_string().contains("user id"));
}

#[test]
fn r54c_lapsed_job_refresh_re_exchanges_stored_pat() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_pat(&host, Site::Global, 1, "pt-reexchange", true);
    host.http()
        .enqueue_response(401, &[], br#"{"message":"refresh token expired"}"#.to_vec());
    host.http().enqueue(
        json::json!({
            "token": "jt-exchange-1",
            "refresh_token": "jrt-exchange-1",
            "expires_in": 86400000_i64
        })
        .to_string()
        .into_bytes(),
    );
    host.http().enqueue(
        json::json!({ "id": "user-re", "email": "re@example.test" })
            .to_string()
            .into_bytes(),
    );
    block_on(sub.refresh(&id, host.context())).expect("refresh");
    let calls = host.http().calls();
    assert!(calls[0].uri().contains("/jobToken/refresh"));
    assert!(
        calls
            .iter()
            .any(|call| call.uri().contains("/jobToken/exchange"))
    );
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("access_token").as_ref().map(secret_text),
        Some("jt-exchange-1")
    );
    assert_eq!(
        blob.secret("pat").as_ref().map(secret_text),
        Some("pt-reexchange")
    );
}

#[test]
fn r54c_lapsed_refresh_without_stored_pat_surfaces_expired() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_pat(&host, Site::Global, 1, "pt-reexchange", false);
    host.http()
        .enqueue_response(401, &[], br#"{"message":"refresh token expired"}"#.to_vec());
    let err = block_on(sub.refresh(&id, host.context())).expect_err("expired");
    assert!(err.to_string().contains("sign-in has expired"));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(
        blob.secret("access_token").as_ref().map(secret_text),
        Some("jt-old")
    );
    assert!(!blob.flag(creds::LAPSED));
}

#[test]
fn r54c_refused_pat_re_exchange_lapses_sign_in() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = seed_pat(&host, Site::Global, 1, "pt-reexchange", true);
    host.http()
        .enqueue_response(401, &[], br#"{"message":"refresh token expired"}"#.to_vec());
    host.http()
        .enqueue_response(401, &[], br#"{"message":"PAT expired"}"#.to_vec());
    let err = block_on(sub.refresh(&id, host.context())).expect_err("lapsed");
    assert!(err.to_string().contains("sign in again"));
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert!(blob.flag(creds::LAPSED));
}
