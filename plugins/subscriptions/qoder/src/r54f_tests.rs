//! Routing labels `qoder` and `qoder-cn` on one store.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{
    PluginId, PluginKind, RawTable, SignInChallenge, SignInCtx, Subscription, SubscriptionFactory,
};

use crate::creds::{self, Credential, Site};
use crate::{Endpoints, factory_with_endpoints};

fn host(dir: &tempfile::TempDir) -> TestHost {
    TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("qoder"),
        dir.path(),
    )
}

fn subscription(host: &TestHost) -> Arc<dyn Subscription> {
    let factory = factory_with_endpoints(Endpoints::default());
    let cfg = factory.validate(&RawTable::new()).expect("config");
    factory.build(&cfg, host.context()).expect("build")
}

fn seed_site(host: &TestHost, site: Site, uid: &str) -> hofund_plugin_api::AccountId {
    let credential = Credential {
        uid: uid.to_owned(),
        email: format!("{uid}@example.test"),
        name: String::new(),
        token: hofund_plugin_api::SecretString::from("token".to_owned()),
        refresh_token: hofund_plugin_api::SecretString::from("refresh".to_owned()),
        expires_at: 9_000_000_000_000,
        machine_id: "machine".to_owned(),
        site,
        vpc: if uid.contains("vpc") {
            "acme.vpc.qoder.com.cn".to_owned()
        } else {
            String::new()
        },
        device_chat: false,
        device_token: hofund_plugin_api::SecretString::from(String::new()),
        device_refresh_token: hofund_plugin_api::SecretString::from(String::new()),
        pat_auth: false,
        pat: hofund_plugin_api::SecretString::from(String::new()),
    };
    let id = creds::account_id(&credential);
    block_on(async {
        let accounts = host.context().accounts();
        let account = accounts
            .account(id.clone(), &credential.email)
            .expect("account");
        accounts
            .upsert(account, creds::secret_blob(&credential).to_secret())
            .await
            .expect("upsert");
    });
    id
}

fn ctx_for(label: &'static str) -> SignInCtx {
    SignInCtx::new(PluginId::from_static(label))
        .with_session("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .expect("session")
}

#[test]
fn r54f_serves_routing_label_splits_global_and_china() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let global = seed_site(&host, Site::Global, "global-uid");
    let china = seed_site(&host, Site::China, "china-uid");
    let vpc = seed_site(&host, Site::China, "vpc-uid");

    block_on(async {
        assert!(
            sub.serves_routing_label("qoder", &global, host.context())
                .await
                .expect("global on qoder")
        );
        assert!(
            !sub.serves_routing_label("qoder", &china, host.context())
                .await
                .expect("china off qoder")
        );
        assert!(
            !sub.serves_routing_label("qoder", &vpc, host.context())
                .await
                .expect("vpc off qoder")
        );
        assert!(
            !sub.serves_routing_label("qoder-cn", &global, host.context())
                .await
                .expect("global off qoder-cn")
        );
        assert!(
            sub.serves_routing_label("qoder-cn", &china, host.context())
                .await
                .expect("china on qoder-cn")
        );
        assert!(
            sub.serves_routing_label("qoder-cn", &vpc, host.context())
                .await
                .expect("vpc on qoder-cn")
        );
    });
}

#[test]
fn r54f_sign_in_default_site_follows_routing_label() {
    let dir = tempfile::tempdir().expect("dir");
    let host = host(&dir);
    let sub = subscription(&host);
    let challenge_cn =
        block_on(sub.start_sign_in(&ctx_for("qoder-cn"), host.context())).expect("china start");
    let challenge_global =
        block_on(sub.start_sign_in(&ctx_for("qoder"), host.context())).expect("global start");
    let SignInChallenge::OpenUrl { url } = challenge_cn else {
        panic!("open url expected");
    };
    assert!(
        url.contains("qoder.cn"),
        "qoder-cn label should open China web host: {url}"
    );
    let SignInChallenge::OpenUrl { url } = challenge_global else {
        panic!("open url expected");
    };
    assert!(
        url.contains("qoder.com"),
        "qoder label should open global web host: {url}"
    );
}

#[test]
fn r54f_site_option_overrides_routing_label() {
    assert_eq!(
        crate::routing::sign_in_site(
            &ctx_for("qoder")
                .with_option("site", "qoder-cn")
                .expect("site")
        ),
        Site::China
    );
}

#[test]
fn r54f_storage_plugin_id_stays_qoder_for_both_labels() {
    assert_eq!(hofund_plugin_registry::storage_id("qoder-cn"), "qoder");
    assert_eq!(hofund_plugin_registry::storage_id("qoder"), "qoder");
}

#[test]
fn r54f_registry_qoder_cn_resolves_qoder_factory() {
    let factory = hofund_plugin_registry::subscription("qoder-cn").expect("factory");
    assert_eq!(factory.spec().plugin().as_str(), "qoder");
}
