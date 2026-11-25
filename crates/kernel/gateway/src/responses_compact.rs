//! Unary `POST /v1/responses/compact` and `POST /responses/compact`.
//!
//! An API-key provider that asks the model on Responses receives the client
//! body with the wire model written in and `stream` removed, posted at
//! `{base}/responses/compact`. An account that compacts by appending a
//! trigger is streamed on `/responses` and gathered into `response.compaction`.
//! The member that sealed an item is remembered so a later Responses call
//! carrying that seal is limited to that member.

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
    let created = reply
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(0))
                .unwrap_or(0)
        });
    let mut object = Map::new();
    object.insert("id".to_owned(), Value::String(id));
    object.insert(
        "object".to_owned(),
        Value::String("response.compaction".to_owned()),
    );
    object.insert("created_at".to_owned(), Value::from(created));
    object.insert("output".to_owned(), Value::Array(output));
    if let Some(usage) = reply.get("usage").filter(|usage| usage.is_object()) {
        object.insert("usage".to_owned(), usage.clone());
    }
    serde_json::to_vec(&Value::Object(object)).ok()
}

fn reply_of(upstream: &[u8]) -> Option<(Value, Vec<Value>)> {
    if let Some(events) = sse_values(upstream) {
        let mut items = Vec::new();
        let mut reply = None;
        for event in events {
            let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "response.output_item.done"
                && let Some(item) = event.get("item").cloned()
            {
                items.push(item);
            }
            if matches!(kind, "response.completed" | "response.incomplete")
                && let Some(response) = event.get("response").cloned()
            {
                if let Some(output) = response.get("output").and_then(Value::as_array) {
                    items = output.clone();
                }
                reply = Some(response);
            }
        }
        return Some((reply.unwrap_or(Value::Null), items));
    }
    let value = serde_json::from_slice::<Value>(upstream).ok()?;
    let items = value
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Some((value, items))
}

fn sse_values(body: &[u8]) -> Option<Vec<Value>> {
    let text = std::str::from_utf8(body).ok()?;
    if !text.contains("data:") {
        return None;
    }
    let mut events = Vec::new();
    for block in text.split("\n\n") {
        let mut data = String::new();
        for line in block.lines() {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&data) {
            events.push(value);
        }
    }
    (!events.is_empty()).then_some(events)
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
    use super::{Owners, gather};

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
