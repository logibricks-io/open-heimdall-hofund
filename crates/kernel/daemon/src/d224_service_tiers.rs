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
use hofund_codec::{Codec, OpenAiChat, OpenAiResponses};
use hofund_config::{CatalogSettings, UsageSettings};
use hofund_gateway::Gateway;
use hofund_model_catalog::{
    CatalogFetcher, EffectivePricer, FetchResponse, ListPriceLookup, ModelCatalog, Price,
};
use hofund_plugin_api::SecretString;
use hofund_plugin_api::{
    AccountId, BodyStream, Completion, MemberId, PluginError, PluginId, ProtocolId,
    SubscriptionFactory, UpstreamProvider,
};
use hofund_plugin_host::TestHost;
use hofund_plugin_sdk::oauth::SecretObject;
use hofund_provider::{Endpoint, OpenAiProtocol};
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
        realtime_url: String::new(),
        realtime_authorization: String::new(),
        alternates: Vec::new(),
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
            effort_models: None,
            effort_live: None,
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
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    );
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

const PUBLIC_API: &str = "https://api.openai.com/v1";

fn open_calling(
    protocol: &'static str,
    plugin_id: &'static str,
    member_id: Option<MemberId>,
    upstream: Arc<dyn UpstreamProvider>,
    models: &[&str],
) -> (hofund_router::Snapshot, Arc<crate::membership::Table>) {
    let key = member_id
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| protocol.to_owned());
    let adapter = ProviderAdapter::new(key.clone(), upstream);
    let provider = BuiltProvider {
        adapter: Arc::new(adapter),
        protocol,
        timeout: Duration::from_secs(60),
        plugin_id: plugin_id.to_owned(),
        preset_id: String::new(),
        fingerprint: String::new(),
        patience: None,
        kernel: false,
        base_url: String::new(),
        realtime_url: String::new(),
        realtime_authorization: String::new(),
        alternates: Vec::new(),
        key: None,
        key_id: String::new(),
        http_profile: hofund_plugin_host::HttpProfile::direct(),
        http_limits: hofund_plugin_host::HttpLimits::default(),
    };
    assert!(provider.base_url.is_empty());
    let mut built = BTreeMap::new();
    built.insert(key.clone(), provider);
    let mut routes = BTreeMap::new();
    for model in models {
        routes.insert(
            (*model).to_owned(),
            vec![PlannedHop {
                provider: key.clone(),
                subscription: None,
                member_id: member_id.clone(),
                at_once_group: String::new(),
                at_once: 0,
                rank: 0,
                wire_model: None,
                effort_models: None,
                effort_live: None,
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
    }
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(|| tokio::time::Instant::now().into_std()),
    ));
    open_with_table(&built, routes, FailoverPolicy::default(), cooldowns).expect("snapshot")
}

fn public_endpoint() -> Endpoint {
    let mut endpoint = Endpoint::new("public", PUBLIC_API);
    endpoint.preset = Some("openai".to_owned());
    endpoint.api_key = "test-key".to_owned();
    endpoint
}

fn priced(dir: &TempDir) -> (Arc<Ledger>, Arc<EffectivePricer>) {
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
    (usage, pricer)
}

async fn ask(
    snapshot: &hofund_router::Snapshot,
    usage: &Arc<Ledger>,
    pricer: &Arc<EffectivePricer>,
    path: &str,
    model: &str,
    tier: &str,
) {
    send_tier(snapshot, usage, pricer, path, model, tier, true).await;
}

async fn send_tier(
    snapshot: &hofund_router::Snapshot,
    usage: &Arc<Ledger>,
    pricer: &Arc<EffectivePricer>,
    path: &str,
    model: &str,
    tier: &str,
    stream: bool,
) {
    let stream_word = if stream { "true" } else { "false" };
    let body = if path.ends_with("/chat/completions") {
        format!(
            r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"stream":{stream_word},"service_tier":"{tier}"}}"#
        )
    } else {
        format!(
            r#"{{"model":"{model}","input":"hi","stream":{stream_word},"service_tier":"{tier}"}}"#
        )
    };
    let response = router_for(snapshot, Arc::clone(usage), Arc::clone(pricer))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
}

fn tier_named(json: &serde_json::Value, model: &str, id: &str) -> bool {
    json["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|row| row["id"] == model)
        .expect(model)
        .get("service_tiers")
        .and_then(|tiers| tiers.as_array())
        .is_some_and(|tiers| {
            tiers
                .iter()
                .any(|tier| tier["id"] == id && (id != "ultrafast" || tier["name"] == "Ultrafast"))
        })
}

#[cfg(test)]
#[tokio::test]
async fn d224_openai_chat_key_downgrades_ultrafast() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Chat,
        public_endpoint(),
        host.context(),
    )
    .expect("chat upstream");
    let (snapshot, table) = open_calling("openai-chat", "openai", None, upstream, &["gpt-6.2-sol"]);
    let hop = table
        .members()
        .into_iter()
        .next()
        .expect("member")
        .attempt();
    assert_eq!(hop.base_url, PUBLIC_API);
    assert_eq!(hop.protocol, "openai-chat");
    assert_eq!(hop.wire_protocol, "openai-chat");
    let (usage, pricer) = priced(&dir);
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/chat/completions",
        "gpt-6.2-sol",
        "ultrafast",
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_asked, "ultrafast");
    assert_eq!(row.tier_sent, "priority");
}

#[cfg(test)]
#[tokio::test]
async fn d224_openai_responses_key_keeps_short_list_ultrafast() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Responses,
        public_endpoint(),
        host.context(),
    )
    .expect("responses upstream");
    let (snapshot, table) = open_calling(
        "openai-responses",
        "openai",
        None,
        upstream,
        &["gpt-6.2-sol", "gpt-6-sol"],
    );
    let hop = table
        .members()
        .into_iter()
        .next()
        .expect("member")
        .attempt();
    assert_eq!(hop.base_url, PUBLIC_API);
    assert_eq!(hop.wire_protocol, "openai-responses");
    let (usage, pricer) = priced(&dir);
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
    assert!(tier_named(&json, "gpt-6.2-sol", "ultrafast"));
    assert!(!tier_named(&json, "gpt-6-sol", "ultrafast"));
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6.2-sol",
        "ultrafast",
    )
    .await;
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6-sol",
        "ultrafast",
    )
    .await;
    let rows = usage.rows().expect("rows");
    let sol = rows
        .iter()
        .find(|row| row.model == "gpt-6.2-sol")
        .expect("sol");
    assert_eq!(sol.tier_sent, "ultrafast");
    let plain = rows
        .iter()
        .find(|row| row.model == "gpt-6-sol")
        .expect("plain");
    assert_eq!(plain.tier_sent, "priority");
}

#[tokio::test]
async fn d224_chatgpt_api_subscription_uses_the_responses_host_rule() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("chatgpt-api"), dir.path());
    let subscription = hofund_plugin_subscription_chatgpt_api::factory_with_endpoints(
        hofund_plugin_subscription_chatgpt_api::Endpoints::default(),
    )
    .build(
        &hofund_plugin_subscription_chatgpt_api::ChatgptApiConfig,
        host.context(),
    )
    .expect("subscription");
    let account = AccountId::from_static("a-0000000000000001");
    let upstream = subscription
        .upstream(&account, host.context())
        .expect("upstream");
    let member = MemberId::from_parts(PluginId::from_static("chatgpt-api"), account.clone());
    let (snapshot, table) = open_calling(
        "subscription",
        "chatgpt-api",
        Some(member),
        upstream,
        &["gpt-6.2-sol", "gpt-6-sol"],
    );
    let hop = table
        .members()
        .into_iter()
        .next()
        .expect("member")
        .attempt();
    assert_eq!(hop.protocol, "subscription");
    assert_eq!(hop.wire_protocol, "openai-responses");
    assert_eq!(hop.base_url, PUBLIC_API);
    assert!(hop.subscription);
    let accounts = host.context().accounts();
    let row = accounts
        .account(account.clone(), "me@example.test")
        .expect("account");
    accounts
        .upsert(
            row,
            SecretObject::new()
                .with_secret("access_token", &SecretString::from("token".to_owned()))
                .with_number("expires_at", 4_000_000_000)
                .to_secret(),
        )
        .await
        .expect("stored sign-in");
    let (usage, pricer) = priced(&dir);
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
    let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert!(tier_named(&json, "gpt-6.2-sol", "ultrafast"));
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6.2-sol",
        "ultrafast",
    )
    .await;
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6-sol",
        "ultrafast",
    )
    .await;
    let rows = usage.rows().expect("rows");
    let sol = rows
        .iter()
        .find(|row| row.model == "gpt-6.2-sol")
        .expect("sol");
    assert_eq!(sol.tier_sent, "ultrafast");
    let plain = rows
        .iter()
        .find(|row| row.model == "gpt-6-sol")
        .expect("plain");
    assert_eq!(plain.tier_sent, "priority");
}

const SERVED_DEFAULT: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6.2-sol\",\"service_tier\":\"ultrafast\"}}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6.2-sol\",\"status\":\"completed\",\"service_tier\":\"default\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n\n",
);

const CHAT_SERVED_DEFAULT: &str = concat!(
    "data: {\"id\":\"chat_1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-6.2-sol\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}],\"service_tier\":\"default\"}\n\n",
    "data: {\"id\":\"chat_1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-6.2-sol\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1000000,\"completion_tokens\":0}}\n\n",
    "data: [DONE]\n\n",
);

const UNARY_SERVED_DEFAULT: &str = r#"{"id":"resp_1","model":"gpt-6.2-sol","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"service_tier":"default","usage":{"input_tokens":1000000,"output_tokens":0}}"#;

const CHAT_UNARY_SERVED_DEFAULT: &str = r#"{"id":"chat_1","object":"chat.completion","model":"gpt-6.2-sol","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"service_tier":"default","usage":{"prompt_tokens":1000000,"completion_tokens":0}}"#;

fn assert_list_price(row: &hofund_usage::Row) {
    assert_eq!(row.tier_asked, "ultrafast");
    assert_eq!(row.tier_served, "default");
    let price = row.price.expect("price");
    assert_eq!(price.input, 1.0);
    assert!((row.cost_usd.expect("cost") - 1.0).abs() < 1e-9);
}

#[tokio::test]
async fn d224_wire_fold_keeps_the_completed_served_tier() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    host.http().enqueue(SERVED_DEFAULT.as_bytes());
    let request = OpenAiResponses
        .decode_request(br#"{"model":"gpt-6.2-sol","input":"hi","stream":true}"#)
        .expect("request");
    let target = hofund_plugin_sdk::wire::Target::new(ProtocolId::OPENAI_RESPONSES, PUBLIC_API);
    let sent = hofund_plugin_sdk::wire::send(
        host.context().http(),
        &target,
        None,
        request,
        ProtocolId::OPENAI_RESPONSES,
    )
    .await
    .expect("sent");
    let Completion::Unary(response) = sent.fold().await.expect("folded").peel() else {
        panic!("folded reply");
    };
    assert_eq!(
        hofund_codec::service_tier::extension_tier(&response.extensions),
        "default"
    );
}

#[tokio::test]
async fn d224_chatgpt_api_unary_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("chatgpt-api"), dir.path());
    let subscription = hofund_plugin_subscription_chatgpt_api::factory_with_endpoints(
        hofund_plugin_subscription_chatgpt_api::Endpoints::default(),
    )
    .build(
        &hofund_plugin_subscription_chatgpt_api::ChatgptApiConfig,
        host.context(),
    )
    .expect("subscription");
    let account = AccountId::from_static("a-0000000000000001");
    let upstream = subscription
        .upstream(&account, host.context())
        .expect("upstream");
    let member = MemberId::from_parts(PluginId::from_static("chatgpt-api"), account.clone());
    let (snapshot, _table) = open_calling(
        "subscription",
        "chatgpt-api",
        Some(member),
        upstream,
        &["gpt-6.2-sol"],
    );
    let accounts = host.context().accounts();
    let row = accounts
        .account(account, "me@example.test")
        .expect("account");
    accounts
        .upsert(
            row,
            SecretObject::new()
                .with_secret("access_token", &SecretString::from("token".to_owned()))
                .with_number("expires_at", 4_000_000_000)
                .to_secret(),
        )
        .await
        .expect("stored sign-in");
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(SERVED_DEFAULT.as_bytes());
    send_tier(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6.2-sol",
        "ultrafast",
        false,
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "ultrafast");
    assert_list_price(&row);
}

#[cfg(test)]
#[tokio::test]
async fn d224_openai_chat_stream_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Chat,
        public_endpoint(),
        host.context(),
    )
    .expect("chat upstream");
    let (snapshot, _table) =
        open_calling("openai-chat", "openai", None, upstream, &["gpt-6.2-sol"]);
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(CHAT_SERVED_DEFAULT.as_bytes());
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/chat/completions",
        "gpt-6.2-sol",
        "ultrafast",
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "priority");
    assert_list_price(&row);
}

#[cfg(test)]
#[tokio::test]
async fn d224_openai_responses_unary_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Responses,
        public_endpoint(),
        host.context(),
    )
    .expect("responses upstream");
    let (snapshot, _table) = open_calling(
        "openai-responses",
        "openai",
        None,
        upstream,
        &["gpt-6.2-sol"],
    );
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(UNARY_SERVED_DEFAULT.as_bytes());
    send_tier(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6.2-sol",
        "ultrafast",
        false,
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "ultrafast");
    assert_list_price(&row);
}

#[cfg(test)]
#[tokio::test]
async fn d224_cross_protocol_stream_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Responses,
        public_endpoint(),
        host.context(),
    )
    .expect("responses upstream");
    let (snapshot, _table) = open_calling(
        "openai-responses",
        "openai",
        None,
        upstream,
        &["gpt-6.2-sol"],
    );
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(SERVED_DEFAULT.as_bytes());
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/chat/completions",
        "gpt-6.2-sol",
        "ultrafast",
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "ultrafast");
    assert_list_price(&row);
}

#[cfg(test)]
#[tokio::test]
async fn d224_openai_chat_unary_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Chat,
        public_endpoint(),
        host.context(),
    )
    .expect("chat upstream");
    let (snapshot, _table) =
        open_calling("openai-chat", "openai", None, upstream, &["gpt-6.2-sol"]);
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(CHAT_UNARY_SERVED_DEFAULT.as_bytes());
    send_tier(
        &snapshot,
        &usage,
        &pricer,
        "/v1/chat/completions",
        "gpt-6.2-sol",
        "ultrafast",
        false,
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "priority");
    assert_list_price(&row);
}

#[cfg(test)]
#[tokio::test]
async fn d224_openai_responses_stream_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Responses,
        public_endpoint(),
        host.context(),
    )
    .expect("responses upstream");
    let (snapshot, _table) = open_calling(
        "openai-responses",
        "openai",
        None,
        upstream,
        &["gpt-6.2-sol"],
    );
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(SERVED_DEFAULT.as_bytes());
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6.2-sol",
        "ultrafast",
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "ultrafast");
    assert_list_price(&row);
}

#[cfg(test)]
#[tokio::test]
async fn d224_cross_protocol_unary_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("openai"), dir.path());
    let upstream = hofund_plugin_registry::openai_compat(
        OpenAiProtocol::Responses,
        public_endpoint(),
        host.context(),
    )
    .expect("responses upstream");
    let (snapshot, _table) = open_calling(
        "openai-responses",
        "openai",
        None,
        upstream,
        &["gpt-6.2-sol"],
    );
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(UNARY_SERVED_DEFAULT.as_bytes());
    send_tier(
        &snapshot,
        &usage,
        &pricer,
        "/v1/chat/completions",
        "gpt-6.2-sol",
        "ultrafast",
        false,
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "ultrafast");
    assert_list_price(&row);
}

#[tokio::test]
async fn d224_chatgpt_api_stream_bills_the_served_default() {
    let dir = TempDir::new().expect("tempdir");
    let host = TestHost::open(PluginId::from_static("chatgpt-api"), dir.path());
    let subscription = hofund_plugin_subscription_chatgpt_api::factory_with_endpoints(
        hofund_plugin_subscription_chatgpt_api::Endpoints::default(),
    )
    .build(
        &hofund_plugin_subscription_chatgpt_api::ChatgptApiConfig,
        host.context(),
    )
    .expect("subscription");
    let account = AccountId::from_static("a-0000000000000001");
    let upstream = subscription
        .upstream(&account, host.context())
        .expect("upstream");
    let member = MemberId::from_parts(PluginId::from_static("chatgpt-api"), account.clone());
    let (snapshot, _table) = open_calling(
        "subscription",
        "chatgpt-api",
        Some(member),
        upstream,
        &["gpt-6.2-sol"],
    );
    let accounts = host.context().accounts();
    let row = accounts
        .account(account, "me@example.test")
        .expect("account");
    accounts
        .upsert(
            row,
            SecretObject::new()
                .with_secret("access_token", &SecretString::from("token".to_owned()))
                .with_number("expires_at", 4_000_000_000)
                .to_secret(),
        )
        .await
        .expect("stored sign-in");
    let (usage, pricer) = priced(&dir);
    host.http().enqueue(SERVED_DEFAULT.as_bytes());
    ask(
        &snapshot,
        &usage,
        &pricer,
        "/v1/responses",
        "gpt-6.2-sol",
        "ultrafast",
    )
    .await;
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "ultrafast");
    assert_list_price(&row);
}
