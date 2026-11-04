//! Lifecycle checks through the public entry point.

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, SignInFlowKind,
};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("devin"),
        dir.path(),
    )
}

#[test]
fn a2b_1_contributes_one_bridge_subscription() {
    let contribution = crate::contribute();
    assert!(contribution.upstreams().is_empty());
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription factory");
    };
    assert_eq!(factory.spec().plugin().as_str(), "devin");
    assert_eq!(factory.spec().flow(), SignInFlowKind::Bridge);

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    assert_eq!(built.id().as_str(), "devin");
}

#[test]
fn a2b_2_unknown_account_needs_sign_in_and_sends_nothing() {
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
    let models = block_on(built.list_models(&account, host.context())).expect_err("models");
    assert_eq!(models.kind(), PluginErrorKind::Auth);
    let upstream = built.upstream(&account, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "swe-2").expect("query");
    let probe = block_on(upstream.probe(&query)).expect_err("no sign-in");
    assert_eq!(probe.kind(), PluginErrorKind::Auth);
    assert!(host.http().calls().is_empty());
}
