//! Vendor overload rests: flat model-scoped cooldown without failure ladder.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hofund_plugin_api::{MemberKey, PresetId};
use hofund_router::{CooldownCause, CooldownPolicy, Cooldowns, MemberId, PluginError};

const CHATGPT_OVERLOADED: &str = r#"{"error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later."}}"#;

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

fn remaining_model(
    book: &Cooldowns,
    id: &MemberId,
    model: &str,
    clock: &Mutex<Instant>,
) -> Duration {
    book.cooling_for_model(id, model)
        .expect("model cooling")
        .until()
        .duration_since(*clock.lock().expect("clock"))
}

#[test]
fn d52_1_chatgpt_529_rests_model_only_flat_thirty_seconds() {
    let (book, id, clock) = fixture();
    let error = PluginError::status_error(529, CHATGPT_OVERLOADED);
    let cooling = book
        .record_failure_for_model(&id, "gpt-5", &error)
        .expect("cooling");
    assert_eq!(cooling.cause(), CooldownCause::Overload);
    assert_eq!(cooling.failures(), 0);
    assert_eq!(
        remaining_model(&book, &id, "gpt-5", &clock),
        Duration::from_secs(30)
    );
    assert!(book.cooling(&id).is_none());
    assert!(book.cooling_for_model(&id, "gpt-4").is_none());
}

#[test]
fn d52_2_anthropic_529_overloaded_error_same_flat_model_rest() {
    let (book, id, clock) = fixture();
    let error = PluginError::status_error(
        529,
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    );
    let cooling = book
        .record_failure_for_model(&id, "model-a", &error)
        .expect("cooling");
    assert_eq!(cooling.cause(), CooldownCause::Overload);
    assert_eq!(cooling.failures(), 0);
    assert_eq!(
        remaining_model(&book, &id, "model-a", &clock),
        Duration::from_secs(30)
    );
}

#[test]
fn d52_3_gemini_503_overloaded_vs_plain_service_unavailable() {
    let (book, id, clock) = fixture();
    let overloaded = PluginError::status_error(503, "The model is overloaded");
    let cooling = book
        .record_failure_for_model(&id, "model-b", &overloaded)
        .expect("cooling");
    assert_eq!(cooling.cause(), CooldownCause::Overload);
    assert_eq!(
        remaining_model(&book, &id, "model-b", &clock),
        Duration::from_secs(30)
    );

    book.record_success(&id);
    let plain = PluginError::status_error(503, "Service Unavailable");
    let generic = book.record_failure(&id, &plain).expect("generic cooling");
    assert_eq!(generic.cause(), CooldownCause::Failure);
    assert_eq!(generic.failures(), 1);
    assert_eq!(
        generic
            .until()
            .duration_since(*clock.lock().expect("clock")),
        Duration::from_secs(60)
    );
}

#[test]
fn d52_4_retry_after_caps_at_sixty_seconds() {
    let (book, id, clock) = fixture();
    let seven =
        PluginError::status_error(529, "overloaded").with_retry_after(Duration::from_secs(7));
    let cooling = book
        .record_failure_for_model(&id, "m", &seven)
        .expect("cooling");
    assert_eq!(cooling.cause(), CooldownCause::Overload);
    assert_eq!(
        remaining_model(&book, &id, "m", &clock),
        Duration::from_secs(7)
    );

    book.record_success_for_model(&id, "m");
    let long =
        PluginError::status_error(529, "overloaded").with_retry_after(Duration::from_secs(120));
    book.record_failure_for_model(&id, "m", &long)
        .expect("cooling");
    assert_eq!(
        remaining_model(&book, &id, "m", &clock),
        Duration::from_secs(60)
    );
}

#[test]
fn d52_5_repeated_overloads_stay_flat_and_later_generic_climbs() {
    let (book, id, clock) = fixture();
    let error = PluginError::status_error(529, CHATGPT_OVERLOADED);
    for _ in 0..6 {
        book.record_success_for_model(&id, "gpt-5");
        let cooling = book
            .record_failure_for_model(&id, "gpt-5", &error)
            .expect("cooling");
        assert_eq!(cooling.failures(), 0);
        assert_eq!(
            remaining_model(&book, &id, "gpt-5", &clock),
            Duration::from_secs(30)
        );
    }
    book.record_success_for_model(&id, "gpt-5");
    let generic = book
        .record_failure(&id, &PluginError::status_error(502, "bad gateway"))
        .expect("generic");
    assert_eq!(generic.failures(), 1);
    assert_eq!(
        generic
            .until()
            .duration_since(*clock.lock().expect("clock")),
        Duration::from_secs(60)
    );
}

#[test]
fn d52_6_429_with_overloaded_wording_is_rate_limit_not_overload() {
    let (book, id, _) = fixture();
    let error = PluginError::status_error(429, "overloaded");
    let cooling = book
        .record_failure_for_model(&id, "gpt-5", &error)
        .expect("cooling");
    assert_eq!(cooling.cause(), CooldownCause::RateLimit);
}
