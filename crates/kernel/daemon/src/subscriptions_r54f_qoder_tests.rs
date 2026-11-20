//! Qoder routing label expansion (recorded credentials only).

use std::sync::Arc;

use hofund_config::Config;
use hofund_plugin_api::{
    MemberId, PluginId, PluginKind, RawTable, Subscription, SubscriptionFactory,
};
use hofund_plugin_host::{self, memory_account_secrets};
use hofund_plugin_subscription_qoder::{Endpoints, factory_with_endpoints};
use hofund_router::{CooldownPolicy, Cooldowns};

use crate::subscriptions::{Subscriptions, build, expand};
use crate::{DaemonRegistry, RegistryAdapter, route_plan};

const GLOBAL_IMPORT: &str = r#"{
  "uid": "global-user",
  "email": "global@example.test",
  "token": "qoder-access-fixture-token",
  "refresh_token": "qoder-refresh-fixture-token",
  "expires_at": 9000000000000,
  "machine_id": "11111111-2222-4333-8444-555555555555"
}"#;

const CHINA_IMPORT: &str = r#"{
  "uid": "china-user",
  "email": "china@example.test",
  "token": "qoder-access-fixture-token",
  "refresh_token": "qoder-refresh-fixture-token",
  "expires_at": 9000000000000,
  "machine_id": "22222222-2222-4333-8444-555555555555",
  "site": "qoder-cn"
}"#;

async fn seed_qoder_accounts(dir: &std::path::Path) {
    let root = dir.join(".hofund");
    std::fs::create_dir_all(&root).expect("state dir");
    let host = hofund_plugin_host::runtime_host_for(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        String::new(),
        &root,
        hofund_plugin_host::HttpLimits::default(),
    )
    .expect("host");
    let factory = factory_with_endpoints(Endpoints {
        web_global: "https://qoder.test".to_owned(),
        web_cn: "https://qoder-cn.test".to_owned(),
        openapi_global: "https://qoder.test".to_owned(),
        openapi_cn: "https://qoder-cn.test".to_owned(),
        chat_global: "https://qoder-chat.test".to_owned(),
        chat_cn: "https://qoder-cn-chat.test".to_owned(),
    });
    let cfg = factory.validate(&RawTable::new()).expect("config");
    let sub: Arc<dyn Subscription> = factory.build(&cfg, &host).expect("build");
    sub.import_accounts(GLOBAL_IMPORT, &host)
        .await
        .expect("global import");
    sub.import_accounts(CHINA_IMPORT, &host)
        .await
        .expect("china import");
}

fn qoder_config(subscription_id: &str) -> Config {
    hofund_config::load(&format!(
        "[[providers]]\nid = \"plain\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"key\"\n\
         [[subscriptions]]\nid = \"{subscription_id}\"\naccounts = \"*\"\n\
         [[routes]]\nmodel = \"demo\"\nupstreams = [\"plain\", {{ subscription = \"{subscription_id}\", model = \"auto\" }}]\n"
    ))
    .expect("config")
}

#[test]
fn r54f_build_qoder_cn_subscription_entry() {
    let dir = tempfile::tempdir().expect("dir");
    let config = hofund_config::load("[[subscriptions]]\nid = \"qoder-cn\"\naccounts = \"*\"\n")
        .expect("config");
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let built = build(
        &config,
        &dir.path().join("hofund.toml"),
        Some(0),
        &registry,
        None,
        None,
    )
    .expect("build qoder-cn");
    let state = built.get("qoder-cn").expect("qoder-cn state");
    assert_eq!(state.host().plugin().as_str(), "qoder");
}

#[tokio::test]
async fn r54f_qoder_hop_expands_global_accounts_only() {
    let _secrets = memory_account_secrets();
    let dir = tempfile::tempdir().expect("dir");
    seed_qoder_accounts(dir.path()).await;
    let config = qoder_config("qoder");
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let subs: Subscriptions = build(
        &config,
        &dir.path().join("hofund.toml"),
        Some(0),
        &registry,
        None,
        None,
    )
    .expect("subscriptions");
    subs.get("qoder")
        .expect("qoder")
        .resolve(&Arc::new(Cooldowns::new(CooldownPolicy::default())))
        .await;
    let members: Vec<MemberId> = subs
        .get("qoder")
        .expect("qoder")
        .members()
        .into_iter()
        .map(|member| member.id)
        .collect();
    assert_eq!(members.len(), 1);
    assert!(
        members[0].to_string().starts_with("account:qoder:"),
        "{}",
        members[0]
    );
}

#[tokio::test]
async fn r54f_qoder_cn_hop_expands_china_accounts_only() {
    let _secrets = memory_account_secrets();
    let dir = tempfile::tempdir().expect("dir");
    seed_qoder_accounts(dir.path()).await;
    let config = qoder_config("qoder-cn");
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let subs: Subscriptions = build(
        &config,
        &dir.path().join("hofund.toml"),
        Some(0),
        &registry,
        None,
        None,
    )
    .expect("subscriptions");
    subs.get("qoder-cn")
        .expect("qoder-cn")
        .resolve(&Arc::new(Cooldowns::new(CooldownPolicy::default())))
        .await;
    let members: Vec<MemberId> = subs
        .get("qoder-cn")
        .expect("qoder-cn")
        .members()
        .into_iter()
        .map(|member| member.id)
        .collect();
    assert_eq!(members.len(), 1);
    assert!(
        members[0].to_string().starts_with("account:qoder-cn:"),
        "{}",
        members[0]
    );
}

#[tokio::test]
async fn r54f_route_expansion_uses_routing_label_on_member_ids() {
    let _secrets = memory_account_secrets();
    let dir = tempfile::tempdir().expect("dir");
    seed_qoder_accounts(dir.path()).await;
    let config = qoder_config("qoder-cn");
    let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
    let subs: Subscriptions = build(
        &config,
        &dir.path().join("hofund.toml"),
        Some(0),
        &registry,
        None,
        None,
    )
    .expect("subscriptions");
    subs.get("qoder-cn")
        .expect("qoder-cn")
        .resolve(&Arc::new(Cooldowns::new(CooldownPolicy::default())))
        .await;
    let (routes, _) = route_plan(&config, None, None, &Default::default());
    let (_, expanded) = expand(&Default::default(), &routes, &subs).expect("expand");
    let member_hops: Vec<_> = expanded["demo"]
        .iter()
        .filter_map(|hop| hop.member_id.clone())
        .collect();
    assert_eq!(member_hops.len(), 1);
    assert!(
        member_hops[0].to_string().starts_with("account:qoder-cn:"),
        "{}",
        member_hops[0]
    );
}
