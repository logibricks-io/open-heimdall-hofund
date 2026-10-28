//! Composite SSE id grammar and resume snapshots.

use std::fs;
use std::path::PathBuf;

use hofund_control_proto::*;

#[test]
fn event_id_grammar_matches_the_snapshot() {
    let long_boot = format!("{}:control=1", "a".repeat(MAX_BOOT_ID_LEN + 1));
    let control_byte = "bo\u{1}ot:control=1";
    let cases = vec![
        ("boot:control=0", "ok boot:control=0"),
        ("boot:requests=40", "ok boot:requests=40"),
        (
            "boot:control=12,requests=40",
            "ok boot:control=12,requests=40",
        ),
        (
            "boot:control=18446744073709551615",
            "ok boot:control=18446744073709551615",
        ),
        ("boot:control=18446744073709551616", "err BadSequence"),
        ("boot:control=01", "err BadSequence"),
        ("boot:control=+1", "err BadSequence"),
        ("boot:control=-1", "err BadSequence"),
        (":control=1", "err BadBootId"),
        ("bo=ot:control=1", "err BadBootId"),
        ("bo,ot:control=1", "err BadBootId"),
        ("bo ot:control=1", "err BadBootId"),
        (control_byte, "err BadBootId"),
        (&long_boot, "err BadBootId"),
        ("boot", "err MissingColon"),
        ("boot:control=1:requests=2", "err BadCursor"),
        ("boot:future=1", "err UnknownTopic"),
        ("boot:control=1,control=2", "err Duplicate"),
        ("boot:requests=2,control=1", "err OutOfOrder"),
        ("boot:control=1,", "err BadCursor"),
        ("boot:", "err BadCursor"),
        ("boot:control=1,,requests=2", "err BadCursor"),
        ("", "err Empty"),
    ];
    let mut actual = String::new();
    for (input, expected) in cases {
        let result = EventId::parse(input)
            .map_or_else(|error| format!("err {error:?}"), |id| format!("ok {id}"));
        assert_eq!(result, expected, "input {input:?}");
        actual.push_str(&format!("{} -> {result}\n", escape(input)));
    }
    text_snapshot("event-id-grammar.txt", &actual);
}

#[test]
fn event_id_construction_and_display_round_trip() {
    let id = EventId::new(
        "boot",
        vec![
            EventCursor::new(Topic::Control, 12),
            EventCursor::new(Topic::Requests, 40),
        ],
    )
    .expect("valid id");
    assert_eq!(EventId::parse(&id.to_string()).expect("round trip"), id);
    assert_eq!(id.boot_id(), "boot");
    assert_eq!(id.cursors().len(), 2);
    assert_eq!(
        EventId::new("bad boot", vec![EventCursor::new(Topic::Control, 1)]).expect_err("bad boot"),
        EventIdError::BadBootId
    );
    assert_eq!(
        EventId::new("boot", Vec::new()).expect_err("empty cursors"),
        EventIdError::BadCursor
    );
    assert_eq!(
        EventId::new("boot", vec![EventCursor::new(Topic::Unknown, 1)]).expect_err("unknown topic"),
        EventIdError::UnknownTopic
    );
}

#[test]
fn resume_plan_matches_the_snapshot() {
    let both = [Topic::Requests, Topic::Control, Topic::Requests];
    let cases = [
        ("no-header", None, "boot", &both[..]),
        (
            "valid-two-topic",
            Some("boot:control=12,requests=40"),
            "boot",
            &both[..],
        ),
        (
            "unknown-boot",
            Some("other:control=12,requests=40"),
            "boot",
            &both[..],
        ),
        ("missing-topic", Some("boot:control=12"), "boot", &both[..]),
        (
            "malformed",
            Some("boot:control=01,requests=40"),
            "boot",
            &both[..],
        ),
        (
            "duplicate",
            Some("boot:control=12,control=13"),
            "boot",
            &both[..],
        ),
        (
            "unrequested-topic-ignored",
            Some("boot:control=12,requests=40"),
            "boot",
            &[Topic::Requests][..],
        ),
    ];
    let mut actual = String::new();
    for (name, header, boot, requested) in cases {
        let plan = resume_plan(header, boot, requested)
            .into_iter()
            .map(|(topic, resume)| format!("{}={}", topic.as_str(), show_resume(resume)))
            .collect::<Vec<_>>()
            .join(",");
        actual.push_str(&format!("{name} -> {plan}\n"));
    }
    text_snapshot("resume-plan.txt", &actual);
}

fn show_resume(resume: Resume) -> String {
    match resume {
        Resume::Fresh => "fresh".to_owned(),
        Resume::After(seq) => format!("after({seq})"),
        Resume::Resync(reason) => format!("resync({})", reason_label(reason)),
        _ => "future".to_owned(),
    }
}

fn reason_label(reason: ResyncReason) -> &'static str {
    match reason {
        ResyncReason::Lagged => "lagged",
        ResyncReason::UnknownBoot => "unknown_boot",
        ResyncReason::Unreplayable => "unreplayable",
        ResyncReason::MissingTopic => "missing_topic",
        ResyncReason::MalformedId => "malformed_id",
        ResyncReason::Unknown => "unknown",
        _ => "future",
    }
}

fn escape(input: &str) -> String {
    input.chars().flat_map(char::escape_default).collect()
}

fn text_snapshot(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        fs::create_dir_all(path.parent().expect("snapshot parent")).expect("snapshot directory");
        fs::write(&path, actual).expect("write snapshot");
    }
    let expected = fs::read_to_string(&path).expect("read snapshot");
    assert_eq!(actual, expected, "snapshot {name} drifted");
}
