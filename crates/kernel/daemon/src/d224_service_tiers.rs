//! A subscription hop uses the backend its plugin calls.
//!
//! The provider row stores an empty base URL. The hop's attempt base comes
//! from the upstream, through the adapter, the same way
//! `SubscriptionMember::member` builds a member and `LiveMember` reads it.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use hofund_codec::{Codec, OpenAiResponses};
use hofund_config::{CatalogSettings, UsageSettings};
use hofund_gateway::Gateway;
use hofund_model_catalog::{
    CatalogFetcher, EffectivePricer, FetchResponse, ListPriceLookup, ModelCatalog, Price,
};
use hofund_plugin_api::{
    AccountId, BodyStream, Completion, MemberId, PluginError, PluginId, ProtocolId,
    UpstreamProvider,
};
use hofund_router::{CooldownPolicy, Cooldowns, FailoverPolicy};
use hofund_usage::Ledger;
use tempfile::TempDir;
use tower::ServiceExt;

use crate::adapt::ProviderAdapter;
use crate::membership::{BuiltProvider, PlannedHop, open_with_table};

/// Production backend the Codex plugin calls. The provider row does not store it.
const BACKEND: &str = "https://chatgpt.com/backend-api/codex";

const ANSWER: &str = concat!(
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_sub\",\"model\":\"gpt-6.2-sol\",\"status\":\"completed\",\"service_tier\":\"default\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n\n",
);

struct ReportingUpstream {
    base: &'static str,
}

#[async_trait]
impl UpstreamProvider for ReportingUpstream {
    fn attempt_base(&self) -> String {
        self.base.to_owned()
    }

    async fn execute(
        &self,
        _request: hofund_plugin_api::Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        Ok(Completion::Raw(BodyStream::from_bytes(Bytes::from_static(
            ANSWER.as_bytes(),
        ))))
    }
}

struct OfflineFetcher;

impl CatalogFetcher for OfflineFetcher {
    fn fetch(
        &self,
        _url: &str,
        _if_none_match: Option<&str>,
    ) -> Result<FetchResponse, std::io::Error> {
        Err(std::io::Error::other("offline"))
    }
}

struct SolList;

impl ListPriceLookup for SolList {
    fn list_price(&self, _provider_id: &str, model: &str) -> Option<Price> {
        (model == "gpt-6.2-sol").then_some(Price {
            input: 1.0,
            output: 4.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        })
    }
}

fn subscription_snapshot() -> (hofund_router::Snapshot, Arc<crate::membership::Table>) {
    let member = MemberId::from_parts(
        PluginId::from_static("codex"),
        AccountId::from_static("a-0000000000000001"),
    );
    let key = member.to_string();
    let adapter = ProviderAdapter::new(key.clone(), Arc::new(ReportingUpstream { base: BACKEND }));
    let provider = BuiltProvider {
        adapter: Arc::new(adapter),
        protocol: "subscription",
        timeout: Duration::from_secs(60),
        plugin_id: "codex".to_owned(),
        preset_id: String::new(),
        fingerprint: String::new(),
        patience: None,
        kernel: false,
        base_url: String::new(),
        key: None,
        key_id: String::new(),
        http_profile: hofund_plugin_host::HttpProfile::direct(),
        http_limits: hofund_plugin_host::HttpLimits::default(),
    };
    assert!(
        provider.base_url.is_empty(),
        "the provider row must not be what the hop reads"
    );
    let mut built = BTreeMap::new();
    built.insert(key.clone(), provider);
    let mut routes = BTreeMap::new();
    routes.insert(
        "gpt-6.2-sol".to_owned(),
        vec![PlannedHop {
            provider: key,
            subscription: None,
            member_id: Some(member),
            at_once_group: String::new(),
            at_once: 0,
            rank: 0,
            wire_model: None,
            max_output_tokens: None,
            context_window: None,
            routing: "smart".to_owned(),
            sink: false,
            patience: Some(Duration::from_secs(60)),
            weight: 1,
            free: false,
            stays: hofund_router::affinity::Stays::Auto,
            cold_after: hofund_router::affinity::CACHE_COLD,
            service_tier: String::new(),
            units: Vec::new(),
        }],
    );
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(|| tokio::time::Instant::now().into_std()),
    ));
    let (snapshot, table) =
        open_with_table(&built, routes, FailoverPolicy::default(), cooldowns).expect("snapshot");
    let hop = table
        .members()
        .into_iter()
        .next()
        .expect("subscription member")
        .attempt();
    assert_eq!(hop.protocol, "subscription");
    assert!(hop.subscription);
    assert_eq!(hop.base_url, BACKEND);
    (snapshot, table)
}

fn router_for(
    snapshot: &hofund_router::Snapshot,
    usage: Arc<Ledger>,
    pricer: Arc<EffectivePricer>,
) -> axum::Router {
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/responses".to_owned(),
        Arc::new(OpenAiResponses) as Arc<dyn Codec>,
    );
    Gateway::new(snapshot.clone(), "secret".to_owned(), codecs, usage)
        .with_effective_pricer(pricer)
        .into_router()
}

async fn post(
    snapshot: &hofund_router::Snapshot,
    usage: &Arc<Ledger>,
    pricer: &Arc<EffectivePricer>,
    tier: &str,
) {
    let response = router_for(snapshot, Arc::clone(usage), Arc::clone(pricer))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"model":"gpt-6.2-sol","input":"hi","stream":true,"service_tier":"{tier}"}}"#
                )))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
}

#[tokio::test]
async fn d224_subscription_hop_uses_the_backend_the_plugin_calls() {
    let dir = TempDir::new().expect("tempdir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("ledger"));
    let catalog = ModelCatalog::open(
        dir.path(),
        CatalogSettings::default(),
        Arc::new(OfflineFetcher),
    )
    .expect("catalog");
    let pricer = Arc::new(EffectivePricer::new(
        UsageSettings::default(),
        catalog,
        Arc::new(SolList),
    ));
    let (snapshot, table) = subscription_snapshot();
    let _table = table;

    let listed = router_for(&snapshot, Arc::clone(&usage), Arc::clone(&pricer))
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(listed.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let sol = json["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|row| row["id"] == "gpt-6.2-sol")
        .expect("gpt-6.2-sol");
    assert_eq!(sol["service_tiers"][0]["id"], "priority");
    assert_eq!(sol["service_tiers"][0]["name"], "Fast");
    assert_eq!(sol["service_tiers"][1]["id"], "ultrafast");
    assert_eq!(sol["service_tiers"][1]["name"], "Ultrafast");

    post(&snapshot, &usage, &pricer, "priority").await;
    post(&snapshot, &usage, &pricer, "flex").await;
    let rows = usage.rows().expect("rows");
    let priority = rows
        .iter()
        .find(|row| row.tier_asked == "priority")
        .expect("priority row");
    assert_eq!(priority.tier_sent, "priority");
    assert_eq!(priority.tier_served, "");
    let price = priority.price.expect("price");
    assert_eq!(price.input, 2.0);
    assert!((priority.cost_usd.expect("cost") - 2.0).abs() < 1e-9);

    let flex = rows
        .iter()
        .find(|row| row.tier_asked == "flex")
        .expect("flex row");
    assert_eq!(flex.tier_sent, "");
    assert_eq!(flex.tier_served, "");
    let price = flex.price.expect("price");
    assert_eq!(price.input, 1.0);
    assert!((flex.cost_usd.expect("cost") - 1.0).abs() < 1e-9);
}
