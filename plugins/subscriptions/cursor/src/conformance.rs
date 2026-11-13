//! Lifecycle checks through the public entry point.

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProtocolId, RawTable, Request,
    SecretString, SignInFlowKind,
};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("cursor"),
        dir.path(),
    )
}

#[test]
fn a2a_26_contributes_one_pkce_subscription() {
    let contribution = crate::contribute();
    assert!(contribution.upstreams().is_empty());
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription factory");
    };
    assert_eq!(factory.spec().plugin().as_str(), "cursor");
    assert_eq!(factory.spec().flow(), SignInFlowKind::Pkce);

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    assert_eq!(built.id().as_str(), "cursor");
    assert_eq!(built.flow().kind(), SignInFlowKind::Pkce);
}

#[test]
fn a2a_27_unknown_account_needs_sign_in() {
    let contribution = crate::contribute();
    let factory = &contribution.subscriptions()[0];
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    let account = AccountId::from_static("a-00000000000000ff");

    let status = block_on(built.account_status(&account, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::NeedsSignIn { .. }));
    let quota = block_on(built.quota(&account, host.context())).expect_err("quota");
    assert_eq!(quota.kind(), PluginErrorKind::Auth);
    let upstream = built
        .upstream(&account, host.context())
        .expect("provider defers account authentication to execute");
    let error = block_on(upstream.execute(Request::new("auto"), ProtocolId::OPENAI_CHAT))
        .map(|_| ())
        .expect_err("unknown account cannot chat");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
}

#[test]
fn r8f3_known_account_has_an_upstream_provider() {
    let contribution = crate::contribute();
    let factory = &contribution.subscriptions()[0];
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    let account = AccountId::from_static("a-00000000000000f4");
    let stored = host
        .context()
        .accounts()
        .account(account.clone(), "known@example.test")
        .expect("account");
    let blob = crate::tokens::blob_oauth(
        &SecretString::from("known-account-token".to_owned()),
        &SecretString::from("known-refresh-token".to_owned()),
        4_000_000_000_000,
        "known@example.test",
        None,
    );
    block_on(host.context().accounts().upsert(stored, blob.to_secret())).expect("store sign-in");

    built
        .upstream(&account, host.context())
        .expect("signed-in account returns a provider");
}
