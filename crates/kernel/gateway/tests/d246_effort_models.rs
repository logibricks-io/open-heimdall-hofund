//! Model-list enrichment for a relay's folded effort family.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{Completion, EffortModels, FailoverPolicy, Hop, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

struct Listed {
    id: MemberId,
    family: EffortModels,
}

#[async_trait]
impl Member for Listed {
    fn id(&self) -> &MemberId {
        &self.id
    }

    fn route_id(&self) -> &str {
        "relay"
    }

    fn protocol(&self) -> &str {
        "test"
    }

    fn effort_models(&self) -> Option<EffortModels> {
        Some(self.family.clone())
    }

    async fn execute(&self, _: ModelRequest, _: &str) -> Result<Completion, PluginError> {
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }
}

#[tokio::test]
async fn d246_models_list_uses_the_folded_effort_levels() {
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let family = EffortModels::new(
        "model-4-alpha",
        "model-4-alpha-medium",
        vec![
            ("low".to_owned(), "model-4-alpha-low".to_owned()),
            ("medium".to_owned(), "model-4-alpha-medium".to_owned()),
            ("high".to_owned(), "model-4-alpha-high".to_owned()),
        ],
    );
    let member = Arc::new(Listed {
        id: MemberId::from_preset(
            PresetId::from_static("relay"),
            MemberKey::from_static("row"),
        ),
        family,
    });
    let mut hop = Hop::new(member);
    hop.wire_model = Some("model-4-alpha".to_owned());
    let router = Router::from_hops(
        BTreeMap::from([("model-4-alpha".to_owned(), vec![hop])]),
        FailoverPolicy::default(),
    );
    let response = Gateway::new(
        Snapshot::new(router),
        "secret".to_owned(),
        HashMap::new(),
        usage,
    )
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
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["data"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        body["data"][0]["supported_reasoning_levels"],
        serde_json::json!([{"effort": "low"}, {"effort": "medium"}, {"effort": "high"}])
    );
}
