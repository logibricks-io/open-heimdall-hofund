//! D-278: an agent pin holds one key and leaves the model list open.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    Completion, FailoverPolicy, Hop, Member, RouteScope, Router, RouterError, affinity,
};

struct Fake {
    route_id: &'static str,
    key_id: &'static str,
    member_id: MemberId,
    hits: Arc<AtomicUsize>,
    status: Option<u16>,
}

#[async_trait]
impl Member for Fake {
    fn id(&self) -> &MemberId {
        &self.member_id
    }

    fn route_id(&self) -> &str {
        self.route_id
    }

    fn provider_key_id(&self) -> &str {
        self.key_id
    }

    fn scope_provider(&self) -> &str {
        self.route_id
    }

    fn protocol(&self) -> &str {
        "test"
    }

    fn account_refs(&self) -> Vec<String> {
        vec![self.key_id.to_owned()]
    }

    async fn execute(&self, _request: Request, _inbound: &str) -> Result<Completion, PluginError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = self.status {
            return Err(PluginError::status_error(status, "slow"));
        }
        let mut message = Message::new(Role::Assistant);
        message.content.push(ContentPart::text("ok"));
        Ok(Completion::Unary(Box::new(Response::new(message))))
    }
}

fn member(
    route_id: &'static str,
    key_id: &'static str,
    status: Option<u16>,
) -> (Arc<Fake>, Arc<dyn Member>) {
    let fake = Arc::new(Fake {
        route_id,
        key_id,
        member_id: MemberId::from_preset(
            PresetId::try_from(route_id).expect("preset"),
            MemberKey::try_from(key_id).expect("key"),
        ),
        hits: Arc::new(AtomicUsize::new(0)),
        status,
    });
    (Arc::clone(&fake), fake as Arc<dyn Member>)
}

fn hop(member: Arc<dyn Member>, fallback: bool) -> Hop {
    Hop {
        member,
        wire_model: Some("demo".to_owned()),
        max_output_tokens: None,
        catalog_output_tokens: None,
        context_window: None,
        patience: None,
        weight: 1,
        free: false,
        nearly_spent: false,
        fallback,
        effort: String::new(),
        cold_after: affinity::CACHE_COLD,
        service_tier: String::new(),
        units: Vec::new(),
    }
}

fn router(hops: Vec<Hop>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), hops);
    Router::from_hops(routes, FailoverPolicy::default())
}

fn pin(agent: &str, accounts: &[&str]) -> RouteScope {
    RouteScope {
        agent: agent.to_owned(),
        agent_accounts: accounts.iter().map(|item| (*item).to_owned()).collect(),
        ..RouteScope::default()
    }
}

async fn ask(
    router: &Router,
    scope: Option<&RouteScope>,
) -> Result<hofund_router::Routed, RouterError> {
    router
        .dispatch_scoped(&Request::new("demo"), "test", scope, None)
        .await
}

#[tokio::test]
async fn d278_each_agent_uses_its_own_key_and_an_unlisted_agent_uses_every_key() {
    let (first, first_member) = member("relay", "aaaaaaaaaa", None);
    let (second, second_member) = member("relay", "bbbbbbbbbb", None);
    let router = router(vec![hop(first_member, false), hop(second_member, false)]);

    let open = ask(&router, Some(&RouteScope::default()))
        .await
        .expect("open");
    assert_eq!(open.upstream_id, "relay");
    assert_eq!(first.hits.load(Ordering::SeqCst), 1);
    assert_eq!(second.hits.load(Ordering::SeqCst), 0);

    let writer = ask(&router, Some(&pin("writer", &["relay/bbbbbbbbbb"])))
        .await
        .expect("writer");
    assert_eq!(writer.provider_key_id, "bbbbbbbbbb");
    assert_eq!(
        first.hits.load(Ordering::SeqCst),
        1,
        "the other key is not asked"
    );
    assert_eq!(second.hits.load(Ordering::SeqCst), 1);

    assert!(
        router.models_scoped(Some(&pin("writer", &["relay/bbbbbbbbbb"]))) == ["demo"],
        "an agent pin does not hide the model"
    );
}

#[tokio::test]
async fn d278_a_resting_pin_uses_another_provider_and_not_the_sibling_key() {
    let (pinned, pinned_member) = member("relay", "aaaaaaaaaa", Some(429));
    let (sibling, sibling_member) = member("relay", "bbbbbbbbbb", None);
    let (spare, spare_member) = member("spare", "cccccccccc", None);
    let router = router(vec![
        hop(pinned_member, false),
        hop(sibling_member, false),
        hop(spare_member, true),
    ]);
    let routed = ask(&router, Some(&pin("writer", &["relay/aaaaaaaaaa"])))
        .await
        .expect("fallback");
    assert_eq!(routed.upstream_id, "spare");
    assert_eq!(pinned.hits.load(Ordering::SeqCst), 1);
    assert_eq!(sibling.hits.load(Ordering::SeqCst), 0);
    assert_eq!(spare.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn d278_a_missing_pinned_key_is_refused_and_the_sibling_is_not_asked() {
    let (sibling, sibling_member) = member("relay", "bbbbbbbbbb", None);
    let router = router(vec![hop(sibling_member, false)]);
    let error = match ask(&router, Some(&pin("writer", &["relay/aaaaaaaaaa"]))).await {
        Ok(_) => panic!("the pinned key is not a candidate"),
        Err(error) => error,
    };
    assert!(matches!(error, RouterError::AccountScope { .. }), "{error}");
    assert_eq!(sibling.hits.load(Ordering::SeqCst), 0);
}

#[test]
fn d278_a_member_pin_outside_the_surrounding_accounts_is_not_allowed() {
    let scope = RouteScope {
        accounts: vec!["relay/kept".to_owned()],
        agent: "writer".to_owned(),
        agent_accounts: vec!["relay/other".to_owned()],
        ..RouteScope::default()
    };
    let (_kept, kept) = member("relay", "kept", None);
    let (_other, other) = member("relay", "other", None);
    assert!(
        !scope.allows_member(kept.as_ref()),
        "the agent pin leaves this account out"
    );
    assert!(
        !scope.allows_member(other.as_ref()),
        "the surrounding accounts leave this pin out"
    );
}
