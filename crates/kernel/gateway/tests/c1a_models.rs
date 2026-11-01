//! Gateway `/v1/models` enrichment from the model catalog.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_config::CatalogSettings;
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role};
use hofund_model_catalog::{CatalogFetcher, FetchResponse, ModelCatalog};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tempfile::TempDir;
use tower::ServiceExt;

const FIXTURE: &str = r#"{
  "openai": {
    "id": "openai",
    "models": {
      "demo": {
        "id": "demo",
        "reasoning_options": [{"type": "effort", "values": ["low", "medium"]}],
        "modalities": {"input": ["text", "image"], "output": ["text"]},
        "limit": {"context": 64000, "output": 8192}
      }
    }
  }
}"#;

struct IdleFetcher;

impl CatalogFetcher for IdleFetcher {
    fn fetch(&self, _: &str, _: Option<&str>) -> Result<FetchResponse, std::io::Error> {
        Err(std::io::Error::other("unused in this test"))
    }
}

struct ListedMember {
    id: MemberId,
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
        "openai-chat"
    }

    async fn execute(&self, _: ModelRequest, _: &str) -> Result<Completion, PluginError> {
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }
}

#[tokio::test]
async fn c1a_7_models_list_includes_catalog_spec_fields() {
    let temp = TempDir::new().expect("tempdir");
    let data = temp.path().join(".hofund");
    std::fs::create_dir_all(data.join("catalog")).expect("mkdir");
    std::fs::write(
        data.join("catalog/models.dev.json"),
        format!(r#"{{"catalog": {FIXTURE}}}"#),
    )
    .expect("write cache");

    let fetcher = Arc::new(IdleFetcher);
    let catalog = ModelCatalog::open(&data, CatalogSettings::default(), fetcher).expect("catalog");
    catalog.reload().expect("reload");

    let usage_path = temp.path().join("usage.sqlite");
    let usage = Arc::new(Ledger::open(&usage_path).expect("ledger"));
    let member = Arc::new(ListedMember {
        id: MemberId::from_preset(
            PresetId::try_from("p").expect("preset"),
            MemberKey::from_static("row"),
        ),
    }) as Arc<dyn Member>;
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), vec![member]);
    let gateway = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
    .with_model_catalog(catalog);

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
    let model = json["data"]
        .as_array()
        .and_then(|rows| rows.first())
        .expect("one model");
    assert_eq!(model["id"], "demo");
    assert_eq!(model["context_window"], 64000);
    assert_eq!(model["max_output_tokens"], 8192);
    assert!(model["input_modalities"].as_array().is_some());
    assert!(model["supported_reasoning_levels"].as_array().is_some());
}
