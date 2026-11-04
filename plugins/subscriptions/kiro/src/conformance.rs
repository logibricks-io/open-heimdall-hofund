//! Lifecycle checks through the public entry point.

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, SignInFlowKind,
};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("kiro"),
        dir.path(),
    )
}

#[test]
fn a2b_19_contributes_one_bridge_subscription() {
    let contribution = crate::contribute();
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription factory");
    };
    assert_eq!(factory.spec().plugin().as_str(), "kiro");
    assert_eq!(factory.spec().flow(), SignInFlowKind::Bridge);
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
}

#[test]
fn a2b_20_unknown_account_needs_sign_in_and_sends_nothing() {
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
    let upstream = built.upstream(&account, host.context()).expect("upstream");
    let query =
        ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude-sonnet-4.5").expect("query");
    let probe = block_on(upstream.probe(&query)).expect_err("no sign-in");
    assert_eq!(probe.kind(), PluginErrorKind::Auth);
    assert!(
        host.http().calls().is_empty(),
        "unknown account must not call the network"
    );
}
