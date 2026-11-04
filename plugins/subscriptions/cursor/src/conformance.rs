//! Lifecycle checks through the public entry point.

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, RawTable, SignInFlowKind,
};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("cursor"),
        dir.path(),
    )
}

#[test]
fn a2a_20_contributes_one_pkce_subscription() {
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
fn a2a_21_unknown_account_needs_sign_in() {
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
    match built.upstream(&account, host.context()) {
        Err(error) => assert_eq!(error.kind(), PluginErrorKind::Unsupported),
        Ok(_) => panic!("upstream should be unsupported"),
    }
}
