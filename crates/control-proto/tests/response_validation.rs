//! Response-side envelope validation and compatibility tests.

use hofund_control_proto::*;
use serde_json::json;

#[test]
fn valid_event_envelope_decodes_at_boot_id_limit() {
    let mut envelope = event(
        EventPayload::Snapshot(SnapshotEvent::default()),
        Topic::Control,
    );
    envelope.boot_id = "b".repeat(MAX_BOOT_ID_LEN);
    envelope.seq = 7;
    let decoded = decode_event(&envelope).expect("valid event");
    assert_eq!(decoded, envelope);
}

#[test]
fn event_envelope_rejects_topic_mismatches() {
    let error = decode_event(&event(
        EventPayload::RequestStarted(RequestStartedEvent::default()),
        Topic::Control,
    ))
    .expect_err("request event on control topic");
    assert_validation_error(&error, "topic", "invalid");

    let error = decode_event(&event(
        EventPayload::Snapshot(SnapshotEvent::default()),
        Topic::Unknown,
    ))
    .expect_err("known event on unknown topic");
    assert_validation_error(&error, "topic", "invalid");
}

#[test]
fn event_envelope_rejects_invalid_boot_ids_without_echoing_them() {
    for boot_id in [
        "b:a d,=".to_owned(),
        "b".repeat(MAX_BOOT_ID_LEN + 1),
        String::new(),
    ] {
        let mut envelope = event(
            EventPayload::Snapshot(SnapshotEvent::default()),
            Topic::Control,
        );
        envelope.boot_id = boot_id.clone();
        let error = decode_event(&envelope).expect_err("invalid boot id");
        assert_validation_error(&error, "boot_id", "invalid");
        if !boot_id.is_empty() {
            assert_error_text_absent(&error, &boot_id);
        }
    }
}

#[test]
fn event_envelope_rejects_version_two() {
    let mut envelope = event(
        EventPayload::Snapshot(SnapshotEvent::default()),
        Topic::Control,
    );
    envelope.version = 2;
    let error = decode_event(&envelope).expect_err("unsupported event version");
    assert_eq!(error.kind, ControlErrorKind::UnsupportedVersion);
}

#[test]
fn unknown_event_tag_decodes_on_any_topic() {
    for topic in [Topic::Control, Topic::Requests, Topic::Unknown] {
        let envelope = event(EventPayload::Unknown, topic);
        let mut value = serde_json::to_value(envelope).expect("encode event");
        value["payload"] = json!({
            "event": "future_event",
            "data": {"anything": [1, 2, 3]}
        });
        let body = serde_json::to_vec(&value).expect("encode modified event");
        let decoded = decode_response::<EventEnvelope>(API_VERSION, &body)
            .expect("unknown event remains forward-compatible");
        assert_eq!(decoded.topic, topic);
        assert_eq!(decoded.payload, EventPayload::Unknown);
    }
}

#[test]
fn error_envelope_rejects_oversized_message_and_accepts_limit() {
    let mut envelope = valid_error_envelope();
    envelope.error.message = "x".repeat(100 * 1024);
    let error = decode_error(&envelope).expect_err("100 KB error message");
    assert_validation_error(&error, "error.message", "too_large");

    let mut envelope = valid_error_envelope();
    envelope.error.message = "x".repeat(MAX_ERROR_MESSAGE_BYTES);
    let decoded = decode_error(&envelope).expect("message at limit");
    assert_eq!(decoded.error.message.len(), MAX_ERROR_MESSAGE_BYTES);

    let mut envelope = valid_error_envelope();
    envelope.error.message = "x".repeat(MAX_ERROR_MESSAGE_BYTES + 1);
    let error = decode_error(&envelope).expect_err("message over limit");
    assert_validation_error(&error, "error.message", "too_large");
}

#[test]
fn error_envelope_rejects_invalid_request_ids_without_echoing_them() {
    for request_id in [
        "bad\u{7f}id".to_owned(),
        "bad id".to_owned(),
        String::new(),
        "r".repeat(MAX_REQUEST_ID_LEN + 1),
    ] {
        let mut envelope = valid_error_envelope();
        envelope.error.request_id = request_id.clone();
        let error = decode_error(&envelope).expect_err("invalid request id");
        assert_validation_error(&error, "error.request_id", "invalid");
        if !request_id.is_empty() {
            assert_error_text_absent(&error, &request_id);
        }
    }
}

#[test]
fn error_envelope_bounds_detail_count_and_text() {
    let mut envelope = valid_error_envelope();
    envelope.error.details = vec![FieldIssue::default(); MAX_IDS];
    decode_error(&envelope).expect("details at limit");

    let mut envelope = valid_error_envelope();
    envelope.error.details = vec![FieldIssue::default(); MAX_IDS + 1];
    let error = decode_error(&envelope).expect_err("too many details");
    assert_validation_error(&error, "error.details", "too_many");

    for name in ["field", "reason", "message"] {
        for length in [MAX_ERROR_MESSAGE_BYTES, MAX_ERROR_MESSAGE_BYTES + 1] {
            let mut envelope = valid_error_envelope();
            let issue = &mut envelope.error.details[0];
            let text = match name {
                "field" => &mut issue.field,
                "reason" => &mut issue.reason,
                "message" => &mut issue.message,
                _ => unreachable!(),
            };
            *text = "x".repeat(length);
            if length == MAX_ERROR_MESSAGE_BYTES {
                decode_error(&envelope).expect("detail text at limit");
            } else {
                let error = decode_error(&envelope).expect_err("oversized detail text");
                assert_validation_error(&error, &format!("error.details.0.{name}"), "too_large");
            }
        }
    }
}

#[test]
fn valid_error_envelope_decodes() {
    let envelope = valid_error_envelope();
    let decoded = decode_error(&envelope).expect("valid error envelope");
    assert_eq!(decoded, envelope);

    let mut envelope = valid_error_envelope();
    envelope.error.request_id = "r".repeat(MAX_REQUEST_ID_LEN);
    decode_error(&envelope).expect("request id at limit");
}

fn event(payload: EventPayload, topic: Topic) -> EventEnvelope {
    let mut envelope = EventEnvelope::default();
    envelope.boot_id = "boot-valid".to_owned();
    envelope.topic = topic;
    envelope.seq = 1;
    envelope.time_ms = 1_700_000_000_000;
    envelope.payload = payload;
    envelope
}

fn decode_event(envelope: &EventEnvelope) -> Result<EventEnvelope, ControlError> {
    let body = serde_json::to_vec(envelope).expect("encode event");
    decode_response(API_VERSION, &body)
}

fn valid_error_envelope() -> ErrorEnvelope {
    let mut envelope = ErrorEnvelope::default();
    envelope.error =
        ControlError::new(ControlErrorKind::BadRequest, "invalid request", "request-1");
    envelope.error.details = vec![FieldIssue::new("version", "invalid", "unsupported version")];
    envelope
}

fn decode_error(envelope: &ErrorEnvelope) -> Result<ErrorEnvelope, ControlError> {
    let body = serde_json::to_vec(envelope).expect("encode error envelope");
    decode_error_envelope(API_VERSION, &body)
}

fn assert_validation_error(error: &ControlError, field: &str, reason: &str) {
    assert_eq!(error.kind, ControlErrorKind::BadRequest);
    assert_eq!(error.details.len(), 1);
    assert_eq!(error.details[0].field, field);
    assert_eq!(error.details[0].reason, reason);
}

fn assert_error_text_absent(error: &ControlError, text: &str) {
    assert!(!error.message.contains(text), "{}", error.message);
    for issue in &error.details {
        assert!(!issue.field.contains(text));
        assert!(!issue.reason.contains(text));
        assert!(!issue.message.contains(text));
    }
}
