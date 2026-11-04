//! Lifecycle checks.

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, RawTable, SignInFlowKind,
};

#[test]
fn a2c_20_contributes_bridge_with_checkin() {
    let contribution = crate::contribute();
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription factory");
    };
    assert_eq!(factory.spec().plugin().as_str(), "workbuddy");
    assert!(factory.spec().daily_checkin());
    assert_eq!(factory.spec().flow(), SignInFlowKind::Bridge);

    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("workbuddy"),
        dir.path(),
    );
    factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
}

#[test]
fn a2c_21_unknown_account_needs_sign_in() {
    let contribution = crate::contribute();
    let factory = &contribution.subscriptions()[0];
    let dir = tempfile::tempdir().expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("workbuddy"),
        dir.path(),
    );
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    let account = AccountId::from_static("a-00000000000000ff");
    let status = block_on(built.account_status(&account, host.context())).expect("status");
    assert!(matches!(status, AccountStatus::NeedsSignIn { .. }));
    let err = block_on(built.quota(&account, host.context())).expect_err("quota");
    assert_eq!(err.kind(), PluginErrorKind::Auth);
}
