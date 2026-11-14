//! R-51d: API-key sign-in and usage-window parity (recorded HTTP only).

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    PluginId, PluginKind, RawTable, SecretString, SignInCtx, SignInProof, Subscription,
    SubscriptionFactory,
};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

use crate::{Endpoints, factory_with_endpoints};

const KEY: &str = "ksk_test_not_a_real_key";

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

fn sign_in_ctx() -> SignInCtx {
    SignInCtx::new(PluginId::from_static("kiro"))
        .with_session("0123456789abcdef0123456789abcdef")
        .expect("session")
}

#[test]
fn r51d_a3_token_sign_in_stores_apikey_method() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = block_on(sub.complete_sign_in(
        &sign_in_ctx(),
        SignInProof::Token(SecretString::from(KEY.to_owned())),
        host.context(),
    ))
    .expect("sign in");
    let secret = block_on(host.context().accounts().secret(&id))
        .expect("read")
        .expect("stored");
    let blob = SecretObject::parse(&secret).expect("blob");
    assert_eq!(blob.text("method"), Some("apikey"));
    assert_eq!(
        blob.secret("access_token").as_ref().map(secret_text),
        Some(KEY)
    );
    assert_eq!(blob.text("region"), Some("us-east-1"));
    assert!(blob.text("profile_arn").is_none_or(str::is_empty));
}

#[test]
fn r51d_a3_rejects_non_ksk_prefix() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let err = block_on(sub.complete_sign_in(
        &sign_in_ctx(),
        SignInProof::Token(SecretString::from("not-a-key".to_owned())),
        host.context(),
    ))
    .expect_err("reject");
    assert!(err.to_string().contains("ksk_"));
    assert!(!err.to_string().contains("not-a-key"));
}

#[test]
fn r51d_k_apikey_quota_uses_control_plane_post() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let id = block_on(sub.complete_sign_in(
        &sign_in_ctx(),
        SignInProof::Token(SecretString::from(KEY.to_owned())),
        host.context(),
    ))
    .expect("sign in");
    host.http().enqueue(
        br#"{"usageBreakdownList":[{"displayNamePlural":"Credits","currentUsageWithPrecision":10,"usageLimitWithPrecision":2000}]}"#
            .to_vec(),
    );
    let windows = block_on(sub.quota(&id, host.context())).expect("quota");
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].display(), "10 / 2000");
    let calls = host.http().calls();
    let call = calls.last().expect("call");
    assert_eq!(call.method(), "POST");
    assert!(call.uri().contains("origin=KIRO_CLI"));
    assert_eq!(
        call.header("x-amz-target"),
        Some("AmazonCodeWhispererService.GetUsageLimits")
    );
    assert_eq!(call.header("tokentype"), Some("API_KEY"));
}
