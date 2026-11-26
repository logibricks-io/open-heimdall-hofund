//! Unary `POST /v1/responses/compact` and `POST /responses/compact`.
//!
//! An API-key provider that asks the model on Responses receives the client
//! body with the wire model written in and `stream` removed, posted at
//! `{base}/responses/compact`. An account that compacts by appending a
//! trigger is streamed on `/responses` and gathered into `response.compaction`.
//! The member that sealed an item is remembered so a later Responses call
//! carrying that seal is limited to that member.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{BodyStream, Completion, PluginError};
use ring::digest;
use serde_json::{Map, Value};

/// Request extension set on a compact call. Codecs skip `hofund_` keys.
pub(super) const FLAG: &str = "hofund_responses_compact";

/// Original client JSON, so a Responses key provider is posted that body.
pub(super) const BODY: &str = "hofund_responses_compact_body";

const KEEP: usize = 1024;
const TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Who sealed a compaction item, keyed by the hash of its sealed text.
pub(super) struct Owners {
    rows: Mutex<Vec<Row>>,
}

struct Row {
    seal: [u8; 32],
    member: String,
    at: SystemTime,
}

impl Owners {
    pub(super) fn new() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
        }
    }

    /// Remembers `member` for every sealed compaction item in `body`.
    pub(super) fn record(&self, member: &str, body: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        let mut seals = Vec::new();
        collect_seals(&value, &mut seals);
        if seals.is_empty() {
            return;
        }
        let now = SystemTime::now();
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for seal in seals {
            let key = seal_key(&seal);
            rows.retain(|row| row.seal != key);
            rows.push(Row {
                seal: key,
                member: member.to_owned(),
                at: now,
            });
        }
        prune(&mut rows, now);
    }

    /// The member that sealed the latest known item in `body`, when one is.
    pub(super) fn member_for(&self, body: &[u8]) -> Option<String> {
        let value = serde_json::from_slice::<Value>(body).ok()?;
        let input = value.get("input")?.as_array()?;
        let now = SystemTime::now();
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune(&mut rows, now);
        for item in input.iter().rev() {
            let Some(seal) = sealed_text(item) else {
                continue;
            };
            let key = seal_key(seal);
            if let Some(row) = rows.iter().rev().find(|row| row.seal == key) {
                return Some(row.member.clone());
            }
        }
        None
    }
}

fn prune(rows: &mut Vec<Row>, now: SystemTime) {
    rows.retain(|row| now.duration_since(row.at).unwrap_or(Duration::ZERO) < TTL);
    if rows.len() > KEEP {
        let drop_count = rows.len() - KEEP;
        rows.drain(0..drop_count);
    }
}

fn seal_key(text: &str) -> [u8; 32] {
    let digest = digest::digest(&digest::SHA256, text.as_bytes());
    let mut key = [0_u8; 32];
    key.copy_from_slice(digest.as_ref());
    key
}

fn collect_seals(value: &Value, out: &mut Vec<String>) {
    if let Some(text) = sealed_text(value) {
        out.push(text.to_owned());
    }
    let Some(items) = value.get("output").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if let Some(text) = sealed_text(item) {
            out.push(text.to_owned());
        }
    }
}

fn sealed_text(item: &Value) -> Option<&str> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    if !matches!(kind, "compaction" | "compaction_summary") {
        return None;
    }
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// Whether `path` is one of the two compact routes.
pub(super) fn is_path(path: &str) -> bool {
    matches!(path, "/v1/responses/compact" | "/responses/compact")
}

/// Reads a raw body, gathers a trigger answer, and records who sealed it.
///
/// A key-provider answer is returned as posted. A trigger answer becomes
/// `response.compaction`. `Err` is the 502 text when a trigger answer has
/// no sealed item.
pub(super) async fn finish(
    owners: &Owners,
    original: &[u8],
    member_id: &str,
    route_id: &str,
    trigger: bool,
    completion: Completion,
) -> Result<Completion, String> {
    let bytes = match completion.peel() {
        Completion::Raw(body) => read_body(body)
            .await
            .map_err(|error| error.message().to_owned())?,
        _ if trigger => return Err(missing(route_id)),
        other => return Ok(other),
    };
    if !trigger {
        owners.record(member_id, &bytes);
        return Ok(Completion::Raw(BodyStream::from_bytes(bytes)));
    }
    let gathered = gather(original, &bytes).ok_or_else(|| missing(route_id))?;
    owners.record(member_id, &gathered);
    Ok(Completion::Raw(BodyStream::from_bytes(gathered)))
}

fn missing(route_id: &str) -> String {
    format!("{route_id}: the compaction_trigger was answered without a compaction item")
}

async fn read_body(mut body: BodyStream) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

/// Builds `response.compaction` from the client's user messages and the
/// sealed item in `upstream`. `None` when the reply has no sealed item.
fn gather(original: &[u8], upstream: &[u8]) -> Option<Vec<u8>> {
    gather_at(original, upstream, SystemTime::now())
}

/// [`gather`] with the clock used when `created_at` is absent.
fn gather_at(original: &[u8], upstream: &[u8], now: SystemTime) -> Option<Vec<u8>> {
    let (reply, items) = reply_of(upstream)?;
    let sealed: Vec<Value> = items
        .into_iter()
        .filter(|item| sealed_text(item).is_some())
        .collect();
    if sealed.is_empty() {
        return None;
    }
    let mut output = kept_users(original);
    output.extend(sealed);
    let id = reply
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("cmp_{}", hex_prefix(original)));
    let created = match reply.get("created_at") {
        Some(value) => value.clone(),
        None => Value::from(unix_secs(now)),
    };
    let mut object = Map::new();
    object.insert("id".to_owned(), Value::String(id));
    object.insert(
        "object".to_owned(),
        Value::String("response.compaction".to_owned()),
    );
    object.insert("created_at".to_owned(), created);
    object.insert("output".to_owned(), Value::Array(output));
    if let Some(usage) = reply.get("usage").filter(|usage| usage.is_object()) {
        object.insert("usage".to_owned(), usage.clone());
    }
    serde_json::to_vec(&Value::Object(object)).ok()
}

fn reply_of(upstream: &[u8]) -> Option<(Value, Vec<Value>)> {
    if let Some(events) = sse_values(upstream) {
        let mut indexed = BTreeMap::<i64, Value>::new();
        let mut reply = None;
        for (name, event) in events {
            let kind = event
                .get("type")
                .and_then(Value::as_str)
                .filter(|kind| !kind.is_empty())
                .unwrap_or(name.as_str());
            if kind == "response.output_item.done"
                && let Some(item) = event.get("item").filter(|item| !item.is_null())
            {
                let index = event
                    .get("output_index")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                indexed.insert(index, item.clone());
            }
            if matches!(kind, "response.completed" | "response.incomplete") {
                // The latest terminal replaces any earlier one. A missing
                // or non-object response clears it. `response.failed` and
                // `error` do not assign `reply`.
                reply = event
                    .get("response")
                    .filter(|response| response.is_object())
                    .cloned();
            }
        }
        let reply = reply?;
        let completed = reply
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let items = if completed.is_empty() {
            indexed.into_values().collect()
        } else {
            completed
        };
        return Some((reply, items));
    }
    let value = serde_json::from_slice::<Value>(upstream).ok()?;
    let items = value
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Some((value, items))
}

fn sse_values(body: &[u8]) -> Option<Vec<(String, Value)>> {
    let text = std::str::from_utf8(body).ok()?;
    if !text.contains("data:") {
        return None;
    }
    let mut events = Vec::new();
    for block in text.split("\n\n") {
        let mut name = String::new();
        let mut data = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                name = rest.trim().to_owned();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&data) {
            events.push((name, value));
        }
    }
    (!events.is_empty()).then_some(events)
}

fn unix_secs(now: SystemTime) -> i64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

fn kept_users(original: &[u8]) -> Vec<Value> {
    let Ok(value) = serde_json::from_slice::<Value>(original) else {
        return Vec::new();
    };
    match value.get("input") {
        Some(Value::String(text)) => vec![user_message(text)],
        Some(Value::Array(items)) => items.iter().filter(|item| is_user(item)).cloned().collect(),
        _ => Vec::new(),
    }
}

fn is_user(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        && item
            .get("type")
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "message")
}

fn user_message(text: &str) -> Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

fn hex_prefix(bytes: &[u8]) -> String {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .iter()
        .take(8)
        .fold(String::new(), |mut hex, byte| {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{Owners, gather, gather_at};

    fn fixed_clock(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    const ASKED: &str = r#"{
        "model": "m1",
        "input": [
            {"type": "compaction", "encrypted_content": "gAAAAearlier=="},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "old"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "one"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "two"}]}
        ]
    }"#;

    const STREAM: &str = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"created_at\":1}}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"created_at\":1,\"usage\":{\"total_tokens\":150},\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}]}}\n\n",
    );

    #[test]
    fn d247_gather_keeps_user_messages_then_the_sealed_item() {
        let body = gather(ASKED.as_bytes(), STREAM.as_bytes()).expect("compaction");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["object"], "response.compaction");
        assert_eq!(value["id"], "resp_1");
        assert_eq!(value["usage"]["total_tokens"], 150);
        assert_eq!(value["output"].as_array().expect("output").len(), 3);
        assert_eq!(value["output"][0]["role"], "user");
        assert_eq!(value["output"][1]["role"], "user");
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
        assert!(!value["output"].to_string().contains("gAAAAearlier=="));
    }

    #[test]
    fn d247_empty_completed_output_keeps_the_streamed_seal() {
        let upstream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"created_at\":1,\"output\":[]}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), upstream.as_bytes()).expect("compaction");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["object"], "response.compaction");
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
    }

    #[test]
    fn d247_output_index_orders_streamed_items() {
        let upstream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"LATER\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"EARLIER\"}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_9\",\"output\":[]}}\n\n",
        );
        let body = gather(br#"{"input":[]}"#, upstream.as_bytes()).expect("compaction");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][0]["encrypted_content"], "EARLIER");
        assert_eq!(value["output"][1]["encrypted_content"], "LATER");
    }

    #[test]
    fn d247_unfinished_stream_does_not_gather() {
        let cut = "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"CUTSEAL\"}}\n\n";
        assert!(gather(ASKED.as_bytes(), cut.as_bytes()).is_none());
        let failed = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"FAILSEAL\"}}\n\n",
            "data: {\"type\":\"response.failed\",\"error\":{\"message\":\"upstream stream failed\"}}\n\n",
        );
        assert!(gather(ASKED.as_bytes(), failed.as_bytes()).is_none());
        let error = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"ERRSEAL\"}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"message\":\"boom\"}}\n\n",
        );
        assert!(gather(ASKED.as_bytes(), error.as_bytes()).is_none());
    }

    #[test]
    fn d247_created_at_keeps_a_fractional_number() {
        let upstream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"created_at\":1700000000.5,\"output\":[]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), upstream.as_bytes()).expect("compaction");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["created_at"], serde_json::json!(1_700_000_000.5));
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
    }

    #[test]
    fn d247_output_index_last_write_wins_and_a_missing_index_is_zero() {
        let upstream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"MISSING\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"SLOT0\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"SLOT2\"}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_2\",\"output\":[]}}\n\n",
        );
        let body = gather(br#"{"input":[]}"#, upstream.as_bytes()).expect("compaction");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][0]["encrypted_content"], "SLOT0");
        assert_eq!(value["output"][1]["encrypted_content"], "SLOT2");
    }

    #[test]
    fn d247_created_at_copies_a_present_raw_value() {
        let now = fixed_clock(1_700_000_000);
        let mut mismatches = Vec::new();
        for (raw, expected) in [
            ("null", serde_json::Value::Null),
            ("\"soon\"", serde_json::json!("soon")),
            ("true", serde_json::json!(true)),
            ("false", serde_json::json!(false)),
            ("{\"n\":1}", serde_json::json!({"n": 1})),
            ("[1,2]", serde_json::json!([1, 2])),
            ("1700000000", serde_json::json!(1_700_000_000)),
            ("1700000000.5", serde_json::json!(1_700_000_000.5)),
        ] {
            let upstream = format!(
                "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}}}\n\n\
                 data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"created_at\":{raw},\"output\":[]}}}}\n\n"
            );
            let body = gather_at(ASKED.as_bytes(), upstream.as_bytes(), now).expect(raw);
            let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
            if value["created_at"] != expected {
                mismatches.push(format!("{raw}: got {}", value["created_at"]));
            }
            assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL", "{raw}");
        }
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    #[test]
    fn d247_created_at_uses_the_injected_clock_when_absent() {
        let upstream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
        );
        let body = gather_at(
            ASKED.as_bytes(),
            upstream.as_bytes(),
            fixed_clock(1_700_000_111),
        )
        .expect("absent created_at");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["created_at"], 1_700_000_111);
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
    }

    #[test]
    fn d247_a_later_malformed_terminal_clears_the_reply() {
        let head = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}]}}\n\n",
        );
        let mut kept = Vec::new();
        for (label, tail) in [
            (
                "completed-null",
                "data: {\"type\":\"response.completed\",\"response\":null}\n\n",
            ),
            (
                "completed-array",
                "data: {\"type\":\"response.completed\",\"response\":[]}\n\n",
            ),
            (
                "completed-string",
                "data: {\"type\":\"response.completed\",\"response\":\"nope\"}\n\n",
            ),
            (
                "completed-number",
                "data: {\"type\":\"response.completed\",\"response\":5}\n\n",
            ),
            (
                "completed-bool",
                "data: {\"type\":\"response.completed\",\"response\":true}\n\n",
            ),
            (
                "completed-missing",
                "data: {\"type\":\"response.completed\"}\n\n",
            ),
            (
                "incomplete-null",
                "data: {\"type\":\"response.incomplete\",\"response\":null}\n\n",
            ),
            (
                "incomplete-array",
                "data: {\"type\":\"response.incomplete\",\"response\":[]}\n\n",
            ),
        ] {
            let upstream = format!("{head}{tail}");
            if gather(ASKED.as_bytes(), upstream.as_bytes()).is_some() {
                kept.push(label);
            }
        }
        assert!(kept.is_empty(), "still gathered: {kept:?}");
    }

    #[test]
    fn d247_empty_json_type_uses_the_sse_event_name() {
        let upstream = concat!(
            "event: response.output_item.done\n",
            "data: {\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"id\":\"resp_1\",\"created_at\":null,\"output\":[]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), upstream.as_bytes()).expect("event name");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
        assert!(value["created_at"].is_null());
    }

    #[test]
    fn d247_empty_string_type_uses_the_sse_event_name() {
        let upstream = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "event: response.incomplete\n",
            "data: {\"type\":\"\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), upstream.as_bytes()).expect("empty type");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
    }

    #[test]
    fn d247_json_type_wins_over_the_sse_event_name() {
        let not_a_terminal = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
        );
        assert!(gather(ASKED.as_bytes(), not_a_terminal.as_bytes()).is_none());
        let type_is_completed = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), type_is_completed.as_bytes()).expect("json type");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
    }

    #[test]
    fn d247_failed_after_a_valid_completed_keeps_the_reply() {
        let upstream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\"},\"error\":{\"message\":\"later\"}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), upstream.as_bytes()).expect("kept");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][2]["encrypted_content"], "NEWSEAL");
        let only_failed = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}}\n\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"NEWSEAL\"}]}}\n\n",
        );
        assert!(gather(ASKED.as_bytes(), only_failed.as_bytes()).is_none());
    }

    #[test]
    fn d247_latest_valid_terminal_keeps_its_output() {
        let empty_latest = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"STREAMED\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"EARLIER\"}]}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), empty_latest.as_bytes()).expect("empty latest");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][2]["encrypted_content"], "STREAMED");
        let filled_latest = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"STREAMED\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"FINAL\"}]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), filled_latest.as_bytes()).expect("filled latest");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][2]["encrypted_content"], "FINAL");
        let malformed_then_valid = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"STREAMED\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":null}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[]}}\n\n",
        );
        let body = gather(ASKED.as_bytes(), malformed_then_valid.as_bytes()).expect("recovered");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["output"][2]["encrypted_content"], "STREAMED");
    }

    #[test]
    fn d247_gather_without_a_sealed_item_is_empty() {
        let upstream = br#"data: {"type":"response.completed","response":{"id":"resp_1","output":[{"type":"message","role":"assistant"}]}}

"#;
        assert!(gather(ASKED.as_bytes(), upstream).is_none());
    }

    #[test]
    fn d247_owner_is_the_member_that_sealed_the_latest_item() {
        let owners = Owners::new();
        let first = br#"{"output":[{"type":"compaction","encrypted_content":"SEAL-A"}]}"#;
        let second = br#"{"output":[{"type":"compaction","encrypted_content":"SEAL-B"}]}"#;
        owners.record("member-a", first);
        owners.record("member-b", second);
        let asked = br#"{"input":[
            {"type":"compaction","encrypted_content":"SEAL-A"},
            {"type":"compaction","encrypted_content":"SEAL-B"}
        ]}"#;
        assert_eq!(owners.member_for(asked).as_deref(), Some("member-b"));
        assert!(owners.member_for(br#"{"input":[]}"#).is_none());
    }
}
