//! D-226: a Claude setup-token paste stays with the Claude subscription.

use std::path::Path;
use std::sync::Arc;

use hofund_plugin_api::{PluginId, PluginKind, RawTable};
use hofund_plugin_host::TestHost;
use hofund_plugin_sdk::oauth::SecretObject;

use crate::registry::{DaemonRegistry, RegistryAdapter};
use crate::sign_in::{Opened, accounts_import};

const TOKEN: &str = "sk-ant-oat01-daemon-import-token";

struct Built {
    hosts: Vec<TestHost>,
    opened: Vec<Opened>,
}

fn build(root: &Path, ids: &[&'static str]) -> Built {
    let mut hosts = Vec::new();
    let mut opened = Vec::new();
    for id in ids {
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static(id),
            root.join(id),
        );
        let instance = if *id == "antigravity" {
            antigravity(host.context())
        } else {
            let factory = RegistryAdapter
                .subscription(id)
                .unwrap_or_else(|| panic!("subscription factory {id}"));
            factory
                .validate_and_build(&RawTable::new(), host.context())
                .unwrap_or_else(|error| panic!("build {id}: {error}"))
        };
        opened.push(Opened {
            id: (*id).to_owned(),
            name: (*id).to_owned(),
            instance,
            host: host.context().clone(),
            config_dir: root.join(id),
        });
        hosts.push(host);
    }
    Built { hosts, opened }
}

fn antigravity(host: &hofund_plugin_api::HostContext) -> Arc<dyn hofund_plugin_api::Subscription> {
    use hofund_plugin_api::{RawValue, SubscriptionFactory};
    use hofund_plugin_subscription_antigravity::{Endpoints, factory_with_endpoints};

    let factory = factory_with_endpoints(Endpoints::default());
    let mut raw = RawTable::new();
    raw.insert("client_id", RawValue::String("test-client".to_owned()));
    raw.insert("client_secret", RawValue::String("test-secret".to_owned()));
    let config = factory.validate(&raw).expect("antigravity config");
    factory.build(&config, host).expect("antigravity")
}

fn assert_no_http(built: &Built) {
    for host in &built.hosts {
        let calls = host.http().calls();
        let uris: Vec<&str> = calls.iter().map(|call| call.uri()).collect();
        assert!(uris.is_empty(), "unexpected http {uris:?}");
    }
}

#[tokio::test]
async fn d226_setup_token_import_stays_on_claude_and_makes_no_token_request() {
    let dir = tempfile::tempdir().expect("dir");
    let pastes = [
        TOKEN.to_owned(),
        format!("CLAUDE_CODE_OAUTH_TOKEN={TOKEN}"),
        format!("export CLAUDE_CODE_OAUTH_TOKEN=\"{TOKEN}\""),
        format!("\"{TOKEN}\""),
    ];
    for (index, paste) in pastes.iter().enumerate() {
        let root = dir.path().join(index.to_string());
        std::fs::create_dir_all(&root).expect("root");
        let built = build(
            &root,
            &["antigravity", "claude", "cursor", "gemini", "qoder"],
        );
        let ids = accounts_import(&built.opened, paste).await.expect("import");
        assert_eq!(ids.len(), 1);
        assert_no_http(&built);
        let claude = built
            .hosts
            .iter()
            .zip(built.opened.iter())
            .find(|(_, opened)| opened.id == "claude")
            .expect("claude")
            .0;
        let listed = claude.context().accounts().list().await.expect("list");
        assert_eq!(listed.len(), 1);
        let secret = claude
            .context()
            .accounts()
            .secret(listed[0].id())
            .await
            .expect("read")
            .expect("stored");
        let blob = SecretObject::parse(&secret).expect("blob");
        assert_eq!(blob.text("setupToken"), Some(TOKEN));
        for host in &built.hosts {
            if host.context().plugin().as_str() == "claude" {
                continue;
            }
            let listed = host.context().accounts().list().await.expect("list");
            assert!(listed.is_empty());
        }
    }
}

#[tokio::test]
async fn d226_setup_token_import_without_claude_makes_no_request() {
    let dir = tempfile::tempdir().expect("dir");
    for ids in [
        &["gemini"][..],
        &["antigravity", "cursor", "gemini", "qoder"][..],
    ] {
        let built = build(dir.path(), ids);
        let error = accounts_import(&built.opened, TOKEN)
            .await
            .expect_err("no claude");
        let message = error.to_string();
        assert!(message.contains("Claude setup-token"), "{message}");
        assert!(
            message.contains("configure the Claude subscription"),
            "{message}"
        );
        assert!(!message.contains(TOKEN), "{message}");
        assert_no_http(&built);
        for host in &built.hosts {
            let listed = host.context().accounts().list().await.expect("list");
            assert!(listed.is_empty());
        }
    }
}
