//! Cooldown ladders, limits, observations, and expiry with a manual clock.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use hofund_plugin_api::{AccountId, MemberKey, PluginId, PresetId, QuotaObservation};
use hofund_router::{
    CooldownCause, CooldownPolicy, Cooldowns, MemberId, PersistedRest, PluginError,
};

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

fn observation() -> QuotaObservation {
    QuotaObservation::from_stored(PluginId::from_static("member"))
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
        advance(&clock, seconds + 1);
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
fn c11_late_rests_only_for_vendor_faults() {
    let (book, id, _) = fixture();
    for error in [
        PluginError::interrupted("upstream connection reset"),
        PluginError::status_error(502, "output is longer than max_tokens"),
        PluginError::status_error(502, "content too long; please try again later"),
    ] {
        assert!(book.record_failure(&id, &error).is_some(), "{error}");
    }
    for message in [
        "prompt is too long: about 120 tokens",
        "Illegal API invocation from an unapproved channel",
        "response protection is unavailable",
        "an empty reply (the model answered nothing; try again)",
    ] {
        let error = PluginError::status_error(502, message);
        assert_eq!(book.record_failure(&id, &error), None, "{message}");
    }
    let shape = PluginError::status_error(400, "Unknown parameter: tools");
    assert_eq!(book.record_failure(&id, &shape), None);
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
    assert_eq!(
        remaining(&book, &id, &clock),
        Duration::from_secs(30 * MINUTE)
    );
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
    assert_eq!(generic.cause(), CooldownCause::Quota);
    assert_eq!(generic.failures(), expected.failures());
    assert_eq!(book.cooling(&id), Some(expected));
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
}

#[test]
fn a_later_deadline_brings_its_own_cause_and_failures() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &PluginError::connect("down"))
        .expect("generic");
    let quota = PluginError::quota("empty").with_retry_after(Duration::from_secs(HOUR));
    let applied = book.record_failure(&id, &quota).expect("quota");
    assert_eq!(applied.cause(), CooldownCause::Quota);
    assert_eq!(
        book.cooling(&id).map(|cooling| cooling.cause()),
        Some(CooldownCause::Quota)
    );
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
}

#[test]
fn rate_ladder_restarts_after_an_intervening_non_rate_rest() {
    let (book, id, clock) = fixture();
    let first = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate");
    assert_eq!(first.failures(), 1);
    advance(&clock, MINUTE + 1);
    let generic = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("generic");
    assert_eq!(generic.cause(), CooldownCause::Failure);
    advance(&clock, MINUTE + 1);
    let next = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate");
    assert_eq!(next.cause(), CooldownCause::RateLimit);
    assert_eq!(next.failures(), 1);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(MINUTE));
}

#[test]
fn rate_ladder_continues_when_the_previous_rest_was_a_rate_rest() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate");
    advance(&clock, MINUTE + 1);
    let next = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate");
    assert_eq!(next.failures(), 2);
    assert_eq!(
        remaining(&book, &id, &clock),
        Duration::from_secs(2 * MINUTE)
    );
}

fn assert_observed(
    book: &Cooldowns,
    id: &MemberId,
    clock: &Mutex<Instant>,
    observation: &QuotaObservation,
    expected: Duration,
) -> hofund_router::Cooling {
    let cooling = book.observe(id, observation).expect("observed cooldown");
    assert_eq!(cooling.cause(), CooldownCause::Observed);
    assert_eq!(remaining(book, id, clock), expected);
    assert_ne!(book.revision(id), 0);
    cooling
}

#[test]
fn zero_remaining_distinguishes_retry_hints_from_window_resets() {
    let (book, id, clock) = fixture();
    assert_observed(
        &book,
        &id,
        &clock,
        &observation().with_remaining(0),
        Duration::from_secs(900),
    );
    book.record_success(&id);
    assert_observed(
        &book,
        &id,
        &clock,
        &observation()
            .with_remaining(0)
            .with_retry_after(Duration::from_secs(9 * DAY)),
        Duration::from_secs(HOUR),
    );
    book.record_success(&id);
    assert_observed(
        &book,
        &id,
        &clock,
        &observation()
            .with_remaining(0)
            .with_window_reset(Duration::from_secs(9 * DAY)),
        Duration::from_secs(8 * DAY),
    );
    book.record_success(&id);
    assert_observed(
        &book,
        &id,
        &clock,
        &observation()
            .with_remaining(0)
            .with_retry_after(Duration::ZERO)
            .with_window_reset(Duration::ZERO),
        Duration::from_secs(900),
    );
    book.record_success(&id);
    assert_observed(
        &book,
        &id,
        &clock,
        &observation()
            .with_remaining(0)
            .with_retry_after(Duration::from_secs(9 * DAY))
            .with_window_reset(Duration::from_secs(2 * HOUR)),
        Duration::from_secs(2 * HOUR),
    );
}

#[test]
fn threshold_observations_require_a_positive_window_reset() {
    let (book, id, _) = fixture();
    for observed in [
        observation()
            .with_remaining(3)
            .with_limit(100)
            .with_window_reset(Duration::from_secs(2 * HOUR)),
        observation().with_remaining(2).with_limit(100),
        observation()
            .with_remaining(2)
            .with_limit(100)
            .with_retry_after(Duration::from_secs(2 * HOUR)),
        observation()
            .with_remaining(2)
            .with_limit(100)
            .with_window_reset(Duration::ZERO),
    ] {
        assert_eq!(book.observe(&id, &observed), None);
        assert_eq!(book.revision(&id), 0);
    }
}

#[test]
fn threshold_observations_rest_only_until_the_known_reset() {
    let (book, id, clock) = fixture();
    for left in [2, 1] {
        assert_observed(
            &book,
            &id,
            &clock,
            &observation()
                .with_remaining(left)
                .with_limit(100)
                .with_retry_after(Duration::from_secs(9 * DAY))
                .with_window_reset(Duration::from_secs(2 * HOUR)),
            Duration::from_secs(2 * HOUR),
        );
        book.record_success(&id);
    }
    assert_observed(
        &book,
        &id,
        &clock,
        &observation()
            .with_remaining(1)
            .with_limit(100)
            .with_window_reset(Duration::from_secs(9 * DAY)),
        Duration::from_secs(8 * DAY),
    );
}

#[test]
fn observations_exempt_accounts_spending_credits() {
    let (book, id, _) = fixture();
    for observed in [
        observation()
            .with_remaining(0)
            .with_window_reset(Duration::from_secs(2 * HOUR))
            .with_spending_credits(true),
        observation()
            .with_remaining(1)
            .with_limit(100)
            .with_window_reset(Duration::from_secs(2 * HOUR))
            .with_spending_credits(true),
    ] {
        assert_eq!(book.observe(&id, &observed), None);
        assert_eq!(book.revision(&id), 0);
    }
}

#[test]
fn zero_limit_and_inconsistent_observations_follow_exhaustion_rules() {
    let (book, id, clock) = fixture();
    assert_observed(
        &book,
        &id,
        &clock,
        &observation().with_remaining(0).with_limit(0),
        Duration::from_secs(900),
    );
    book.record_success(&id);
    for observed in [
        observation()
            .with_remaining(1)
            .with_limit(0)
            .with_window_reset(Duration::from_secs(2 * HOUR)),
        observation()
            .with_remaining(101)
            .with_limit(100)
            .with_window_reset(Duration::from_secs(2 * HOUR)),
    ] {
        assert_eq!(book.observe(&id, &observed), None);
        assert_eq!(book.revision(&id), 0);
    }
}

#[test]
fn observations_without_exhaustion_inputs_record_nothing() {
    let (book, id, _) = fixture();
    for observed in [
        observation()
            .with_remaining(5)
            .with_retry_after(Duration::from_secs(120))
            .with_window_reset(Duration::from_secs(8 * DAY)),
        observation()
            .with_limit(100)
            .with_retry_after(Duration::from_secs(120))
            .with_window_reset(Duration::from_secs(8 * DAY)),
        observation(),
    ] {
        assert_eq!(book.observe(&id, &observed), None);
        assert_eq!(book.revision(&id), 0);
    }
}

#[test]
fn observation_does_not_advance_ladders_or_shorten_a_cooldown() {
    let (book, id, clock) = fixture();
    let failure = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("failure");
    let observed = book
        .observe(
            &id,
            &observation()
                .with_remaining(0)
                .with_retry_after(Duration::from_secs(1)),
        )
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
    let _ = book.observe(&id, &observation().with_remaining(0));
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
    let observed = observation()
        .with_remaining(500)
        .with_window_reset(Duration::from_secs(30 * MINUTE));
    assert_eq!(book.observe(&id, &observed), None);
    assert_eq!(book.cooling(&id), None);
    assert_eq!(book.revision(&id), 0);
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
        let _ = book.observe(&id, &observation().with_remaining(0));
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
    let _ = book.observe(&id, &observation().with_remaining(0));
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

#[test]
fn rate_ladder_resets_during_active_non_rate_rest_after_forget() {
    let (book, clock) = with_policy(CooldownPolicy::default());
    let id = member("member");
    book.record_failure(&id, &PluginError::status_error(429, "slow"));
    advance(&clock, MINUTE);
    let second = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("second rate failure");
    assert_eq!(second.failures(), 2);
    book.record_failure(
        &id,
        &PluginError::quota("empty").with_retry_after(Duration::from_secs(HOUR)),
    );
    advance(&clock, 32 * MINUTE);
    book.maintain();
    let kept = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate failure during quota rest");
    assert_eq!(kept.cause(), CooldownCause::Quota);
    advance(&clock, 29 * MINUTE);
    let next = book
        .record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate failure after quota rest");
    assert_eq!(next.cause(), CooldownCause::RateLimit);
    assert_eq!(next.failures(), 1);
}

#[test]
fn a1_short_reset_rest_uses_the_stated_minute() {
    let (book, id, clock) = fixture();
    let error = PluginError::status_error(429, "usage limit resets in 1 minutes")
        .with_retry_after(Duration::from_secs(MINUTE));
    let cooling = book.record_failure(&id, &error).expect("rate rest");
    assert_eq!(cooling.cause(), CooldownCause::RateLimit);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(MINUTE));
}

#[test]
fn a10_rest_carries_status_and_300_vendor_characters() {
    let (book, id, _) = fixture();
    // Repeated "service" wording checks a character-safe cut.
    let words = "\u{670d}\u{52a1}".repeat(400);
    let error = PluginError::status_error(500, words);
    let cooling = book.record_failure(&id, &error).expect("failure rest");
    assert_eq!(cooling.status(), Some(500));
    assert_eq!(cooling.error().chars().count(), 300);
    assert!(cooling.error().starts_with("\u{670d}\u{52a1}"));
    let entries = book.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1.status(), Some(500));
    assert_eq!(entries[0].1.error(), cooling.error());
}

fn credit_error() -> PluginError {
    PluginError::credit_status(402, "out of credit")
}

fn subscription() -> MemberId {
    MemberId::from_parts(
        PluginId::from_static("subscription"),
        AccountId::from_static("account"),
    )
}

fn windowed() -> (Cooldowns, MemberId, Arc<Mutex<Instant>>) {
    let (book, _, clock) = fixture();
    let id = subscription();
    book.configure_credit_window(&id);
    (book, id, clock)
}

fn assert_flat_credit(error: PluginError, expected: Duration) {
    let (book, id, clock) = fixture();
    for _ in 0..3 {
        let cooling = book.record_failure(&id, &error).expect("credit rest");
        assert_eq!(cooling.failures(), 0);
        assert_eq!(remaining(&book, &id, &clock), expected);
        advance(&clock, expected.as_secs() + 1);
    }
}

#[test]
fn b1a_credit_ladder_climbs_30m_2h_8h_24h_3d_7d_and_repeats() {
    let (book, id, clock) = fixture();
    let rungs = [
        30 * MINUTE,
        2 * HOUR,
        8 * HOUR,
        DAY,
        3 * DAY,
        7 * DAY,
        7 * DAY,
    ];
    for (index, seconds) in rungs.into_iter().enumerate() {
        let cooling = book.record_failure(&id, &credit_error()).expect("credit");
        assert_eq!(cooling.failures(), (index + 1).min(6) as u32);
        assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(seconds));
        advance(&clock, seconds + 1);
    }
}

#[test]
fn b1b_refused_while_resting_keeps_rung() {
    let (book, id, clock) = fixture();
    let first = book.record_failure(&id, &credit_error()).expect("first");
    let again = book.record_failure(&id, &credit_error()).expect("again");
    assert_eq!(again.failures(), 1);
    assert_eq!(again.until(), first.until());
    advance(&clock, 30 * MINUTE + 1);
    book.record_failure(&id, &credit_error())
        .expect("second rung");
    let until = book.cooling(&id).expect("second rest").until();
    advance(&clock, MINUTE);
    let repeated = book.record_failure(&id, &credit_error()).expect("repeat");
    assert_eq!(repeated.failures(), 2);
    assert!(repeated.until() > until);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(2 * HOUR));
}

#[test]
fn b1c_refused_after_rest_ended_climbs() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &credit_error()).expect("first");
    advance(&clock, 30 * MINUTE);
    let next = book.record_failure(&id, &credit_error()).expect("next");
    assert_eq!(next.failures(), 2);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(2 * HOUR));
}

#[test]
fn b1d_success_and_manual_clear_restart_ladder() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &credit_error()).expect("first");
    book.record_success(&id);
    assert_eq!(
        book.record_failure(&id, &credit_error())
            .expect("after success")
            .failures(),
        1
    );
    advance(&clock, 30 * MINUTE + 1);
    assert_eq!(
        book.record_failure(&id, &credit_error())
            .expect("second rung")
            .failures(),
        2
    );
    book.remove(&id);
    assert_eq!(
        book.record_failure(&id, &credit_error())
            .expect("after removal")
            .failures(),
        1
    );
}

#[test]
fn b1e_retry_after_stays_flat() {
    assert_flat_credit(
        credit_error().with_retry_after(Duration::from_secs(2 * HOUR)),
        Duration::from_secs(30 * MINUTE),
    );
}

#[test]
fn b1e_afford_wording_stays_flat() {
    for message in [
        "This request can only afford 12000 tokens",
        "This request requires more credits, or fewer max_tokens",
    ] {
        assert_flat_credit(
            PluginError::credit_status(402, message).with_credit_soon(),
            Duration::from_secs(30 * MINUTE),
        );
    }
}

#[test]
fn b1e_try_again_wording_stays_flat() {
    assert_flat_credit(
        PluginError::credit_status(402, "Insufficient balance. Try again in 30 seconds")
            .with_reset_after(Duration::from_secs(30)),
        Duration::from_secs(30),
    );
}

#[test]
fn b1e_minutes_later_wording_stays_flat() {
    assert_flat_credit(
        PluginError::credit_status(402, "\u{4f59}\u{989d}\u{4e0d}\u{8db3}\u{ff0c}\u{8bf7}\u{5145}\u{503c}\u{540e} 1 \u{5206}\u{949f}\u{540e}\u{91cd}\u{8bd5}")
            .with_reset_after(Duration::from_secs(MINUTE)),
        Duration::from_secs(MINUTE),
    );
}

#[test]
fn b1f_named_reset_rests_until_it_capped_35d() {
    let (book, id, clock) = fixture();
    let error = credit_error().with_reset_after(Duration::from_secs(40 * DAY));
    let cooling = book.record_failure(&id, &error).expect("named reset");
    assert_eq!(cooling.failures(), 0);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(35 * DAY));
}

#[test]
fn b1g_subscription_account_keeps_flat_30m() {
    let (book, _, clock) = fixture();
    let id = subscription();
    for _ in 0..3 {
        let cooling = book.record_failure(&id, &credit_error()).expect("credit");
        assert_eq!(cooling.failures(), 0);
        assert_eq!(
            remaining(&book, &id, &clock),
            Duration::from_secs(30 * MINUTE)
        );
        advance(&clock, 30 * MINUTE + 1);
    }
}

#[test]
fn b2b_running_rest_continues_after_restore() {
    let (book, id, clock) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall + Duration::from_secs(2 * HOUR),
            2,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    let cooling = book.cooling(&id).expect("restored rest");
    assert_eq!(cooling.failures(), 2);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(2 * HOUR));
}

#[test]
fn b2b_ended_rest_keeps_its_rung() {
    let (book, id, clock) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall - Duration::from_secs(HOUR),
            3,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(book.cooling(&id), None);
    let next = book
        .record_failure(&id, &credit_error())
        .expect("next refusal");
    assert_eq!(next.failures(), 4);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(DAY));
}

#[test]
fn b2b_rest_ended_more_than_35_days_ago_is_forgotten() {
    let (book, id, _) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall - Duration::from_secs(35 * DAY + 1),
            5,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(book.revision(&id), 0);
}

#[test]
fn b2b_member_no_longer_configured_is_dropped() {
    let (book, id, _) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall + Duration::from_secs(HOUR),
            2,
        )],
        &HashSet::new(),
        wall,
    );
    assert_eq!(book.revision(&id), 0);
}

#[test]
fn b2b_in_process_state_wins_over_file() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &credit_error())
        .expect("in process");
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall + Duration::from_secs(7 * DAY),
            6,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(book.cooling(&id).expect("in process").failures(), 1);
    assert_eq!(
        remaining(&book, &id, &clock),
        Duration::from_secs(30 * MINUTE)
    );
}

#[test]
fn b2c_only_credit_ladder_persists() {
    let (book, id, _) = fixture();
    book.record_failure(&id, &credit_error()).expect("ladder");
    let flat = member("flat");
    book.record_failure(
        &flat,
        &credit_error().with_retry_after(Duration::from_secs(MINUTE)),
    );
    let rate = member("rate");
    book.record_failure(&rate, &PluginError::status_error(429, "slow"));
    let quota = member("quota");
    book.record_failure(&quota, &PluginError::quota("empty"));
    let account = subscription();
    book.configure_credit_window(&account);
    book.record_failure(&account, &credit_error());
    let snapshot = book.credit_snapshot(SystemTime::now());
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].member, id.to_string());
    assert_eq!(snapshot[0].rung, 1);
}

#[test]
fn b3a_credit_reset_named_in_body_wins() {
    let (book, id, clock) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    let error = credit_error()
        .with_reset_after(Duration::from_secs(10 * DAY))
        .with_retry_after(Duration::from_secs(6 * HOUR));
    book.record_failure(&id, &error).expect("body reset");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(10 * DAY));
}

#[test]
fn b3b_credit_reset_named_in_headers_wins() {
    let (book, id, clock) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    let error = credit_error()
        .with_reset_after(Duration::from_secs(2 * DAY))
        .with_retry_after(Duration::from_secs(6 * HOUR));
    book.record_failure(&id, &error).expect("header reset");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(2 * DAY));
}

#[test]
fn b3c_full_usage_window_reset_wins_over_retry_and_default() {
    let (book, id, clock) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    let error = credit_error().with_retry_after(Duration::from_secs(6 * HOUR));
    book.record_failure(&id, &error).expect("window reset");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(3 * HOUR));
}

#[test]
fn b3d_retry_after_longer_than_default_wins() {
    let (book, id, clock) = windowed();
    let error = credit_error().with_retry_after(Duration::from_secs(6 * HOUR));
    book.record_failure(&id, &error).expect("retry rest");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(6 * HOUR));
}

#[test]
fn b3e_otherwise_credit_rest_defaults_to_five_hours() {
    let (book, id, clock) = windowed();
    let error = credit_error().with_retry_after(Duration::from_secs(4 * HOUR));
    book.record_failure(&id, &error).expect("default rest");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(5 * HOUR));
}

#[test]
fn b3f_later_observation_showing_window_renewed_lifts_rest() {
    let (book, id, clock) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    book.record_failure(&id, &credit_error())
        .expect("window rest");
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(3 * HOUR));
    book.observe_credit_window(&id, None, false);
    assert_eq!(book.cooling(&id), None);
    assert_eq!(book.drain_recoveries(), vec![(id, CooldownCause::Credit)]);
}

#[test]
fn b3f_reading_with_no_window_ever_full_keeps_rest() {
    let (book, id, clock) = windowed();
    book.observe_credit_window(&id, None, false);
    book.record_failure(&id, &credit_error())
        .expect("default rest");
    book.observe_credit_window(&id, None, false);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(5 * HOUR));
    assert!(book.drain_recoveries().is_empty());
}

#[test]
fn b3f_first_reading_after_rest_does_not_lift() {
    let (book, id, clock) = windowed();
    book.record_failure(&id, &credit_error())
        .expect("default rest");
    book.observe_credit_window(&id, None, false);
    book.observe_credit_window(&id, Some(Duration::from_secs(2 * HOUR)), false);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(5 * HOUR));
}

#[test]
fn b3f_same_or_later_window_reset_keeps_rest() {
    let (book, id, clock) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    book.record_failure(&id, &credit_error())
        .expect("window rest");
    advance(&clock, MINUTE);
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR - MINUTE)), false);
    book.observe_credit_window(&id, Some(Duration::from_secs(4 * HOUR)), false);
    assert_eq!(
        remaining(&book, &id, &clock),
        Duration::from_secs(3 * HOUR - MINUTE)
    );
    book.observe_credit_window(&id, Some(Duration::from_secs(HOUR)), false);
    assert_eq!(book.cooling(&id), None);
}

#[test]
fn b3f_full_window_without_reset_renews_latest() {
    let (book, id, clock) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    book.record_failure(&id, &credit_error())
        .expect("window rest");
    book.observe_credit_window(&id, Some(Duration::from_secs(HOUR)), true);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(3 * HOUR));
    book.observe_credit_window(&id, Some(Duration::from_secs(HOUR)), false);
    assert_eq!(book.cooling(&id), None);
}

#[test]
fn b3f_renewal_lifts_only_a_credit_rest_of_a_windowed_account() {
    let (book, id, _) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    book.record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate rest");
    book.observe_credit_window(&id, None, false);
    assert!(book.cooling(&id).is_some());
    let account =
        MemberId::from_parts(PluginId::from_static("other"), AccountId::from_static("me"));
    book.observe_credit_window(&account, Some(Duration::from_secs(3 * HOUR)), false);
    book.record_failure(&account, &credit_error())
        .expect("flat rest");
    book.observe_credit_window(&account, None, false);
    assert!(book.cooling(&account).is_some());
}

#[test]
fn b3f_renewal_lifts_a_window_quota_rest_of_a_windowed_account() {
    let (book, id, _) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    let applied = book
        .observe(
            &id,
            &observation()
                .with_remaining(0)
                .with_window_reset(Duration::from_secs(3 * HOUR)),
        )
        .expect("window rest");
    assert_eq!(applied.cause(), CooldownCause::Observed);
    book.observe_credit_window(&id, None, false);
    assert_eq!(book.cooling(&id), None);
    assert_eq!(book.drain_recoveries(), vec![(id, CooldownCause::Observed)]);
}

#[test]
fn b4a_generic_no_climb_while_resting() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &PluginError::connect("down"));
    let again = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("again");
    assert_eq!(again.failures(), 1);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(MINUTE));
}

#[test]
fn b4b_generic_climbs_after_rest_ended() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &PluginError::connect("down"));
    advance(&clock, MINUTE);
    let next = book
        .record_failure(&id, &PluginError::connect("down"))
        .expect("next");
    assert_eq!(next.failures(), 2);
    assert_eq!(
        remaining(&book, &id, &clock),
        Duration::from_secs(2 * MINUTE)
    );
}

#[test]
fn b1c_ended_rung_is_kept_in_memory_past_35_days() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &credit_error()).expect("first");
    advance(&clock, 36 * DAY);
    let next = book.record_failure(&id, &credit_error()).expect("next");
    assert_eq!(next.failures(), 2);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(2 * HOUR));
}

#[test]
fn b2b_rest_of_unconfigured_member_does_not_land_on_another() {
    let (book, id, _) = fixture();
    let other = member("other");
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            other.to_string(),
            wall + Duration::from_secs(HOUR),
            2,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(book.cooling(&id), None);
    assert_eq!(book.revision(&id), 0);
}

#[test]
fn b2b_ended_rest_restores_without_a_recovery() {
    let (book, id, _) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall - Duration::from_secs(HOUR),
            3,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(book.cooling(&id), None);
    assert!(book.drain_recoveries().is_empty());
}

#[test]
fn b2b_rung_zero_rest_is_ignored() {
    let (book, id, _) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall + Duration::from_secs(HOUR),
            0,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(book.cooling(&id), None);
    assert_eq!(book.revision(&id), 0);
}

#[test]
fn b2b_restored_rest_is_capped_at_a_week() {
    let (book, id, clock) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall + Duration::from_secs(30 * DAY),
            6,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(7 * DAY));
}

#[test]
fn b2b_rung_past_the_ladder_is_clamped() {
    let (book, id, clock) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall - Duration::from_secs(HOUR),
            9,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    let next = book.record_failure(&id, &credit_error()).expect("next");
    assert_eq!(next.failures(), 6);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(7 * DAY));
}

#[test]
fn b2c_rest_ended_more_than_35_days_ago_is_not_saved() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &credit_error()).expect("ladder");
    advance(&clock, 30 * MINUTE + 35 * DAY + MINUTE);
    assert!(book.credit_snapshot(SystemTime::now()).is_empty());
    let next = book.record_failure(&id, &credit_error()).expect("next");
    assert_eq!(next.failures(), 2);
}

#[test]
fn b2c_credit_rung_is_not_saved_after_a_later_rate_rest() {
    let (book, id, clock) = fixture();
    book.record_failure(&id, &credit_error()).expect("ladder");
    assert_eq!(book.credit_snapshot(SystemTime::now()).len(), 1);
    advance(&clock, 30 * MINUTE + 1);
    book.record_failure(&id, &PluginError::status_error(429, "slow"))
        .expect("rate rest");
    assert!(book.credit_snapshot(SystemTime::now()).is_empty());
}

#[test]
fn b3f_renewal_lifts_a_quota_refusal_rest_of_a_windowed_account() {
    let (book, id, _) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    let applied = book
        .record_failure(&id, &PluginError::quota("empty"))
        .expect("quota rest");
    assert_eq!(applied.cause(), CooldownCause::Quota);
    book.observe_credit_window(&id, None, false);
    assert_eq!(book.cooling(&id), None);
    assert_eq!(book.drain_recoveries(), vec![(id, CooldownCause::Quota)]);
}

#[test]
fn b3f_renewal_ten_minutes_sooner_lifts_rest() {
    let (book, id, _) = windowed();
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR)), false);
    book.record_failure(&id, &credit_error())
        .expect("window rest");
    book.observe_credit_window(&id, Some(Duration::from_secs(3 * HOUR - 10 * MINUTE)), false);
    assert_eq!(book.cooling(&id), None);
}

#[test]
fn b2b_running_rest_past_the_ladder_restores_on_the_top_rung() {
    let (book, id, _) = fixture();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            id.to_string(),
            wall + Duration::from_secs(HOUR),
            9,
        )],
        &HashSet::from([id.clone()]),
        wall,
    );
    assert_eq!(book.cooling(&id).expect("restored").failures(), 6);
    assert_eq!(book.credit_snapshot(SystemTime::now())[0].rung, 6);
}

#[test]
fn b2b_subscription_account_rest_is_not_restored() {
    let (book, _, _) = fixture();
    let account = subscription();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    book.restore_credit(
        vec![PersistedRest::new(
            account.to_string(),
            wall + Duration::from_secs(HOUR),
            2,
        )],
        &HashSet::from([account.clone()]),
        wall,
    );
    assert_eq!(book.cooling(&account), None);
    assert_eq!(book.revision(&account), 0);
}
