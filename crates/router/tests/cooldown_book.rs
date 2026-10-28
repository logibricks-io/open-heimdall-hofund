//! Cooldown ladders, limits, observations, and expiry with a manual clock.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hofund_plugin_api::{MemberKey, PresetId};
use hofund_router::{CooldownCause, CooldownPolicy, Cooldowns, MemberId, PluginError};

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

fn fixture() -> (Cooldowns, MemberId, Arc<Mutex<Instant>>) {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let book = Cooldowns::with_clock(
        CooldownPolicy::default(),
        Arc::new(move || *clock.lock().expect("clock")),
    );
    let id = MemberId::from_preset(
        PresetId::from_static("member"),
        MemberKey::from_static("row"),
    );
    (book, id, now)
}

fn advance(clock: &Mutex<Instant>, seconds: u64) {
    let mut now = clock.lock().expect("clock");
    *now += Duration::from_secs(seconds);
}

fn remaining(book: &Cooldowns, id: &MemberId, clock: &Mutex<Instant>) -> Duration {
    book.cooling(id)
        .expect("cooling")
        .until()
        .duration_since(*clock.lock().expect("clock"))
}

#[test]
fn generic_ladder_doubles_caps_and_success_clears_it() {
    let (book, id, clock) = fixture();
    for (index, seconds) in [60, 120, 240, 480, 600, 600].into_iter().enumerate() {
        let cooling = book
            .record_failure(&id, &PluginError::connect("down"))
            .expect("cooling");
        assert_eq!(cooling.cause(), CooldownCause::Failure);
        assert_eq!(cooling.failures(), u32::try_from(index).unwrap() + 1);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(seconds));
    }
    book.record_success(&id);
    assert_eq!(book.cooling(&id), None);
    let cooling = book.record_panic(&id);
    assert_eq!(cooling.cause(), CooldownCause::Panic);
    assert_eq!(cooling.failures(), 1);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(60));
}

#[test]
fn every_generic_failure_kind_and_status_cools() {
    let (book, id, _) = fixture();
    for error in [
        PluginError::timeout("late"),
        PluginError::transport("reset"),
        PluginError::interrupted("cut off"),
        PluginError::decode("bad"),
        PluginError::io("io"),
        PluginError::auth("denied"),
        PluginError::status_error(401, "denied"),
        PluginError::status_error(403, "denied"),
        PluginError::status_error(408, "late"),
        PluginError::status_error(500, "down"),
        PluginError::status_error(599, "down"),
    ] {
        book.record_success(&id);
        assert!(book.record_failure(&id, &error).is_some(), "{error}");
    }
}

#[test]
fn request_shaped_errors_and_statuses_do_not_cool() {
    let (book, id, _) = fixture();
    for error in [
        PluginError::cancelled("cancelled"),
        PluginError::unsupported("unsupported"),
        PluginError::config("config"),
        PluginError::internal("bug"),
        PluginError::status_error(400, "bad"),
        PluginError::status_error(404, "missing"),
        PluginError::status_error(413, "large"),
        PluginError::status_error(422, "invalid"),
    ] {
        assert_eq!(book.record_failure(&id, &error), None, "{error}");
    }
    assert!(book.entries().is_empty());
}

#[test]
fn rate_limit_uses_retry_hint_and_does_not_stretch_its_ladder() {
    let (book, id, clock) = fixture();
    let first =
        PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(2 * MINUTE));
    let cooling = book.record_failure(&id, &first).expect("cooling");
    assert_eq!(cooling.failures(), 1);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
    let repeated = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("cooling");
    assert_eq!(repeated.failures(), 1);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
    advance(&clock, 121);
    let next = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("cooling");
    assert_eq!(next.failures(), 2);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
}

#[test]
fn rate_limit_ladder_caps_resets_and_clamps_retry_hint() {
    let (book, id, clock) = fixture();
    let mut previous = 0;
    for (seconds, failures) in [60, 120, 240, 480, 960, 1800, 1800]
        .into_iter()
        .zip([1, 2, 3, 4, 5, 6, 7])
    {
        if previous > 0 {
            advance(&clock, previous + 1);
        }
        let cooling = book
            .record_failure(&id, &PluginError::status_error(429, "slow"))
            .expect("cooling");
        assert_eq!(cooling.failures(), failures);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(seconds));
        previous = seconds;
    }
    advance(&clock, 1800 + 1800);
    let reset = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("cooling");
    assert_eq!(reset.failures(), 1);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(60));
    book.record_success(&id);
    let clamped =
        PluginError::status_error(429, "slow").with_retry_after(Duration::from_secs(2 * HOUR));
    assert_eq!(
        book.record_failure(&id, &clamped)
            .expect("cooling")
            .until()
            .duration_since(*clock.lock().expect("clock")),
        Duration::from_secs(HOUR)
    );
}

#[test]
fn quota_and_credit_use_their_own_limits() {
    let (book, id, clock) = fixture();
    let quota = book
        .record_failure(&id, &PluginError::quota("empty"))
        .expect("quota");
    assert_eq!(quota.cause(), CooldownCause::Quota);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(900));
    book.record_success(&id);
    let quota = PluginError::quota("empty").with_retry_after(Duration::from_secs(9 * DAY));
    book.record_failure(&id, &quota).expect("quota");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
    book.record_success(&id);
    let credit = book
        .record_failure(&id, &PluginError::status_error(402, "credit"))
        .expect("credit");
    assert_eq!(credit.cause(), CooldownCause::Credit);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(1800));
    book.record_success(&id);
    let credit =
        PluginError::status_error(402, "credit").with_retry_after(Duration::from_secs(2 * HOUR));
    book.record_failure(&id, &credit).expect("credit");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
}

#[test]
fn a_new_failure_never_shortens_a_longer_cooldown() {
    let (book, id, clock) = fixture();
    let quota = PluginError::quota("empty").with_retry_after(Duration::from_secs(HOUR));
    let expected = book.record_failure(&id, &quota).expect("quota");
    let generic = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("generic");
    assert_eq!(generic.until(), expected.until());
    assert_eq!(generic.cause(), CooldownCause::Failure);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
}

#[test]
fn observations_follow_exhaustion_and_retry_hint_rules() {
    let (book, id, clock) = fixture();
    assert_eq!(
        book.observe(&id, None, Some(0)).expect("empty").cause(),
        CooldownCause::Observed
    );
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(900));
    book.record_success(&id);
    book.observe(&id, Some(Duration::from_secs(9 * DAY)), Some(0));
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(8 * DAY));
    book.record_success(&id);
    book.observe(&id, Some(Duration::ZERO), Some(0));
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(900));
    book.record_success(&id);
    assert_eq!(
        book.observe(&id, Some(Duration::from_secs(120)), Some(5)),
        None
    );
    assert_eq!(book.observe(&id, None, Some(5)), None);
    assert_eq!(
        book.observe(&id, Some(Duration::from_secs(120)), None),
        None
    );
    assert_eq!(book.observe(&id, None, None), None);
}

#[test]
fn observation_does_not_advance_ladders_or_shorten_a_cooldown() {
    let (book, id, clock) = fixture();
    let failure = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("failure");
    let observed = book
        .observe(&id, Some(Duration::from_secs(1)), Some(0))
        .expect("observation");
    assert_eq!(observed.until(), failure.until());
    assert_eq!(observed.failures(), 1);
    advance(&clock, 61);
    let next = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("failure");
    assert_eq!(next.failures(), 2);
}

#[test]
fn success_does_not_clear_a_newer_observation() {
    let (book, id, _) = fixture();
    let revision = book.revision(&id);
    book.observe(&id, None, Some(0));
    book.record_success_if_unchanged(&id, revision);
    assert!(book.cooling(&id).is_some());
    book.record_success(&id);
    assert_eq!(book.cooling(&id), None);
}

#[test]
fn expiry_hides_cooldowns_and_entries() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &PluginError::connect("down"));
    assert_eq!(book.entries().len(), 1);
    advance(&clock, 60);
    assert_eq!(book.cooling(&id), None);
    assert!(book.entries().is_empty());
}

// A non-positive retry hint is absent, so the standard quota rest applies.
#[test]
fn zero_retry_after_on_quota_still_rests() {
    let (book, id, clock) = fixture();
    let _ = book.record_failure(
        &id,
        &PluginError::quota("used up").with_retry_after(Duration::ZERO),
    );
    assert_eq!(
        remaining(&book, &id, &clock),
        Duration::from_secs(15 * MINUTE)
    );
}

// Header retry hints have the short trust cap, not the longer quota-reset cap.
#[test]
fn quota_header_retry_after_is_capped_at_one_hour() {
    let (book, id, clock) = fixture();
    let _ = book.record_failure(
        &id,
        &PluginError::quota("used up").with_retry_after(Duration::from_secs(3 * DAY)),
    );
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
}

// A reset hint alone does not cool a member whose window still has capacity.
#[test]
fn observation_with_remaining_quota_does_not_rest() {
    let (book, id, _) = fixture();
    let _ = book.observe(&id, Some(Duration::from_secs(30 * MINUTE)), Some(500));
    assert_eq!(book.cooling(&id), None);
}

#[test]
fn overflowing_policy_durations_still_cool_far_into_the_future() {
    let (_, id, clock) = fixture();
    let mut policy = CooldownPolicy::default();
    policy.base = Duration::MAX;
    policy.generic_max = Duration::MAX;
    policy.quota_default = Duration::MAX;
    policy.quota_max = Duration::MAX;
    let manual_clock = Arc::clone(&clock);
    let book = Cooldowns::with_clock(
        policy,
        Arc::new(move || *manual_clock.lock().expect("clock")),
    );
    let now = *clock.lock().expect("clock");
    let quota = book
        .record_failure(&id, &PluginError::quota("used up"))
        .expect("quota");
    assert!(quota.until().duration_since(now) > Duration::from_secs(365 * DAY));
    let generic = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("generic");
    assert!(generic.until().duration_since(now) > Duration::from_secs(365 * DAY));
    assert!(book.cooling(&id).is_some());
}

fn member(name: &str) -> MemberId {
    MemberId::from_preset(
        PresetId::try_from(name).expect("preset id"),
        MemberKey::from_static("default"),
    )
}

fn with_policy(policy: CooldownPolicy) -> (Cooldowns, Arc<Mutex<Instant>>) {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    (
        Cooldowns::with_clock(policy, Arc::new(move || *clock.lock().expect("clock"))),
        now,
    )
}

#[allow(clippy::field_reassign_with_default)]
fn custom_policy(max_entries: usize, rate_forget: Duration) -> CooldownPolicy {
    let mut policy = CooldownPolicy::default();
    policy.max_entries = max_entries;
    policy.rate_forget = rate_forget;
    policy
}

#[test]
fn success_does_not_insert_missing_entry() {
    let (book, _) = with_policy(CooldownPolicy::default());
    let id = member("missing");
    assert_eq!(book.revision(&id), 0);
    book.record_success(&id);
    assert_eq!(book.revision(&id), 0);
    book.record_success_if_unchanged(&id, 0);
    assert_eq!(book.revision(&id), 0);
    assert!(book.entries().is_empty());
}

#[test]
fn success_clears_and_removes_entry_without_history() {
    let (book, _) = with_policy(CooldownPolicy::default());
    for conditional in [false, true] {
        let id = member(if conditional { "conditional" } else { "direct" });
        book.observe(&id, None, Some(0));
        let revision = book.revision(&id);
        if conditional {
            book.record_success_if_unchanged(&id, revision);
        } else {
            book.record_success(&id);
        }
        assert_eq!(book.cooling(&id), None);
        assert_eq!(book.revision(&id), 0);
    }
}

#[test]
fn stale_success_after_missing_capture_does_not_clear_concurrent_failure() {
    let (book, _) = with_policy(CooldownPolicy::default());
    let id = member("member");
    assert_eq!(book.revision(&id), 0);
    book.record_failure(&id, &PluginError::connect("down"));
    book.record_success_if_unchanged(&id, 0);
    assert!(book.cooling(&id).is_some());
}

#[test]
fn revisions_are_book_wide_monotonic_and_never_zero() {
    let (book, _) = with_policy(CooldownPolicy::default());
    let first = member("first");
    let second = member("second");
    assert_eq!(book.revision(&first), 0);
    book.record_failure(&first, &PluginError::connect("down"));
    let one = book.revision(&first);
    book.record_failure(&second, &PluginError::connect("down"));
    let two = book.revision(&second);
    assert!(one >= 1);
    assert!(two > one);
    book.record_success(&first);
    assert_eq!(book.revision(&first), 0);
    book.record_failure(&first, &PluginError::connect("down"));
    assert!(book.revision(&first) > two);
}

#[test]
fn expired_idle_entry_is_pruned_on_read() {
    let policy = custom_policy(10_000, Duration::from_secs(10));
    let (book, clock) = with_policy(policy);
    let id = member("member");
    book.record_failure(&id, &PluginError::connect("down"));
    advance(&clock, 61);
    assert_eq!(book.cooling(&id), None);
    assert_eq!(book.revision(&id), 1);
    advance(&clock, 10);
    assert_eq!(book.revision(&id), 0);
}

#[test]
fn rate_limit_entry_kept_for_rate_forget_then_pruned() {
    let policy = custom_policy(10_000, Duration::from_secs(30));
    let (book, clock) = with_policy(policy);
    let id = member("member");
    book.record_failure(&id, &PluginError::status_error(429, "slow"));
    advance(&clock, 60);
    assert_eq!(book.cooling(&id), None);
    assert_ne!(book.revision(&id), 0);
    advance(&clock, 29);
    assert_ne!(book.revision(&id), 0);
    advance(&clock, 1);
    assert_eq!(book.revision(&id), 0);
}

#[test]
fn maintenance_never_expires_active_rest() {
    let policy = custom_policy(10_000, Duration::ZERO);
    let (book, _) = with_policy(policy);
    let id = member("member");
    book.record_failure(&id, &PluginError::connect("down"));
    book.maintain();
    assert!(book.cooling(&id).is_some());
    assert_ne!(book.revision(&id), 0);
}

#[test]
fn bounded_book_evicts_expired_then_lru_idle_never_active() {
    let mut policy = custom_policy(2, Duration::from_secs(30));
    let (book, clock) = with_policy(policy.clone());
    let expired = member("expired");
    let active = member("active");
    let incoming = member("incoming");
    book.record_failure(&expired, &PluginError::connect("down"));
    advance(&clock, 1);
    book.record_failure(&active, &PluginError::connect("down"));
    advance(&clock, 59);
    book.record_failure(&incoming, &PluginError::connect("down"));
    assert_eq!(book.revision(&expired), 0);
    assert!(book.cooling(&active).is_some());
    assert!(book.cooling(&incoming).is_some());

    let (book, clock) = with_policy(policy.clone());
    book.record_failure(&expired, &PluginError::connect("down"));
    advance(&clock, 61);
    assert_eq!(book.cooling(&expired), None);
    book.record_failure(&active, &PluginError::connect("down"));
    book.record_failure(&incoming, &PluginError::connect("down"));
    assert_eq!(book.revision(&expired), 0);
    assert!(book.cooling(&active).is_some());
    assert!(book.cooling(&incoming).is_some());

    policy.max_entries = 1;
    let (book, _) = with_policy(policy);
    book.record_failure(&active, &PluginError::connect("down"));
    book.record_failure(&incoming, &PluginError::connect("down"));
    assert!(book.cooling(&active).is_some());
    assert_eq!(book.cooling(&incoming), None);
    assert_eq!(book.revision(&incoming), 0);
}

#[test]
fn recovery_fires_once_per_rest() {
    let (book, clock) = with_policy(CooldownPolicy::default());
    let expired = member("expired");
    let cleared = member("cleared");
    book.record_failure(&expired, &PluginError::connect("down"));
    advance(&clock, 60);
    assert_eq!(book.cooling(&expired), None);
    assert_eq!(book.cooling(&expired), None);
    assert_eq!(
        book.drain_recoveries(),
        vec![(expired.clone(), CooldownCause::Failure)]
    );
    assert!(book.drain_recoveries().is_empty());

    book.record_failure(&cleared, &PluginError::status_error(429, "slow"));
    book.record_success(&cleared);
    book.record_success(&cleared);
    assert_eq!(
        book.drain_recoveries(),
        vec![(cleared, CooldownCause::RateLimit)]
    );
    assert!(book.drain_recoveries().is_empty());
}

#[test]
fn candidate_retention_prunes_only_absent_members() {
    let (book, _) = with_policy(CooldownPolicy::default());
    let kept = member("kept");
    let removed = member("removed");
    book.record_failure(&kept, &PluginError::connect("down"));
    book.record_failure(&removed, &PluginError::connect("down"));
    book.retain_members(&std::collections::HashSet::from([kept.clone()]));
    assert!(book.cooling(&kept).is_some());
    assert_eq!(book.revision(&removed), 0);
}

#[test]
fn observation_only_rest_is_pruned_as_soon_as_it_expires() {
    let (book, clock) = with_policy(CooldownPolicy::default());
    let id = member("observed");
    book.observe(&id, None, Some(0));
    assert_ne!(book.revision(&id), 0);
    advance(&clock, 15 * MINUTE);
    assert_eq!(book.revision(&id), 0);
    assert_eq!(book.drain_recoveries(), vec![(id, CooldownCause::Observed)]);
}

#[test]
fn rate_ladder_resets_after_forget_despite_later_generic_history() {
    let (book, clock) = with_policy(CooldownPolicy::default());
    let id = member("member");
    book.record_failure(&id, &PluginError::connect("down"));
    advance(&clock, 1);
    book.record_failure(&id, &PluginError::status_error(429, "slow"));
    advance(&clock, 1);
    book.record_failure(&id, &PluginError::connect("down"));
    advance(&clock, 1_859);
    let next = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate rest");
    assert_eq!(next.failures(), 1);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(MINUTE));
}
