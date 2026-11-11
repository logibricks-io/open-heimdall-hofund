//! Plan/SKU labels and quota entitlement refresh fixtures.

use std::sync::Arc;

use hofund_kit_subscription::{TestHost, block_on};
use hofund_plugin_api::{AccountId, PluginId, PluginKind, RawTable, SubscriptionFactory};
use hofund_plugin_sdk::json::json;
use hofund_plugin_sdk::oauth::SecretObject;

use crate::quota::{self, Entitlement};
use crate::{CopilotFactory, session};

fn entitlement(plan: &str, sku: &str) -> Entitlement {
    Entitlement {
        plan: (!plan.is_empty()).then(|| plan.to_owned()),
        sku: (!sku.is_empty()).then(|| sku.to_owned()),
    }
}

#[test]
fn entitlement_labels_match_github_clients() {
    for (plan, sku, want) in [
        ("individual", "free_educational_quota", "Education"),
        ("free", "free_educational_quota", "Education"),
        ("individual", "unknown-sku", "Pro"),
        ("individual", "free_limited_copilot", "Free"),
        ("individual_pro", "", "Pro+"),
        ("business", "", "Business"),
        ("individual_max", "free_github_star_quota", "Max"),
        ("individual_edu", "", "Education"),
        ("enterprise", "", "Enterprise"),
        ("free", "", "Free"),
        ("", "free_limited_copilot", "Free"),
        ("", "unknown-sku", ""),
        ("future_plan", "future-sku", "Future_plan"),
        ("", "", ""),
    ] {
        assert_eq!(entitlement(plan, sku).label(), want, "{plan} {sku}");
    }
}

fn seeded() -> (tempfile::TempDir, TestHost, AccountId) {
    let dir = tempfile::Builder::new()
        .prefix("hofund-r48-copilot-entitlement-")
        .tempdir()
        .expect("dir");
    let host = TestHost::open_for(
        PluginKind::Subscription,
        PluginId::from_static("copilot"),
        dir.path(),
    );
    let id = AccountId::from_static("a-0000000000000048");
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &"gho-entitlement".to_owned().into())
        .with_text("host", "github.com")
        .with_text("login", "octo")
        .to_secret();
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "octo")
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, blob)
            .await
            .expect("seed");
    });
    (dir, host, id)
}

fn subscription(host: &TestHost) -> Arc<dyn hofund_plugin_api::Subscription> {
    let cfg = CopilotFactory.validate(&RawTable::new()).expect("config");
    CopilotFactory.build(&cfg, host.context()).expect("build")
}

fn stored(host: &TestHost, id: &AccountId) -> SecretObject {
    let secret = block_on(host.context().accounts().secret(id))
        .expect("read")
        .expect("stored");
    SecretObject::parse(&secret).expect("blob")
}

fn education_payload() -> Vec<u8> {
    json!({
        "copilot_plan":"individual",
        "access_type_sku":"free_educational_quota",
        "quota_snapshots":{
            "chat":{"unlimited":true},
            "completions":{"unlimited":true},
            "premium_interactions":{"has_quota":true,"entitlement":200,"quota_remaining":200}
        }
    })
    .to_string()
    .into_bytes()
}

#[test]
fn quota_cards_show_sku_plan_and_keep_it_on_missing_or_failed_reads() {
    let (_dir, host, id) = seeded();
    let sub = subscription(&host);
    host.http().enqueue(education_payload());
    let windows = block_on(sub.quota(&id, host.context())).expect("education quota");
    let names: Vec<_> = windows.iter().map(|window| window.name()).collect();
    assert_eq!(
        names,
        [
            "Education · Premium requests",
            "Education · Chat requests",
            "Education · Completions"
        ]
    );
    assert_eq!(stored(&host, &id).text("plan"), Some("individual"));
    assert_eq!(
        stored(&host, &id).text("access_type_sku"),
        Some("free_educational_quota")
    );

    host.http().enqueue(
        json!({"quota_snapshots":{"premium_interactions":{"entitlement":200,"quota_remaining":150}}})
            .to_string()
            .into_bytes(),
    );
    let windows = block_on(sub.quota(&id, host.context())).expect("missing entitlement");
    assert_eq!(windows[0].name(), "Education · Premium requests");
    assert_eq!(windows[0].used(), 50);
    assert_eq!(stored(&host, &id).text("plan"), Some("individual"));

    host.http().enqueue_response(503, &[], b"busy".to_vec());
    block_on(sub.quota(&id, host.context())).expect_err("failed read");
    assert_eq!(stored(&host, &id).text("plan"), Some("individual"));
    assert_eq!(
        stored(&host, &id).text("access_type_sku"),
        Some("free_educational_quota")
    );
}

#[test]
fn delayed_entitlement_does_not_update_replaced_token() {
    let (_dir, host, id) = seeded();
    let old = block_on(session::stored(host.context(), &id)).expect("old credential");
    let replacement = SecretObject::new()
        .with_number("v", 1)
        .with_secret("oauth_token", &"gho-replacement".to_owned().into())
        .with_text("host", "github.com")
        .with_text("login", "octo")
        .with_text("plan", "free")
        .to_secret();
    block_on(async {
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "octo")
            .expect("account");
        host.context()
            .accounts()
            .upsert(account, replacement)
            .await
            .expect("replace");
    });
    block_on(session::update_entitlement(
        host.context(),
        &id,
        &old,
        &entitlement("individual_max", "free_github_star_quota"),
    ))
    .expect("stale update ignored");
    assert_eq!(stored(&host, &id).text("plan"), Some("free"));
    assert_eq!(stored(&host, &id).text("access_type_sku"), None);
}

#[test]
fn quota_usage_reference_shapes_keep_their_values() {
    let body = json!({
        "access_type_sku":"free_github_star_quota",
        "copilot_plan":"individual_max",
        "quota_snapshots":{
            "chat":{"unlimited":true},
            "completions":{"unlimited":true},
            "premium_interactions":{"entitlement":20000,"quota_remaining":19999.9}
        }
    });
    let windows = quota::windows(body.to_string().as_bytes()).expect("Max quota");
    assert_eq!(windows[0].name(), "Max · Premium requests");
    assert_eq!((windows[0].used(), windows[0].limit()), (0, Some(20000)));

    let body = json!({
        "access_type_sku":"free_limited_copilot",
        "copilot_plan":"free",
        "quota_snapshots":{
            "chat":{"percent_remaining":98.7,"entitlement":"200"},
            "completions":{"percent_remaining":100,"entitlement":"4000"},
            "premium_interactions":{"percent_remaining":0,"entitlement":"0"}
        }
    });
    let windows = quota::windows(body.to_string().as_bytes()).expect("Free quota");
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].name(), "Free · Chat requests");
    assert_eq!(windows[0].used(), 1);
    assert_eq!(windows[1].name(), "Free · Completions");
}
