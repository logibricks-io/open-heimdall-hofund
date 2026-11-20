//! `/v1/models` offers Ultrafast for a GPT Sol, Astra, or Terra route.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tempfile::TempDir;
use tower::ServiceExt;

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
        "plain"
    }

    async fn execute(&self, _: ModelRequest, _: &str) -> Result<Completion, PluginError> {
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }
}

fn member(key: &'static str) -> Arc<dyn Member> {
    Arc::new(ListedMember {
        id: MemberId::from_preset(
            PresetId::try_from("p").expect("preset"),
            MemberKey::from_static(key),
        ),
    })
}

#[tokio::test]
async fn d224_models_list_offers_ultrafast_for_the_family() {
    let temp = TempDir::new().expect("tempdir");
    let usage = Arc::new(Ledger::open(&temp.path().join("usage.sqlite")).expect("ledger"));
    let mut pool = Hop::new(member("pool"));
    pool.wire_model = Some("gpt-6.1-terra".to_owned());
    pool.service_tier = "ultrafast".to_owned();
    let routes = BTreeMap::from([
        ("gpt-6.2-sol".to_owned(), vec![Hop::new(member("sol"))]),
        ("pool".to_owned(), vec![pool]),
        ("glm-4.6".to_owned(), vec![Hop::new(member("glm"))]),
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
}
