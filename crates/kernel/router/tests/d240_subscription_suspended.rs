//! A plugin-marked suspended subscription rests a day when the key has
//! answered before, and a background list probe may lift it. A key with no
//! answer on record keeps the generic 401 rest.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hofund_plugin_api::{AccountId, MemberId, MemberKey, PluginError, PluginId, PresetId};
use hofund_router::{CooldownCause, CooldownPolicy, Cooldowns};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const MINUTE: Duration = Duration::from_secs(60);

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

fn marked() -> PluginError {
    PluginError::status_error(401, "denied").subscription_suspended()
}

fn advance(clock: &Mutex<Instant>, by: Duration) {
    *clock.lock().expect("clock") += by;
}

#[test]
fn d240_answered_key_rests_suspended_for_a_day_and_is_probed() {
    let (book, id, clock) = fixture();
    book.note_answered(&id);
    let cooling = book.record_failure(&id, &marked()).expect("rest");
    assert_eq!(cooling.cause(), CooldownCause::Suspended);
    assert_eq!(cooling.status(), Some(401));
    assert!(cooling.background_probe());
    assert_eq!(cooling.failures(), 0);
    let left = cooling
        .until()
        .duration_since(*clock.lock().expect("clock"));
    assert_eq!(left, DAY);
    let again = book.record_failure(&id, &marked()).expect("still");
    assert_eq!(again.cause(), CooldownCause::Suspended);
    assert_eq!(
        again.until().duration_since(*clock.lock().expect("clock")),
        DAY
    );
}

#[test]
fn d240_never_answered_key_keeps_the_generic_401_rest() {
    let (book, id, clock) = fixture();
    let cooling = book.record_failure(&id, &marked()).expect("rest");
    assert_eq!(cooling.cause(), CooldownCause::Failure);
    assert!(!cooling.background_probe());
    assert_eq!(
        cooling
            .until()
            .duration_since(*clock.lock().expect("clock")),
        MINUTE
    );
    assert!(!book.lift_probe(&id, cooling.until()));
    assert!(book.cooling(&id).is_some());
}

#[test]
fn d240_unmarked_401_stays_generic_after_an_answer() {
    let (book, id, _) = fixture();
    book.note_answered(&id);
    let cooling = book
        .record_failure(&id, &PluginError::status_error(401, "denied"))
        .expect("rest");
    assert_eq!(cooling.cause(), CooldownCause::Failure);
    assert!(!cooling.background_probe());
}

#[test]
fn d240_other_statuses_are_not_suspended() {
    let (book, id, _) = fixture();
    book.note_answered(&id);
    for status in [403, 429, 500] {
        book.record_success(&id);
        let error = PluginError::status_error(status, "no");
        let cooling = book.record_failure(&id, &error).expect("rest");
        assert_ne!(cooling.cause(), CooldownCause::Suspended, "{status}");
    }
}

#[test]
fn d240_suspension_outlives_the_answer_until_it_is_cleared() {
    let (book, id, clock) = fixture();
    book.note_answered(&id);
    book.record_failure(&id, &marked()).expect("first");
    book.forget_answered(&id);
    advance(&clock, DAY + Duration::from_secs(1));
    let again = book.record_failure(&id, &marked()).expect("after expiry");
    assert_eq!(again.cause(), CooldownCause::Suspended);
    assert!(again.background_probe());
    book.clear_suspension(&id);
    let cleared = book.record_failure(&id, &marked()).expect("cleared");
    assert_eq!(cleared.cause(), CooldownCause::Failure);
    assert!(!cleared.background_probe());
}

#[test]
fn d240_list_answer_lifts_the_suspended_rest() {
    let (book, id, _) = fixture();
    book.note_answered(&id);
    let cooling = book.record_failure(&id, &marked()).expect("rest");
    assert!(book.lift_probe(&id, cooling.until()));
    assert!(book.cooling(&id).is_none());
    let next = book.record_failure(&id, &marked()).expect("answered still");
    assert_eq!(next.cause(), CooldownCause::Suspended);
}

#[test]
fn d240_edited_key_forgets_the_answer_and_the_suspension() {
    let (book, old, _) = fixture();
    let new = MemberId::from_preset(
        PresetId::from_static("member"),
        MemberKey::from_static("next"),
    );
    book.note_answered(&old);
    book.record_failure(&old, &marked()).expect("old");
    book.move_member(&old, &new);
    book.forget_prior_service(&new);
    assert!(book.cooling(&new).is_none());
    let cooling = book.record_failure(&new, &marked()).expect("new");
    assert_eq!(cooling.cause(), CooldownCause::Failure);
    assert!(!cooling.background_probe());
}

#[test]
fn d240_same_key_keeps_its_answer_when_the_member_id_moves() {
    let (book, old, _) = fixture();
    let new = MemberId::from_preset(
        PresetId::from_static("member"),
        MemberKey::from_static("next"),
    );
    book.note_answered(&old);
    book.move_member(&old, &new);
    let cooling = book.record_failure(&new, &marked()).expect("moved");
    assert_eq!(cooling.cause(), CooldownCause::Suspended);
}

#[test]
fn d240_subscription_account_is_not_suspended() {
    let (book, _, _) = fixture();
    let id = MemberId::from_parts(
        PluginId::from_static("example"),
        AccountId::from_static("work"),
    );
    book.note_answered(&id);
    let cooling = book.record_failure(&id, &marked()).expect("account");
    assert_eq!(cooling.cause(), CooldownCause::Failure);
    assert!(!cooling.background_probe());
}

#[test]
fn d240_removing_the_member_drops_the_answer() {
    let (book, id, _) = fixture();
    book.note_answered(&id);
    book.remove(&id);
    let cooling = book.record_failure(&id, &marked()).expect("removed");
    assert_eq!(cooling.cause(), CooldownCause::Failure);
}

#[test]
fn d240_a_model_success_lifts_the_member_wide_suspended_rest() {
    let (book, id, _) = fixture();
    book.note_answered(&id);
    book.record_failure(&id, &marked()).expect("rest");
    let revision = book.revision(&id);
    book.record_success_for_model_if_unchanged(&id, "demo", revision);
    assert!(book.cooling(&id).is_none());
    let again = book.record_failure(&id, &marked()).expect("still answered");
    assert_eq!(again.cause(), CooldownCause::Suspended);
    assert!(again.background_probe());
}

#[test]
fn d240_a_stale_model_success_keeps_the_suspended_rest() {
    let (book, id, _) = fixture();
    book.note_answered(&id);
    let cooling = book.record_failure(&id, &marked()).expect("rest");
    book.record_success_for_model_if_unchanged(&id, "demo", 0);
    let kept = book.cooling(&id).expect("still resting");
    assert_eq!(kept.cause(), CooldownCause::Suspended);
    assert_eq!(kept.until(), cooling.until());
}
