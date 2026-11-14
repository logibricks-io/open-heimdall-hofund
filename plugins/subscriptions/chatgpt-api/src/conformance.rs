//! Lifecycle checks through the public entry point.

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, SignInFlowKind, SubscriptionFactory,
};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("chatgpt-api"),
        dir.path(),
    )
}

#[test]
fn contributes_one_pkce_subscription() {
    let contribution = crate::contribute();
    assert!(contribution.upstreams().is_empty());
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription factory");
    };
    assert_eq!(factory.spec().plugin().as_str(), "chatgpt-api");
    assert_eq!(factory.spec().flow(), SignInFlowKind::Pkce);

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    assert_eq!(built.id().as_str(), "chatgpt-api");
    assert_eq!(built.flow().kind(), SignInFlowKind::Pkce);
}

#[test]
fn unknown_account_needs_sign_in_and_sends_nothing() {
    let contribution = crate::contribute();
    let factory = &contribution.subscriptions()[0];
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    let account = AccountId::from_static("a-00000000000000ff");

    let status = block_on(built.account_status(&account, host.context())).expect("status");
    assert!(
        matches!(status, AccountStatus::NeedsSignIn { .. }),
        "{status:?}"
    );
    let quota = block_on(built.quota(&account, host.context())).expect("quota message");
    assert_eq!(quota.len(), 1);
    assert!(!quota[0].blocking());
    let models = block_on(built.list_models(&account, host.context())).expect_err("models");
    assert_eq!(models.kind(), PluginErrorKind::Auth);
    let refresh = block_on(built.refresh(&account, host.context())).expect_err("refresh");
    assert_eq!(refresh.kind(), PluginErrorKind::Auth);

    let upstream = built.upstream(&account, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "gpt-4.1").expect("query");
    let probe = block_on(upstream.probe(&query)).expect_err("no sign-in");
    assert_eq!(probe.kind(), PluginErrorKind::Auth);
    assert!(block_on(built.health()).is_ok());
    assert!(host.http().calls().is_empty());
}
