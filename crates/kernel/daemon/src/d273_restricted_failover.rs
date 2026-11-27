//! Sibling-skip classification survives the subscription provider adapter.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hofund_plugin_api::{
    AccountId, Completion, ContentPart, MemberId, Message, PluginError, PluginId, ProtocolId,
    Request, Response, Role, UpstreamProvider,
};
use hofund_provider::Provider;
use hofund_router::{FailoverPolicy, Member, Router};

use crate::adapt::ProviderAdapter;

struct RestrictedUpstream;

#[async_trait]
impl UpstreamProvider for RestrictedUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        Err(PluginError::status_error(405, "account restricted")
            .account_restricted()
            .skip_failover_same_plugin())
    }
}

async fn round_tripped_error() -> PluginError {
    let adapter = ProviderAdapter::new("restricted", Arc::new(RestrictedUpstream));
    let Err(error) = adapter.execute(Request::new("demo"), "openai-chat").await else {
        panic!("restricted upstream");
    };
    crate::membership::into_plugin(error)
}

struct Fake {
    route_id: &'static str,
    member_id: MemberId,
    plugin_id: &'static str,
    hits: Arc<AtomicUsize>,
    failure: Mutex<Option<PluginError>>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn plugin_id(&self) -> &str {
        self.plugin_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.failure.lock().expect("failure").take() {
            return Err(error);
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn member(
    route_id: &'static str,
    account: &'static str,
    plugin_id: &'static str,
    hits: &Arc<AtomicUsize>,
    failure: Option<PluginError>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id,
        member_id: MemberId::from_parts(
            PluginId::from_static(plugin_id),
            AccountId::from_static(account),
        ),
        plugin_id,
        hits: Arc::clone(hits),
        failure: Mutex::new(failure),
    })
}

#[tokio::test]
async fn d273_provider_adapter_round_trip_preserves_sibling_skip() {
    let error = round_tripped_error().await;
    assert_eq!(error.status(), Some(405));
    assert!(error.is_account_restricted());
    assert!(error.skips_failover_same_plugin());
}

#[tokio::test]
async fn d273_router_skips_sibling_with_round_tripped_error() {
    let error = round_tripped_error().await;
    let first_hits = Arc::new(AtomicUsize::new(0));
    let sibling_hits = Arc::new(AtomicUsize::new(0));
    let next_hits = Arc::new(AtomicUsize::new(0));
    let mut routes = BTreeMap::new();
    routes.insert(
        "demo".to_owned(),
        vec![
            member(
                "restricted",
                "first",
                "restricted-plugin",
                &first_hits,
                Some(error),
            ),
            member(
                "restricted",
                "sibling",
                "restricted-plugin",
                &sibling_hits,
                None,
            ),
            member("next", "next", "other-plugin", &next_hits, None),
        ],
    );
    let router = Router::new(routes, FailoverPolicy::default());

    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("next member answers");
    assert_eq!(routed.upstream_id, "next");
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_hits.load(Ordering::SeqCst), 0);
    assert_eq!(next_hits.load(Ordering::SeqCst), 1);
}
