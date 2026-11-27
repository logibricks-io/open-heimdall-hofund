//! D-278: TOML pins survive the daemon's agent-identification and routing hooks.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hofund_agent::request_agent_id;
use hofund_codec::{Codec, OpenAiChat};
use hofund_gateway::{AgentAccountHook, Gateway, RequestAgentHook};
use hofund_plugin_api::{
    AgentDescription, AgentRoute, ContentPart, MemberId, MemberKey, Message, PluginError, PluginId,
    PresetId, Request as ModelRequest, Response, Role, Visible,
};
use hofund_router::{Completion, FailoverPolicy, Member, Router, Snapshot};
use hofund_usage::Ledger;
use tower::ServiceExt;

use crate::agent_accounts;
use crate::provider_keys::key_id;
use crate::subscriptions::Subscriptions;

static AGENT: AgentDescription = AgentDescription::from_static(
    PluginId::from_static("fixture-agent"),
    "Fixture Agent",
    "fixture",
    &[],
    &[],
    None,
    None,
    Visible::All,
    AgentRoute::Gateway,
    &[],
)
.with_client_prefixes(&["fixture"]);

struct Fake {
    key_id: String,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        "relay"
    }

    fn provider_key_id(&self) -> &str {
        &self.key_id
    }

    fn protocol(&self) -> &str {
        "openai-chat"
    }

    fn account_refs(&self) -> Vec<String> {
        vec![self.key_id.clone()]
    }

    async fn execute(
        &self,
        _request: ModelRequest,
        _inbound: &str,
    ) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn member(secret: &str, hits: &Arc<AtomicUsize>) -> Arc<dyn Member> {
    let key_id = key_id(secret);
    Arc::new(Fake {
        member_id: MemberId::from_preset(
            PresetId::from_static("relay"),
            MemberKey::try_from(key_id.as_str()).expect("member key"),
        ),
        key_id,
        hits: Arc::clone(hits),
    })
}

#[tokio::test]
async fn d278_toml_agent_pin_limits_dispatch_through_the_real_daemon_hooks() {
    let config = Arc::new(
        hofund_config::load(
            r#"
[[providers]]
id = "relay"
protocol = "openai-chat"
base_url = "http://127.0.0.1:9"

[[providers.keys]]
name = "first"
key = "first-secret"

[[providers.keys]]
name = "second"
key = "second-secret"

[providers.agent_accounts]
"Fixture-Agent" = "Second"
"#,
        )
        .expect("TOML config"),
    );
    assert_eq!(
        config.providers[0].agent_accounts.get("fixture-agent"),
        Some(&"Second".to_owned())
    );

    let request_agent: RequestAgentHook = Arc::new(|headers, identity| {
        request_agent_id(
            headers
                .iter()
                .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?))),
            &identity.key_name,
            &[&AGENT],
        )
    });
    let subscriptions = Subscriptions::new();
    let pin_config = Arc::clone(&config);
    let pins: AgentAccountHook =
        Arc::new(move |agent| agent_accounts::pins(&pin_config, agent, &subscriptions));

    let first = Arc::new(AtomicUsize::new(0));
    let second = Arc::new(AtomicUsize::new(0));
    let routes = BTreeMap::from([(
        "demo".to_owned(),
        vec![
            member("first-secret", &first),
            member("second-secret", &second),
        ],
    )]);
    let codecs = HashMap::from([(
        "/v1/chat/completions".to_owned(),
        Arc::new(OpenAiChat) as Arc<dyn Codec>,
    )]);
    let dir = tempfile::tempdir().expect("temp dir");
    let usage = Arc::new(Ledger::open(&dir.path().join("usage.sqlite")).expect("usage"));
    let app = Gateway::new(
        Snapshot::new(Router::new(routes, FailoverPolicy::default())),
        "secret".to_owned(),
        codecs,
        usage,
    )
    .with_request_agent(request_agent)
    .with_agent_accounts(pins)
    .into_router();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer secret")
                .header("user-agent", "FiXtUrE/1.0")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"demo","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(first.load(Ordering::SeqCst), 0);
    assert_eq!(second.load(Ordering::SeqCst), 1);
}
