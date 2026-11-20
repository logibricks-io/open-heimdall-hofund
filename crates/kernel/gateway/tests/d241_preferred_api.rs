//! `/v1/models` names the API a route is asked on best, and a catalog entry a member learned.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_gateway::Gateway;
use hofund_ir::{Message, Request as ModelRequest, Response, Role};
use hofund_plugin_api::{
    CATALOG_ENTRY_FIELD, MemberId, MemberKey, PREFERRED_API_FIELD, PluginError, PresetId,
    ProtocolId,
};

const CHAT: &str = ProtocolId::OPENAI_CHAT.as_str();
const RESPONSES: &str = ProtocolId::OPENAI_RESPONSES.as_str();
const MESSAGES: &str = ProtocolId::ANTHROPIC_MESSAGES.as_str();
use hofund_router::{Completion, FailoverPolicy, Hop, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tempfile::TempDir;
use tower::ServiceExt;

struct ListedMember {
    id: MemberId,
    protocol: &'static str,
    preferred: Option<String>,
    catalog_entry: Option<String>,
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
        self.protocol
    }

    fn learned_preferred(&self, _model: &str) -> Option<String> {
        self.preferred.clone()
    }

    fn learned_catalog_entry(&self, _model: &str) -> Option<String> {
        self.catalog_entry.clone()
    }

    async fn execute(&self, _: ModelRequest, _: &str) -> Result<Completion, PluginError> {
        Ok(Completion::Unary(Box::new(Response::new(Message::new(
            Role::Assistant,
        )))))
    }
}

fn member(
    key: &'static str,
    protocol: &'static str,
    preferred: Option<&str>,
    catalog_entry: Option<&str>,
) -> Arc<dyn Member> {
    Arc::new(ListedMember {
        id: MemberId::from_preset(
            PresetId::try_from("p").expect("preset"),
            MemberKey::from_static(key),
        ),
        protocol,
        preferred: preferred.map(str::to_owned),
        catalog_entry: catalog_entry.map(str::to_owned),
    })
}

#[tokio::test]
async fn d241_models_list_names_the_preferred_api() {
    let temp = TempDir::new().expect("tempdir");
    let usage = Arc::new(Ledger::open(&temp.path().join("usage.sqlite")).expect("ledger"));
    let routes = BTreeMap::from([
        (
            "north/model-a".to_owned(),
            vec![Hop::new(member("astra", RESPONSES, None, None))],
        ),
        (
            "south/model-b".to_owned(),
            vec![Hop::new(member("south", CHAT, None, None))],
        ),
        (
            "east/model-c".to_owned(),
            vec![Hop::new(member("east", MESSAGES, None, None))],
        ),
        (
            "group/auto".to_owned(),
            vec![
                Hop::new(member("g1", RESPONSES, None, None)),
                Hop::new(member("g2", RESPONSES, None, None)),
            ],
        ),
        (
            "group/mixed".to_owned(),
            vec![
                Hop::new(member("m1", CHAT, None, None)),
                Hop::new(member("m2", RESPONSES, None, None)),
            ],
        ),
        (
            "plain-model".to_owned(),
            vec![Hop::new(member("plain", "plain", None, None))],
        ),
        (
            "learned".to_owned(),
            vec![Hop::new(member("learned", CHAT, Some("responses"), None))],
        ),
        (
            "handed".to_owned(),
            vec![Hop::new(member(
                "handed",
                CHAT,
                None,
                Some(
                    r#"{"base_instructions":"REMOTE","model_messages":{"instructions_template":"T"}}"#,
                ),
            ))],
        ),
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
    assert_eq!(row("north/model-a")[PREFERRED_API_FIELD], "/v1/responses");
    assert_eq!(
        row("south/model-b")[PREFERRED_API_FIELD],
        "/v1/chat/completions"
    );
    assert_eq!(row("east/model-c")[PREFERRED_API_FIELD], "/v1/messages");
    assert_eq!(row("group/auto")[PREFERRED_API_FIELD], "/v1/responses");
    assert!(row("group/mixed").get(PREFERRED_API_FIELD).is_none());
    assert!(row("plain-model").get(PREFERRED_API_FIELD).is_none());
    assert_eq!(row("learned")[PREFERRED_API_FIELD], "/v1/responses");
    assert_eq!(
        row("handed")[CATALOG_ENTRY_FIELD]["base_instructions"],
        "REMOTE"
    );
    assert_eq!(
        row("handed")[CATALOG_ENTRY_FIELD]["model_messages"]["instructions_template"],
        "T"
    );
}
