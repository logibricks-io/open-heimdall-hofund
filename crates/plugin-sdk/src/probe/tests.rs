use std::time::Duration;

use bytes::Bytes;
use hofund_plugin_api::{BodyStream, PluginError, PluginErrorKind, ProbeQuery, ProtocolId};
use serde_json::json;

use super::{
    ERROR_CAP, PROBE_WAIT, ProbePage, STREAM_CAP, merge_extra, probe_error_message, probe_failure,
    probe_report, read_probe, status_hint, stream_report,
};
use crate::HttpResponse;

fn page(status: u16, body: &[u8]) -> ProbePage {
    ProbePage {
        status,
        retry_after: None,
        body: body.to_vec(),
    }
}

fn response(status: u16, headers: &[(&str, &str)], chunks: Vec<Result<Bytes, ()>>) -> HttpResponse {
    let mut builder = http::Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(BodyStream::from_stream(futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| chunk.map_err(|()| PluginError::transport("reset"))),
        )))
        .expect("response")
}

#[test]
fn the_limits_match_the_builtin_probe() {
    assert_eq!(PROBE_WAIT, Duration::from_secs(20));
    assert_eq!(STREAM_CAP, 1024 * 1024);
    assert_eq!(ERROR_CAP, 4096);
}

#[test]
fn a_refusal_names_the_url_status_detail_and_hint() {
    let mut refused = page(
        403,
        br#"{"error":{"message":"Your Go plan doesn't include API access"}}"#,
    );
    refused.retry_after = Some(Duration::from_secs(7));
    let error = probe_failure("https://user:pw@v.example/m?key=k", &refused, &[]);
    assert_eq!(error.kind(), PluginErrorKind::Status);
    assert_eq!(error.status(), Some(403));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
    let text = error.to_string();
    assert!(
        text.starts_with(
            "https://v.example/m?<redacted>: HTTP 403 (Your Go plan doesn't include API access)"
        ),
        "{text}"
    );
    assert!(text.contains("API-enabled plan"), "{text}");
    let bare = probe_failure("https://v.example/m", &page(500, b""), &[]);
    assert_eq!(bare.message(), "https://v.example/m: HTTP 500");
    assert_eq!(bare.retry_after(), None);
}

#[test]
fn secrets_are_redacted_from_a_refusal_and_its_hint() {
    let secret = "sk-header-secret-value-1234";
    let body = format!(r#"{{"error":{{"message":"bad {secret}"}}}}"#);
    let error = probe_failure(
        "https://v.example/m",
        &page(401, body.as_bytes()),
        &["", secret],
    );
    assert!(!error.to_string().contains(secret), "{error}");
    assert!(error.to_string().contains("bad "), "{error}");
}

#[test]
fn a_report_reads_2xx_bodies_and_streams() {
    let report = probe_report("https://v.example", page(204, b"ok"), false, &[]).expect("ok");
    assert_eq!(report.snippet(), "ok");
    let refused = probe_report("https://v.example", page(300, b""), false, &[]).expect_err("300");
    assert_eq!(refused.status(), Some(300));
    let failed = probe_report("https://v.example", page(199, b""), false, &[]).expect_err("199");
    assert_eq!(failed.status(), Some(199));
    let stream = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
    let report = probe_report("https://v.example", page(200, stream), true, &[]).expect("stream");
    assert!(report.snippet().contains("hi"), "{}", report.snippet());
    let secret = "sk-stream-secret-value-1234";
    let bad = format!("data: {{\"error\":{{\"message\":\"no {secret}\"}}}}\n\n");
    let error = probe_report(
        "https://v.example",
        page(200, bad.as_bytes()),
        true,
        &[secret],
    )
    .expect_err("stream error");
    assert_eq!(error.kind(), PluginErrorKind::Decode);
    assert!(!error.to_string().contains(secret), "{error}");
    assert_eq!(stream_report(b"").expect("empty").snippet(), "");
}

#[test]
fn extra_body_keys_replace_top_level_keys() {
    let query = ProbeQuery::new(ProtocolId::GENERATIVE_LANGUAGE, "m")
        .expect("query")
        .with_extra_body(Bytes::from_static(br#"{"a":2,"b":3}"#));
    let mut body = json!({"a": 1, "c": 4});
    merge_extra(&mut body, &query).expect("merged");
    assert_eq!(body, json!({"a": 2, "b": 3, "c": 4}));
    let bad = ProbeQuery::new(ProtocolId::GENERATIVE_LANGUAGE, "m")
        .expect("query")
        .with_extra_body(Bytes::from_static(b"[1]"));
    assert!(merge_extra(&mut json!({}), &bad).is_err());
}

#[test]
fn messages_and_hints_follow_the_builtin_rules() {
    assert_eq!(
        probe_error_message(br#"{"error":{"message":"denied"}}"#),
        "denied"
    );
    assert_eq!(probe_error_message(b"<html>x</html>"), "");
    assert!(status_hint(b"<html>request has been blocked</html>").contains("network edge"));
    assert_eq!(status_hint(b"{}"), "");
}

#[tokio::test]
async fn a_stream_is_read_to_one_mib_and_a_refusal_to_four_kib() {
    let big = vec![Ok(Bytes::from(vec![b' '; STREAM_CAP + 10]))];
    let read = read_probe(response(200, &[], big.clone()), true)
        .await
        .expect("read");
    assert_eq!(read.body.len(), STREAM_CAP);
    let read = read_probe(response(200, &[], big.clone()), false)
        .await
        .expect("read");
    assert_eq!(read.body.len(), ERROR_CAP);
    let read = read_probe(response(500, &[("retry-after", "9")], big), true)
        .await
        .expect("read");
    assert_eq!(read.body.len(), ERROR_CAP);
    assert_eq!(read.status, 500);
    assert_eq!(read.retry_after, Some(Duration::from_secs(9)));
    let chunks = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"cd"))];
    let read = read_probe(response(200, &[], chunks), true)
        .await
        .expect("read");
    assert_eq!(read.body, b"abcd");
}

#[tokio::test]
async fn a_broken_refusal_keeps_what_arrived_but_a_broken_answer_fails() {
    let chunks = vec![Ok(Bytes::from_static(b"part")), Err(())];
    let read = read_probe(response(502, &[], chunks.clone()), false)
        .await
        .expect("kept");
    assert_eq!(read.body, b"part");
    let error = read_probe(response(200, &[], chunks), false)
        .await
        .expect_err("interrupted");
    assert_eq!(error.kind(), PluginErrorKind::Interrupted);
}
