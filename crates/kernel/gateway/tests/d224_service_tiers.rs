//! `/v1/models` offers Ultrafast for a GPT Sol, Astra, or Terra route.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use futures_util::stream;
use hofund_codec::{Codec, OpenAiResponses};
use hofund_config::{CatalogSettings, UsageSettings};
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role};
use hofund_model_catalog::{
    CatalogFetcher, EffectivePricer, FetchResponse, ListPriceLookup, ModelCatalog, Price,
};
use hofund_plugin_api::BodyStream;
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, MemberAttempt, Router, Snapshot};
use hofund_usage::Ledger;
use tempfile::TempDir;
use tower::ServiceExt;

struct ListedMember {
    id: MemberId,
    base_url: &'static str,
}

#[async_trait]
impl Member for ListedMember {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "p"
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    fn attempt(&self) -> MemberAttempt {
        MemberAttempt {
            provider_id: self.route_id().to_owned(),
            protocol: self.protocol().to_owned(),
            base_url: self.base_url.to_owned(),
            ..MemberAttempt::default()
        }
    }

    async fn execute(&self, _: ModelRequest, _: &str) -> Result<Completion, PluginError> {
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }
}

fn member(key: &'static str, base_url: &'static str) -> Arc<dyn Member> {
    Arc::new(ListedMember {
        id: MemberId::from_preset(
            PresetId::try_from("p").expect("preset"),
            MemberKey::from_static(key),
        ),
        base_url,
    })
}

#[tokio::test]
async fn d224_models_list_offers_ultrafast_for_the_family() {
    let temp = TempDir::new().expect("tempdir");
    let usage = Arc::new(Ledger::open(&temp.path().join("usage.sqlite")).expect("ledger"));
    let mut pool = Hop::new(member("pool", ""));
    pool.wire_model = Some("gpt-6.1-terra".to_owned());
    pool.service_tier = "ultrafast".to_owned();
    let routes = BTreeMap::from([
        (
            "gpt-6.2-sol".to_owned(),
            vec![Hop::new(member("sol", "https://chatgpt.com/backend-api"))],
        ),
        ("gpt-6-sol".to_owned(), vec![Hop::new(member("plain", ""))]),
        ("pool".to_owned(), vec![pool]),
        ("glm-4.6".to_owned(), vec![Hop::new(member("glm", ""))]),
    ]);
    let gateway = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    );
    let response = gateway
        .into_router()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    let row = |id: &str| {
        json["data"]
            .as_array()
            .expect("data")
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("missing {id}"))
            .clone()
    };
    let sol = row("gpt-6.2-sol");
    assert_eq!(sol["service_tiers"][0]["id"], "priority");
    assert_eq!(sol["service_tiers"][0]["name"], "Fast");
    assert_eq!(sol["service_tiers"][1]["id"], "ultrafast");
    assert_eq!(sol["service_tiers"][1]["name"], "Ultrafast");
    assert_eq!(row("pool")["service_tiers"][1]["name"], "Ultrafast");
    assert!(row("glm-4.6").get("service_tiers").is_none());
    assert!(row("gpt-6-sol").get("service_tiers").is_none());
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

struct RawTier {
    id: MemberId,
    body: Vec<u8>,
}

#[async_trait]
impl Member for RawTier {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "primary"
    }

    fn protocol(&self) -> &str {
        "plain"
    }

    async fn execute(&self, _: ModelRequest, _: &str) -> Result<Completion, PluginError> {
        let part = Bytes::copy_from_slice(&self.body);
        Ok(Completion::Raw(BodyStream::from_stream(stream::iter(
            vec![Ok(part)],
        ))))
    }
}

#[tokio::test]
async fn d224_streamed_default_after_ultrafast_is_the_standard_rate() {
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
    let sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6.2-sol\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6.2-sol\",\"status\":\"completed\",\"service_tier\":\"default\",\"usage\":{\"input_tokens\":1000000,\"output_tokens\":0}}}\n\n",
    );
    let routes = BTreeMap::from([(
        "gpt-6.2-sol".to_owned(),
        vec![Hop::new(Arc::new(RawTier {
            id: MemberId::from_preset(
                PresetId::try_from("p").expect("preset"),
                MemberKey::from_static("raw"),
            ),
            body: sse.as_bytes().to_vec(),
        }) as Arc<dyn Member>)],
    )]);
    let mut codecs = HashMap::new();
    codecs.insert(
        "/v1/responses".to_owned(),
        Arc::new(OpenAiResponses) as Arc<dyn Codec>,
    );
    let gateway = Gateway::new(
        Snapshot::new(Router::from_hops(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        Arc::clone(&usage),
    )
    .with_effective_pricer(pricer);
    let response = gateway
        .into_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-6.2-sol","input":"hi","stream":true,"service_tier":"ultrafast"}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let row = usage.rows().expect("rows").pop().expect("row");
    assert_eq!(row.tier_sent, "ultrafast");
    assert_eq!(row.tier_served, "default");
    let price = row.price.expect("price");
    assert_eq!(price.input, 1.0);
    assert!((row.cost_usd.expect("cost") - 1.0).abs() < 1e-9);
}
