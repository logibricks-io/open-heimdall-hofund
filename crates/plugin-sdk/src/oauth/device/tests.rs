use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Duration;

use hofund_plugin_api::{PluginError, PluginErrorKind};
use secrecy::ExposeSecret;
use tokio::time::Instant;

use super::{DeviceStart, MIN_INTERVAL, device_code_form, parse_start, poll, start};
use crate::Reply;
use crate::oauth::Clock;
use crate::oauth::clock::test_clock::{StoppedClock, TokioClock};

const PENDING: &str = r#"{"error":"authorization_pending"}"#;
const SLOW: &str = r#"{"error":"slow_down"}"#;
const TOKEN: &str =
    r#"{"access_token":"sentinel-access","refresh_token":"sentinel-refresh","expires_in":60}"#;

fn device(expires_in: u64, interval: u64) -> DeviceStart {
    parse_start(
        format!(
            r#"{{"device_code":"sentinel-device","user_code":"ABCD-EFGH","verification_uri":"https://example.com/device","expires_in":{expires_in},"interval":{interval}}}"#
        )
        .as_bytes(),
    )
    .expect("start")
}

/// Answers polls from a script and records when each one was sent. Once the
/// script runs out it answers 403, which ends the poll, so no test spins.
struct Script {
    answers: RefCell<VecDeque<Result<Reply, PluginError>>>,
    sent: RefCell<Vec<Duration>>,
    origin: Instant,
}

impl Script {
    fn new(answers: Vec<Result<Reply, PluginError>>) -> Self {
        Self {
            answers: RefCell::new(answers.into()),
            sent: RefCell::new(Vec::new()),
            origin: Instant::now(),
        }
    }

    fn send(&self) -> impl Future<Output = Result<Reply, PluginError>> + use<> {
        self.sent.borrow_mut().push(self.origin.elapsed());
        let answer = self
            .answers
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Ok(Reply::from_parts(403, "{}")));
        std::future::ready(answer)
    }

    fn sent_at(&self) -> Vec<u64> {
        self.sent.borrow().iter().map(Duration::as_secs).collect()
    }
}

fn pending(count: usize) -> Vec<Result<Reply, PluginError>> {
    (0..count).map(|_| ok(400, PENDING)).collect()
}

fn ok(status: u16, body: &str) -> Result<Reply, PluginError> {
    Ok(Reply::from_parts(status, body))
}

#[tokio::test(start_paused = true)]
async fn device_poll_honours_slow_down() {
    let script = Script::new(vec![
        ok(400, SLOW),
        ok(400, PENDING),
        ok(400, SLOW),
        ok(200, TOKEN),
    ]);
    let clock = TokioClock::new();
    let set = poll(&clock, &device(600, 5), || script.send())
        .await
        .expect("token");
    assert_eq!(set.access().expose_secret(), "sentinel-access");
    // 5 s, then 10 s after the first slow_down, then 15 s after the second.
    assert_eq!(script.sent_at(), vec![5, 15, 25, 40]);
    assert_eq!(
        set.expires_at(),
        Some(clock.now() + Duration::from_secs(60)),
        "read at the clock's now"
    );
}

#[tokio::test(start_paused = true)]
async fn device_poll_fails_after_five_misses() {
    let script = Script::new(vec![
        Err(PluginError::transport("reset")),
        ok(503, "busy"),
        ok(429, "{}"),
        ok(200, "not json"),
        ok(502, "{}"),
        ok(200, TOKEN),
    ]);
    let error = poll(&TokioClock::new(), &device(600, 1), || script.send())
        .await
        .expect_err("five misses");
    assert_eq!(error.status(), Some(502));
    assert_eq!(script.sent_at().len(), 5);
}

#[tokio::test(start_paused = true)]
async fn device_poll_miss_count_resets_on_an_answer() {
    let miss = || ok(503, "busy");
    let mut answers = vec![miss(), miss(), miss(), miss(), ok(400, PENDING)];
    answers.extend([miss(), miss(), miss(), miss(), ok(400, SLOW)]);
    answers.extend([miss(), miss(), miss(), miss(), ok(200, TOKEN)]);
    let script = Script::new(answers);
    poll(&TokioClock::new(), &device(600, 1), || script.send())
        .await
        .expect("token after misses split by answers");
    assert_eq!(script.sent_at().len(), 15);
}

#[tokio::test(start_paused = true)]
async fn device_poll_stops_at_expires_in() {
    let script = Script::new(pending(10));
    let error = poll(&TokioClock::new(), &device(12, 5), || script.send())
        .await
        .expect_err("expired");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    // Polls at 5 s and 10 s; at 15 s the code has expired, so no third poll.
    assert_eq!(script.sent_at(), vec![5, 10]);
}

#[tokio::test(start_paused = true)]
async fn device_poll_stops_at_expires_in_with_a_stopped_clock() {
    let script = Script::new(pending(10));
    let error = poll(&StoppedClock, &device(10, 5), || script.send())
        .await
        .expect_err("expired");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    assert_eq!(script.sent_at(), vec![5]);
}

#[tokio::test(start_paused = true)]
async fn device_poll_ends_on_denial_and_expiry() {
    for body in [
        r#"{"error":"access_denied","error_description":"sentinel"}"#,
        r#"{"error":"expired_token"}"#,
        r#"{"error":"invalid_grant"}"#,
    ] {
        let script = Script::new(vec![ok(400, PENDING), ok(400, body)]);
        let error = poll(&TokioClock::new(), &device(600, 1), || script.send())
            .await
            .expect_err(body);
        assert_eq!(error.kind(), PluginErrorKind::Auth, "{body}");
        assert!(!error.to_string().contains("sentinel"), "{error}");
        assert_eq!(script.sent_at().len(), 2, "{body}");
    }
    // A standard error in a 200 body (one provider does this) still counts.
    let script = Script::new(vec![
        ok(200, PENDING),
        ok(200, r#"{"error":"access_denied"}"#),
    ]);
    let error = poll(&TokioClock::new(), &device(600, 1), || script.send())
        .await
        .expect_err("denied");
    assert_eq!(error.kind(), PluginErrorKind::Auth);
    // A plain 403 is not retried.
    let script = Script::new(vec![ok(403, "{}")]);
    let error = poll(&TokioClock::new(), &device(600, 1), || script.send())
        .await
        .expect_err("forbidden");
    assert_eq!(error.status(), Some(403));
    assert_eq!(script.sent_at().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn device_poll_interval_floor_is_one_second() {
    let start = device(600, 0);
    assert_eq!(start.interval(), MIN_INTERVAL);
    let script = Script::new(vec![ok(400, PENDING), ok(200, TOKEN)]);
    poll(&TokioClock::new(), &start, || script.send())
        .await
        .expect("token");
    assert_eq!(script.sent_at(), vec![1, 2]);
}

#[test]
fn parse_start_reads_fields_and_defaults() {
    let start = parse_start(
        br#"{"device_code":"d","user_code":"U","verification_url":"https://example.com/v","verification_uri_complete":"https://example.com/v?c=U","expires_in":"900"}"#,
    )
    .expect("start");
    assert_eq!(start.user_code(), "U");
    assert_eq!(start.verification_uri(), "https://example.com/v");
    assert_eq!(
        start.verification_uri_complete(),
        Some("https://example.com/v?c=U")
    );
    assert_eq!(start.expires_in(), Duration::from_secs(900));
    assert_eq!(start.interval(), Duration::from_secs(5));
    assert_eq!(start.device_code().expose_secret(), "d");
    let debug = format!("{:?}", device(10, 1));
    assert!(!debug.contains("sentinel"), "{debug}");
    assert!(debug.contains("ABCD-EFGH"), "{debug}");
}

#[test]
fn parse_start_names_the_missing_field() {
    let full = r#""device_code":"sentinel","user_code":"U","verification_uri":"https://e.example","expires_in":9"#;
    for (field, expected) in [
        ("device_code", "device_code"),
        ("user_code", "user_code"),
        ("verification_uri", "verification_uri"),
        ("expires_in", "expires_in"),
    ] {
        let kept: Vec<&str> = full
            .split(',')
            .filter(|pair| !pair.starts_with(&format!("\"{field}\"")))
            .collect();
        let body = format!("{{{}}}", kept.join(","));
        let error = parse_start(body.as_bytes()).expect_err(field);
        assert_eq!(error.kind(), PluginErrorKind::Decode);
        assert!(error.message().ends_with(expected), "{error}");
        assert!(!error.message().contains("sentinel"), "{error}");
    }
    let zero = parse_start(
        br#"{"device_code":"d","user_code":"U","verification_uri":"https://e.example","expires_in":0}"#,
    )
    .expect_err("zero expiry");
    assert!(zero.message().ends_with("expires_in"));
    assert!(parse_start(b"[]").is_err());
}

#[tokio::test]
async fn start_maps_refusals_through_token_error() {
    let refused = start(std::future::ready(ok(400, r#"{"error":"invalid_client"}"#)))
        .await
        .expect_err("refused");
    assert_eq!(refused.kind(), PluginErrorKind::Auth);
    let started = start(std::future::ready(ok(
        200,
        r#"{"device_code":"d","user_code":"U","verification_uri":"https://e.example","expires_in":9,"interval":2}"#,
    )))
    .await
    .expect("started");
    assert_eq!(started.interval(), Duration::from_secs(2));
}

#[test]
fn device_code_form_has_the_grant() {
    let form = device_code_form("client 1", &device(10, 1));
    assert_eq!(
        form,
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&client_id=client%201&device_code=sentinel-device"
    );
}
