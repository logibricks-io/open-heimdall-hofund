//! The Messages adapter and the router, on the transport production uses.
//!
//! This crate already depends on both. The registry does not depend on the
//! router, so the second-member case lives here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hofund_plugin_api::{
    Completion, ContentPart, MemberId, MemberKey, Message, PluginError, PluginId, PresetId,
    ProtocolId, Request, Response, Role, UpstreamProvider,
};
use hofund_plugin_host::TestHost;
use hofund_plugin_registry::anthropic;
use hofund_provider::Endpoint;
use hofund_router::{CooldownCause, CooldownPolicy, Cooldowns, FailoverPolicy, Member, Router};

const ORG: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"This organization has been disabled."}}"#;
const TWO_HOURS: Duration = Duration::from_secs(2 * 60 * 60);

struct Live {
    route_id: &'static str,
    member_id: MemberId,
    provider: Arc<dyn UpstreamProvider>,
    hits: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Option<PluginError>>>,
}

#[async_trait]
impl Member for Live {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn plugin_id(&self) -> &str {
        "anthropic"
    }

    fn protocol(&self) -> &str {
        "anthropic-messages"
    }

    async fn execute(&self, request: Request, inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let inbound = ProtocolId::try_from(inbound).expect("protocol");
        match self.provider.execute(request, inbound).await {
            Ok(done) => Ok(done),
            Err(error) => {
                *self.seen.lock().expect("seen") = Some(stamp(&error));
                Err(error)
            }
        }
    }
}

struct Spare {
    route_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Member for Spare {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn plugin_id(&self) -> &str {
        "anthropic"
    }

    fn protocol(&self) -> &str {
        "anthropic-messages"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn stamp(error: &PluginError) -> PluginError {
    let mut copied = PluginError::status_error(error.status().unwrap_or(400), error.message());
    if error.is_account_restricted() {
        copied = copied.account_restricted();
    }
    if error.is_ordinary_failure() {
        copied = copied.ordinary_failure();
    }
    if !error.cools() {
        copied = copied.without_cooldown();
    }
    copied
}

fn request() -> Request {
    serde_json::from_value(serde_json::json!({
        "model": "demo",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        "params": {"max_tokens": 16},
    }))
    .expect("request")
}

fn member_id(route: &'static str) -> MemberId {
    MemberId::from_preset(
        PresetId::try_from(route).expect("preset"),
        MemberKey::from_static("row"),
    )
}

#[tokio::test]
async fn d234_official_host_asks_the_next_member_and_rests_two_hours() {
    let (error, waited, cause) = turn("https://api.anthropic.com").await;
    assert!(error.is_account_restricted());
    assert!(!error.is_ordinary_failure());
    assert_eq!(cause, CooldownCause::Restricted);
    assert_eq!(waited, TWO_HOURS);
}

#[tokio::test]
async fn d234_relay_asks_the_next_member_with_an_ordinary_rest() {
    let (error, waited, cause) = turn("https://relay.example.com").await;
    assert!(!error.is_account_restricted());
    assert!(error.is_ordinary_failure());
    assert_eq!(cause, CooldownCause::Failure);
    assert_eq!(waited, Duration::from_secs(60));
}

async fn turn(base: &str) -> (PluginError, Duration, CooldownCause) {
    let root = tempfile::tempdir().expect("root");
    let host = TestHost::open(PluginId::from_static("anthropic"), root.path());
    host.http()
        .enqueue_response(400, &[("content-type", "application/json")], ORG.as_bytes());
    let mut endpoint = Endpoint::new("d234", base);
    endpoint.api_key = "test-key".to_owned();
    let provider = anthropic(endpoint, host.context()).expect("provider");
    let start = Instant::now();
    let clock = Arc::new(move || start);
    let cooldowns = Arc::new(Cooldowns::with_clock(CooldownPolicy::default(), clock));
    let hits = Arc::new(AtomicUsize::new(0));
    let spare_hits = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(std::sync::Mutex::new(None));
    let first_id = member_id("first");
    let first = Arc::new(Live {
        route_id: "first",
        member_id: first_id.clone(),
        provider,
        hits: Arc::clone(&hits),
        seen: Arc::clone(&seen),
    });
    let second = Arc::new(Spare {
        route_id: "second",
        member_id: member_id("second"),
        hits: Arc::clone(&spare_hits),
    });
    let mut routes = std::collections::BTreeMap::new();
    let members: Vec<Arc<dyn Member>> = vec![first, second];
    routes.insert("demo".to_owned(), members);
    let router =
        Router::new(routes, FailoverPolicy::default()).with_cooldowns(Arc::clone(&cooldowns));
    let routed = router
        .dispatch(&request(), "anthropic-messages")
        .await
        .unwrap_or_else(|error| panic!("{base}: {error}"));
    assert_eq!(routed.upstream_id, "second", "{base}");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "{base}");
    assert_eq!(spare_hits.load(Ordering::SeqCst), 1, "{base}");
    let error = seen.lock().expect("seen").take().expect("recorded error");
    let cooling = cooldowns
        .record_failure_for_model(&first_id, "demo", &error)
        .unwrap_or_else(|| panic!("{base} should rest"));
    let uri = host.http().calls()[0].uri().to_owned();
    assert!(uri.starts_with(base), "{base} {uri}");
    (
        error,
        cooling.until().duration_since(start),
        cooling.cause(),
    )
}
