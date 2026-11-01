//! Gateway list filtering and disabled refusals (C-1b).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_codec::{Codec, OpenAiChat};
use hofund_config::CatalogSettings;
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role};
use hofund_model_catalog::{CatalogFetcher, FetchResponse, ModelCatalog};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tempfile::TempDir;
use tower::ServiceExt;

const FIXTURE: &str = r#"{"acme":{"models":{"shown":{"id":"shown","status":"active","limit":{"context":8000}},"hidden":{"id":"hidden","status":"retired","limit":{"context":8000}}}}}"#;

struct IdleFetcher;

impl CatalogFetcher for IdleFetcher {
    fn fetch(&self, _: &str, _: Option<&str>) -> Result<FetchResponse, std::io::Error> {
        Err(std::io::Error::other("unused"))
    }
}

struct EchoMember {
    id: MemberId,
}

#[async_trait]
impl Member for EchoMember {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "p"
    }

    fn scope_provider(&self) -> &str {
        "p"
    }

    fn protocol(&self) -> &str {
        "test-chat"
    }

    async fn execute(&self, _: ModelRequest, _: &str) -> Result<Completion, PluginError> {
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }
}

fn gateway(temp: &TempDir, settings: CatalogSettings) -> Gateway {
    let data = temp.path().join(".hofund");
    std::fs::create_dir_all(data.join("catalog")).expect("mkdir");
    std::fs::write(
        data.join("catalog/models.dev.json"),
        format!(r#"{{"catalog": {FIXTURE}}}"#),
    )
    .expect("cache");
    let catalog = ModelCatalog::open(&data, settings, Arc::new(IdleFetcher)).expect("catalog");
    catalog.reload().expect("reload");
    let usage = Arc::new(Ledger::open(&temp.path().join("usage.sqlite")).expect("ledger"));
    let member = Arc::new(EchoMember {
        id: MemberId::from_preset(
            PresetId::try_from("p").expect("preset"),
            MemberKey::from_static("k"),
        ),
    }) as Arc<dyn Member>;
    let mut routes = BTreeMap::new();
    routes.insert("shown".to_owned(), vec![Hop::new(Arc::clone(&member))]);
    routes.insert("hidden".to_owned(), vec![Hop::new(Arc::clone(&member))]);
    let mut router = Router::from_hops(routes, FailoverPolicy::default());
    let visibility = catalog.visibility().clone();
    router = router.with_member_skip(Arc::new(move |route, asked, provider, wire| {
        visibility.hop_disabled(route, asked, provider, wire)
    }));
    let codecs = HashMap::from([(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    )]);
    Gateway::new(Snapshot::new(router), "secret".to_owned(), codecs, usage)
        .with_model_catalog(catalog)
}

#[tokio::test]
async fn c1b_9_v1_models_hides_retired_route() {
    let temp = TempDir::new().expect("tempdir");
    let gateway = gateway(&temp, CatalogSettings::default());
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
    let ids: Vec<_> = json["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["shown"]);
}

#[tokio::test]
async fn c1b_10_disabled_model_is_403_before_upstream() {
    let temp = TempDir::new().expect("tempdir");
    let settings = CatalogSettings {
        disabled: vec!["shown".to_owned()],
        ..Default::default()
    };
    let gateway = gateway(&temp, settings);
    let body = br#"{"model":"shown","messages":[{"role":"user","content":"hi"}]}"#;
    let response = gateway
        .into_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn c1b_11_explicit_retired_route_still_dispatches() {
    let temp = TempDir::new().expect("tempdir");
    let gateway = gateway(&temp, CatalogSettings::default());
    let body = br#"{"model":"hidden","messages":[{"role":"user","content":"hi"}]}"#;
    let response = gateway
        .into_router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.as_slice()))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
}
