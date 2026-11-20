//! A vendor-restricted account rests for two hours and the group moves on.
//!
//! The sentences are classified where the account is known. This harness only
//! sees the mark those classifiers set. An unmarked 400 still stops the group.
//! A relay's copy of the same body is marked as an ordinary failure, so the
//! group moves on and the member takes the generic rest.

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
    hits: Arc<AtomicUsize>,
    plugin_id: &'static str,
    error: Option<PluginError>,
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
        if let Some(error) = &self.error {
            Err(clone_error(error))
        } else {
            let mut message = Message::new(Role::Assistant);
            message.content.push(ContentPart::text("ok"));
            Ok(Completion::Unary(Box::new(Response::new(message))))
        }
    }
}

fn clone_error(error: &PluginError) -> PluginError {
    let mut copied = PluginError::status_error(error.status().unwrap_or(400), error.message());
    if !error.cools() {
        copied = copied.without_cooldown();
    }
    if error.is_account_restricted() {
        copied = copied.account_restricted();
    }
    if error.is_ordinary_failure() {
        copied = copied.ordinary_failure();
    }
    copied
}

fn member(
    id: &'static str,
    hits: &Arc<AtomicUsize>,
    plugin_id: &'static str,
    error: Option<PluginError>,
) -> Arc<dyn Member> {
    member_on(id, id, hits, plugin_id, error)
}

fn member_on(
    route_id: &'static str,
    id: &'static str,
    hits: &Arc<AtomicUsize>,
    plugin_id: &'static str,
    error: Option<PluginError>,
) -> Arc<dyn Member> {
    Arc::new(Fake {
        route_id,
        member_id: MemberId::from_preset(
            PresetId::try_from(id).expect("preset"),
            MemberKey::from_static("row"),
        ),
        hits: Arc::clone(hits),
        plugin_id,
        error,
    })
}

fn router(members: Vec<Arc<dyn Member>>, cooldowns: Arc<Cooldowns>) -> Router {
    let mut routes = BTreeMap::new();
    routes.insert("demo".to_owned(), members);
    Router::new(routes, FailoverPolicy::default()).with_cooldowns(cooldowns)
}

fn book() -> (Arc<Cooldowns>, Instant) {
    let start = Instant::now();
    let clock = Arc::new(move || start);
    (
        Arc::new(Cooldowns::with_clock(CooldownPolicy::default(), clock)),
        start,
    )
}

fn restricted(status: u16, message: &str) -> PluginError {
    PluginError::status_error(status, message).account_restricted()
}

async fn dispatch(router: &Router) -> Result<hofund_router::Routed, RouterError> {
    router.dispatch(&Request::new("demo"), "inbound").await
}

#[tokio::test]
async fn d234_restricted_400_and_403_rest_two_hours_and_ask_the_next_member() {
    for status in [400_u16, 403] {
        for message in [
            "This organization has been disabled.",
            "OAuth authentication is currently not allowed for this organization.",
        ] {
            let (cooldowns, start) = book();
            let first_hits = Arc::new(AtomicUsize::new(0));
            let second_hits = Arc::new(AtomicUsize::new(0));
            let first = member(
                "first",
                &first_hits,
                "subscription",
                Some(restricted(status, message)),
            );
            let second = member("second", &second_hits, "subscription", None);
            let first_id = first.id().clone();
            let router = router(vec![first, second], Arc::clone(&cooldowns));
            let routed = dispatch(&router)
                .await
                .unwrap_or_else(|error| panic!("{status} {message}: {error}"));
            assert_eq!(routed.upstream_id, "second", "{status} {message}");
            assert_eq!(first_hits.load(Ordering::SeqCst), 1, "{status}");
            assert_eq!(second_hits.load(Ordering::SeqCst), 1, "{status}");

            let error = restricted(status, message);
            let cooling = cooldowns
                .record_failure_for_model(&first_id, "demo", &error)
                .unwrap_or_else(|| panic!("no rest for {status} {message}"));
            assert_eq!(cooling.cause(), CooldownCause::Restricted);
            assert_eq!(cooling.until().duration_since(start), TWO_HOURS);
            assert_eq!(cooling.failures(), 0);
            assert_eq!(cooling.status(), Some(status));
            assert!(
                cooldowns
                    .cooling_for_model(&first_id, "other-model")
                    .is_some()
            );

            let again = cooldowns
                .record_failure_for_model(&first_id, "demo", &error)
                .expect("second rest");
            assert_eq!(again.cause(), CooldownCause::Restricted);
            assert_eq!(again.until().duration_since(start), TWO_HOURS);
            let generic = cooldowns
                .record_failure(&first_id, &PluginError::connect("down"))
                .expect("generic");
            assert_eq!(generic.cause(), CooldownCause::Restricted);
            assert_eq!(generic.until().duration_since(start), TWO_HOURS);

            dispatch(&router)
                .await
                .unwrap_or_else(|error| panic!("next request {status}: {error}"));
            assert_eq!(first_hits.load(Ordering::SeqCst), 1, "still resting");
            assert_eq!(second_hits.load(Ordering::SeqCst), 2, "spare answers alone");
        }
    }
}

#[tokio::test]
async fn d234_unmarked_400_stops_the_group_and_does_not_rest() {
    let (cooldowns, _) = book();
    let first_hits = Arc::new(AtomicUsize::new(0));
    let second_hits = Arc::new(AtomicUsize::new(0));
    let message = "This organization has been disabled.";
    let first = member(
        "first",
        &first_hits,
        "subscription",
        Some(PluginError::status_error(400, message)),
    );
    let first_id = first.id().clone();
    let router = router(
        vec![first, member("second", &second_hits, "subscription", None)],
        Arc::clone(&cooldowns),
    );
    let Err(error) = dispatch(&router).await else {
        panic!("group stops");
    };
    let RouterError::Upstream { upstream_id, error } = error else {
        panic!("unexpected {error}");
    };
    assert_eq!(upstream_id, "first");
    assert_eq!(error.status(), Some(400));
    assert!(!error.is_account_restricted());
    assert!(!error.is_ordinary_failure());
    assert_eq!(second_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        cooldowns.record_failure_for_model(&first_id, "demo", &error),
        None
    );
}

#[tokio::test]
async fn d234_unmarked_403_is_an_ordinary_failure_rest() {
    let (cooldowns, start) = book();
    let second_hits = Arc::new(AtomicUsize::new(0));
    let message = "This organization has been disabled.";
    let error = PluginError::status_error(403, message);
    let first = member("first", &Arc::new(AtomicUsize::new(0)), "pool", Some(error));
    let first_id = first.id().clone();
    let router = router(
        vec![first, member("second", &second_hits, "pool", None)],
        Arc::clone(&cooldowns),
    );
    let routed = dispatch(&router).await.expect("403 fails over");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    let cooling = cooldowns
        .record_failure_for_model(&first_id, "demo", &PluginError::status_error(403, message))
        .expect("ordinary rest");
    assert_eq!(cooling.cause(), CooldownCause::Failure);
    assert_eq!(
        cooling.until().duration_since(start),
        Duration::from_secs(60)
    );
    assert!(!PluginError::status_error(403, message).is_account_restricted());
}

#[tokio::test]
async fn d234_extra_usage_and_usage_policy_do_not_move_on() {
    for (message, cools) in [
        (
            "Third-party apps now draw from your extra usage, not your plan limits.",
            false,
        ),
        (
            "unable to respond to this request, which appears to violate our Usage Policy",
            true,
        ),
    ] {
        let (cooldowns, _) = book();
        let second_hits = Arc::new(AtomicUsize::new(0));
        let mut shown = PluginError::status_error(400, message);
        if !cools {
            shown = shown.without_cooldown();
        }
        let recorded = clone_error(&shown);
        let first = member(
            "first",
            &Arc::new(AtomicUsize::new(0)),
            "subscription",
            Some(shown),
        );
        let first_id = first.id().clone();
        let router = router(
            vec![first, member("second", &second_hits, "subscription", None)],
            Arc::clone(&cooldowns),
        );
        let Err(failed) = dispatch(&router).await else {
            panic!("{message}");
        };
        let RouterError::Upstream { upstream_id, error } = failed else {
            panic!("unexpected {failed}");
        };
        assert_eq!(upstream_id, "first", "{message}");
        assert_eq!(second_hits.load(Ordering::SeqCst), 0, "{message}");
        assert!(!error.is_account_restricted(), "{message}");
        assert_eq!(error.cools(), cools, "{message}");
        assert_eq!(
            cooldowns.record_failure_for_model(&first_id, "demo", &recorded),
            None,
            "{message}"
        );
    }
}

#[tokio::test]
async fn d234_ordinary_failure_400_fails_over_and_rests_on_the_generic_ladder() {
    let (cooldowns, start) = book();
    let second_hits = Arc::new(AtomicUsize::new(0));
    let message = "This organization has been disabled.";
    let error = PluginError::status_error(400, message).ordinary_failure();
    let first = member(
        "first",
        &Arc::new(AtomicUsize::new(0)),
        "subscription",
        Some(error),
    );
    let first_id = first.id().clone();
    let router = router(
        vec![first, member("second", &second_hits, "subscription", None)],
        Arc::clone(&cooldowns),
    );
    let routed = dispatch(&router).await.expect("ordinary 400 fails over");
    assert_eq!(routed.upstream_id, "second");
    assert_eq!(second_hits.load(Ordering::SeqCst), 1);
    let cooling = cooldowns
        .record_failure_for_model(
            &first_id,
            "demo",
            &PluginError::status_error(400, message).ordinary_failure(),
        )
        .expect("ordinary rest");
    assert_eq!(cooling.cause(), CooldownCause::Failure);
    assert_eq!(
        cooling.until().duration_since(start),
        Duration::from_secs(60)
    );
}

#[tokio::test]
async fn d234_restricted_rests_even_when_the_words_would_not() {
    let (cooldowns, start) = book();
    let second_hits = Arc::new(AtomicUsize::new(0));
    let message = "response protection is unavailable";
    let error = PluginError::status_error(400, message).account_restricted();
    let first = member(
        "first",
        &Arc::new(AtomicUsize::new(0)),
        "subscription",
        Some(error),
    );
    let first_id = first.id().clone();
    let router = router(
        vec![first, member("second", &second_hits, "subscription", None)],
        Arc::clone(&cooldowns),
    );
    let routed = dispatch(&router)
        .await
        .expect("restricted still fails over");
    assert_eq!(routed.upstream_id, "second");
    let cooling = cooldowns
        .record_failure_for_model(
            &first_id,
            "demo",
            &PluginError::status_error(400, message).account_restricted(),
        )
        .expect("restricted rest");
    assert_eq!(cooling.cause(), CooldownCause::Restricted);
    assert_eq!(cooling.until().duration_since(start), TWO_HOURS);
    let (plain, _) = book();
    assert_eq!(
        plain.record_failure_for_model(&first_id, "demo", &PluginError::status_error(400, message)),
        None,
        "the same words without the mark do not rest"
    );
}

#[test]
fn d234_restricted_502_stream_event_does_not_arm_or_lift_a_probe() {
    let (cooldowns, start) = book();
    let id = MemberId::from_preset(
        PresetId::try_from("api-key").expect("preset"),
        MemberKey::from_static("row"),
    );
    assert!(id.subscription().is_none());
    // An in-band permission error is inferred as 502. That status would arm a
    // model-list probe on a non-subscription member when the cause is generic.
    let error =
        PluginError::status_error(502, "account cannot receive replies").account_restricted();
    let cooling = cooldowns
        .record_failure_for_model(&id, "demo", &error)
        .expect("restricted rest");
    assert_eq!(cooling.cause(), CooldownCause::Restricted);
    assert_eq!(cooling.until().duration_since(start), TWO_HOURS);
    assert!(!cooling.background_probe());
    assert!(cooldowns.cooling(&id).is_some());
    assert!(cooldowns.cooling_for_model(&id, "other-model").is_some());
    assert!(!cooldowns.lift_probe(&id, cooling.until()));
    let still = cooldowns.cooling(&id).expect("rest stands");
    assert_eq!(still.cause(), CooldownCause::Restricted);
    assert_eq!(still.until().duration_since(start), TWO_HOURS);
    assert!(!still.background_probe());
}

#[test]
fn d234_restricted_unserved_words_stay_member_wide() {
    let (cooldowns, start) = book();
    let id = MemberId::from_preset(
        PresetId::try_from("api-key").expect("preset"),
        MemberKey::from_static("row"),
    );
    let message = "model model-a is not supported";
    let error = PluginError::status_error(404, message).account_restricted();
    let cooling = cooldowns
        .record_failure_for_model(&id, "model-a", &error)
        .expect("restricted rest");
    assert_eq!(cooling.cause(), CooldownCause::Restricted);
    assert_eq!(cooling.until().duration_since(start), TWO_HOURS);
    assert!(cooldowns.cooling(&id).is_some());
    assert!(cooldowns.cooling_for_model(&id, "model-b").is_some());
    assert_eq!(cooldowns.scoped_entries().len(), 1);
    assert_eq!(cooldowns.scoped_entries()[0].1, None);

    let (plain, _) = book();
    plain
        .record_failure_for_model(&id, "model-a", &PluginError::status_error(404, message))
        .expect("model rest");
    assert!(plain.cooling(&id).is_none());
    assert!(plain.cooling_for_model(&id, "model-b").is_none());
    assert_eq!(
        plain.scoped_entries()[0].1.as_deref(),
        Some("model-a"),
        "the same unserved words without the mark rest only that model"
    );
}

#[tokio::test]
async fn d234_restricted_protection_words_still_ask_sibling_keys() {
    let (cooldowns, _) = book();
    let mate = Arc::new(AtomicUsize::new(0));
    let other = Arc::new(AtomicUsize::new(0));
    let message = "response protection is unavailable";
    let first = member_on(
        "shared",
        "first",
        &Arc::new(AtomicUsize::new(0)),
        "pool",
        Some(PluginError::status_error(502, message).account_restricted()),
    );
    let router = router(
        vec![
            first,
            member_on("shared", "mate", &mate, "pool", None),
            member_on("other", "spare", &other, "pool", None),
        ],
        cooldowns,
    );
    let routed = dispatch(&router).await.expect("sibling is asked");
    assert_eq!(routed.upstream_id, "shared");
    assert_eq!(mate.load(Ordering::SeqCst), 1);
    assert_eq!(other.load(Ordering::SeqCst), 0);
}
