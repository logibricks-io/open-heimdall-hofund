//! Restricted-account failover skips sibling accounts and rests the account.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hofund_ir::{ContentPart, Message, Request, Response, Role};
use hofund_plugin_api::{Completion, MemberId, MemberKey, PluginError, PresetId};
use hofund_router::{
    CooldownCause, CooldownPolicy, Cooldowns, FailoverPolicy, Member, Router, RouterError,
};

const TWO_HOURS: Duration = Duration::from_secs(2 * 60 * 60);

struct Fake {
    route_id: &'static str,
    member_id: MemberId,
    plugin_id: &'static str,
    hits: Arc<AtomicUsize>,
    failure: Option<PluginError>,
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
        if let Some(error) = &self.failure {
            let mut failure =
                PluginError::status_error(error.status().unwrap_or(400), error.message());
            if error.is_account_restricted() {
                failure = failure.account_restricted();
            }
            if error.skips_failover_same_plugin() {
                failure = failure.skip_failover_same_plugin();
            }
            Err(failure)
        } else {
            let mut message = Message::new(Role::Assistant);
            message.content.push(ContentPart::text("ok"));
            Ok(Completion::Unary(Box::new(Response::new(message))))
        }
    }
}

fn restricted_error() -> PluginError {
    PluginError::status_error(405, "account restricted")
        .account_restricted()
        .skip_failover_same_plugin()
}

fn member(
    route_id: &'static str,
    id: &'static str,
    plugin_id: &'static str,
    hits: &Arc<AtomicUsize>,
    failure: Option<PluginError>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id,
        member_id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        plugin_id,
        hits: Arc::clone(hits),
        failure,
    })
}

fn router(members: Vec<Arc<dyn Member>>, cooldowns: Arc<Cooldowns>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), members);
    Router::new(routes, FailoverPolicy::default()).with_cooldowns(cooldowns)
}

#[tokio::test]
async fn d273_restricted_account_skips_siblings_and_next_member_answers() {
    let start = Instant::now();
    let clock = Arc::new(move || start);
    let cooldowns = Arc::new(Cooldowns::with_clock(CooldownPolicy::default(), clock));
    let first_hits = Arc::new(AtomicUsize::new(0));
    let sibling_hits = Arc::new(AtomicUsize::new(0));
    let next_hits = Arc::new(AtomicUsize::new(0));
    let first = member(
        "shared",
        "first",
        "restricted-plugin",
        &first_hits,
        Some(restricted_error()),
    );
    let first_id = first.id().clone();
    let router = router(
        vec![
            first,
            member(
                "shared",
                "sibling",
                "restricted-plugin",
                &sibling_hits,
                None,
            ),
            member("next", "next", "other-plugin", &next_hits, None),
        ],
        Arc::clone(&cooldowns),
    );

    let routed = router
        .dispatch(&Request::new("demo"), "inbound")
        .await
        .expect("next member answers");
    assert_eq!(routed.upstream_id, "next");
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_hits.load(Ordering::SeqCst), 0);
    assert_eq!(next_hits.load(Ordering::SeqCst), 1);

    let cooling = cooldowns
        .record_failure_for_model(&first_id, "demo", &restricted_error())
        .expect("restricted account rests");
    assert_eq!(cooling.cause(), CooldownCause::Restricted);
    assert_eq!(cooling.until().duration_since(start), TWO_HOURS);
}

#[tokio::test]
async fn d273_plain_unmarked_405_stops_the_group() {
    let start = Instant::now();
    let cooldowns = Arc::new(Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || start),
    ));
    let next_hits = Arc::new(AtomicUsize::new(0));
    let first = member(
        "first",
        "first",
        "first-plugin",
        &Arc::new(AtomicUsize::new(0)),
        Some(PluginError::status_error(405, "method not allowed")),
    );
    let router = router(
        vec![
            first,
            member("next", "next", "other-plugin", &next_hits, None),
        ],
        cooldowns,
    );

    let Err(RouterError::Upstream { error, .. }) =
        router.dispatch(&Request::new("demo"), "inbound").await
    else {
        panic!("plain 405 must stop the group");
    };
    assert_eq!(error.status(), Some(405));
    assert_eq!(next_hits.load(Ordering::SeqCst), 0);
}
