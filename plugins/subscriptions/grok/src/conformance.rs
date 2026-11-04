//! Lifecycle checks through the public entry point, against recording
//! backends. The shared subscription conformance harness is follow-up #293.

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, PluginId, PluginKind, ProbeQuery, ProtocolId,
    RawTable, RawValue, SignInFlowKind,
};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("grok"),
        dir.path(),
    )
}

#[test]
fn a2a_1_contributes_one_bridge_subscription() {
    let contribution = crate::contribute();
    assert!(contribution.upstreams().is_empty());
    assert!(contribution.surfaces().is_empty());
    let [factory] = contribution.subscriptions() else {
        panic!("one subscription factory");
    };
    assert_eq!(factory.spec().plugin().as_str(), "grok");
    assert_eq!(factory.spec().flow(), SignInFlowKind::Bridge);
    let agent = factory.spec().agent_sign_in().expect("agent sign-in");
    assert_eq!(agent.home_env(), "GROK_HOME");
    assert_eq!(agent.home_dir(), ".grok");

    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let built = factory
        .validate_and_build(&RawTable::new(), host.context())
        .expect("build");
    assert_eq!(built.id().as_str(), "grok");
    assert_eq!(built.flow().kind(), SignInFlowKind::Bridge);

    let mut named = RawTable::new();
    let program = if cfg!(windows) {
        r"C:\grok\bin\grok.exe"
    } else {
        "/opt/grok/bin/grok"
    };
    named.insert("command", RawValue::String(program.to_owned()));
    assert!(factory.validate_and_build(&named, host.context()).is_ok());

    let mut unknown = RawTable::new();
    unknown.insert("base", RawValue::String("https://example.test".to_owned()));
    assert!(
        factory
            .validate_and_build(&unknown, host.context())
            .is_err()
    );
}

#[test]
fn a2a_2_unknown_account_needs_sign_in_and_sends_nothing() {
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
    let quota = block_on(built.quota(&account, host.context())).expect_err("quota");
    assert_eq!(quota.kind(), PluginErrorKind::Auth);
    let models = block_on(built.list_models(&account, host.context())).expect_err("models");
    assert_eq!(models.kind(), PluginErrorKind::Auth);
    let refresh = block_on(built.refresh(&account, host.context())).expect_err("refresh");
    assert_eq!(refresh.kind(), PluginErrorKind::Auth);
    let upstream = built.upstream(&account, host.context()).expect("upstream");
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, "grok-4.7").expect("query");
    let probe = block_on(upstream.probe(&query)).expect_err("no sign-in");
    assert_eq!(probe.kind(), PluginErrorKind::Auth);
    assert!(block_on(built.health()).is_ok());
    assert!(host.http().calls().is_empty());
}
