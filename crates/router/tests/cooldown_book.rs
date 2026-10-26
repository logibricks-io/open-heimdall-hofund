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
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(8 * DAY));
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
    book.observe(&id, Some(Duration::from_secs(2 * HOUR)), None);
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(HOUR));
    book.record_success(&id);
    book.observe(&id, Some(Duration::from_secs(120)), Some(5));
    assert_eq!(remaining(&book, &id, &clock), Duration::from_secs(120));
    book.record_success(&id);
    assert_eq!(book.observe(&id, None, Some(5)), None);
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
