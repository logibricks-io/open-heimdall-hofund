//! Recovery when a member cannot read another member's sealed input.

use std::collections::{BTreeMap, BTreeSet, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::affinity::{Conversation, STICK_KEEP};
use crate::{Attempt, AttemptHook};

const SEALED_KINDS: &[&str] = &["reasoning", "compaction", "compaction_summary"];

#[derive(Clone, Default)]
pub(crate) struct SealedState {
    refused: Arc<Mutex<BTreeMap<(String, String), Refused>>>,
}

struct Refused {
    at: SystemTime,
    seals: BTreeSet<u64>,
}

impl SealedState {
    fn without_refused(&self, key: &str, who: &str, body: &[u8]) -> Option<Vec<u8>> {
        let seals = {
            let mut refused = self.refused.lock().expect("refused seals");
            let entry = refused.get_mut(&(key.to_owned(), who.to_owned()))?;
            if expired(entry.at, SystemTime::now()) {
                refused.remove(&(key.to_owned(), who.to_owned()));
                return None;
            }
            entry.seals.clone()
        };
        rewrite(body, |seal| seals.contains(&seal))
    }

    fn refused(&self, key: &str, who: &str, body: &[u8], replacement: &[u8]) {
        let remaining = sealed_items(replacement);
        let seals = sealed_items(body)
            .difference(&remaining)
            .copied()
            .collect::<BTreeSet<_>>();
        if seals.is_empty() {
            return;
        }
        let now = SystemTime::now();
        let mut refused = self.refused.lock().expect("refused seals");
        let entry = refused
            .entry((key.to_owned(), who.to_owned()))
            .or_insert_with(|| Refused {
                at: now,
                seals: BTreeSet::new(),
            });
        entry.at = now;
        entry.seals.extend(seals);
        if refused.len() > 4096 {
            refused.retain(|_, entry| !expired(entry.at, now));
        }
    }
}

pub(crate) struct SealedHook {
    state: SealedState,
    key: String,
    body: Vec<u8>,
    inner: Option<Arc<dyn AttemptHook>>,
    retried: Mutex<BTreeSet<String>>,
}

impl SealedHook {
    pub(crate) fn new(
        state: SealedState,
        conversation: &Conversation,
        inner: Option<Arc<dyn AttemptHook>>,
    ) -> Self {
        Self {
            state,
            key: conversation.key.clone(),
            body: conversation.body.clone(),
            inner,
            retried: Mutex::new(BTreeSet::new()),
        }
    }
}

impl AttemptHook for SealedHook {
    fn before(&self, attempt: &Attempt) -> Result<(), crate::AttemptError> {
        self.inner
            .as_ref()
            .map_or(Ok(()), |inner| inner.before(attempt))
    }

    fn prepare(&self, attempt: &Attempt, body: &[u8]) -> Option<Vec<u8>> {
        let prepared = self
            .inner
            .as_ref()
            .and_then(|inner| inner.prepare(attempt, body));
        let body = prepared.as_deref().unwrap_or(body);
        self.state
            .without_refused(&self.key, &who(attempt), body)
            .or(prepared)
    }

    fn refused(&self, attempt: &Attempt, status: u16, body: &[u8]) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.refused(attempt, status, body))
    }

    fn retry(&self, attempt: &Attempt, _status: u16, body: &[u8]) -> Option<Vec<u8>> {
        if let Some(replacement) = self
            .inner
            .as_ref()
            .and_then(|inner| inner.retry(attempt, _status, body))
        {
            return Some(replacement);
        }
        let text = String::from_utf8_lossy(body);
        if !foreign_reasoning(&text) {
            return None;
        }
        let who = who(attempt);
        {
            let mut retried = self.retried.lock().expect("sealed retries");
            if !retried.insert(who.clone()) {
                return None;
            }
        }
        let replacement = without_sealed(&self.body)?;
        self.state
            .refused(&self.key, &who, &self.body, &replacement);
        Some(replacement)
    }

    fn answered(&self, attempt: &Attempt) {
        if let Some(inner) = &self.inner {
            inner.answered(attempt);
        }
    }

    fn failed_handoff(
        &self,
        attempt: &Attempt,
        status: u16,
        billed: hofund_plugin_api::BilledUsage,
    ) {
        if let Some(inner) = &self.inner {
            inner.failed_handoff(attempt, status, billed);
        }
    }
}

fn who(attempt: &Attempt) -> String {
    if attempt.member.route.is_empty() {
        attempt.member.provider_id.clone()
    } else {
        attempt.member.route.clone()
    }
}

fn without_sealed(body: &[u8]) -> Option<Vec<u8>> {
    rewrite(body, |_| true)
}

fn rewrite(body: &[u8], refused: impl Fn(u64) -> bool) -> Option<Vec<u8>> {
    let mut value = serde_json::from_slice::<Value>(body).ok()?;
    let input = value.get_mut("input")?.as_array_mut()?;
    let mut changed = false;
    let mut kept = Vec::with_capacity(input.len());
    for item in input.drain(..) {
        if sealed_item(&item).is_some_and(|(_, seal)| refused(seal)) {
            changed = true;
            continue;
        }
        if let Some(replacement) = without_sealed_agent_message(&item, &refused) {
            changed = true;
            kept.push(replacement);
        } else {
            kept.push(item);
        }
    }
    if !changed {
        return None;
    }
    value["input"] = Value::Array(kept);
    serde_json::to_vec(&value).ok()
}

fn sealed_items(body: &[u8]) -> BTreeSet<u64> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return BTreeSet::new();
    };
    value
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|item| {
            let mut seals = Vec::new();
            if let Some((_, seal)) = sealed_item(item) {
                seals.push(seal);
            }
            if let Some(agent) = agent_message(item) {
                for part in agent
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if part.get("type").and_then(Value::as_str) == Some("encrypted_content")
                        && let Some(encrypted) =
                            part.get("encrypted_content").and_then(Value::as_str)
                        && !encrypted.is_empty()
                    {
                        seals.push(seal_hash(encrypted));
                    }
                }
            }
            seals
        })
        .collect()
}

fn sealed_item(item: &Value) -> Option<(&str, u64)> {
    let kind = item.get("type")?.as_str()?;
    if !SEALED_KINDS.contains(&kind) {
        return None;
    }
    let encrypted = item.get("encrypted_content")?.as_str()?;
    if encrypted.is_empty() {
        return None;
    }
    Some((encrypted, seal_hash(encrypted)))
}

fn agent_message(item: &Value) -> Option<&Value> {
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") => Some(item),
        Some("message") => {
            let parts = item.get("content")?.as_array()?;
            let [part] = parts.as_slice() else {
                return None;
            };
            (part.get("type").and_then(Value::as_str) == Some("agent_message")).then_some(part)
        }
        _ => None,
    }
}

fn without_sealed_agent_message(item: &Value, refused: &impl Fn(u64) -> bool) -> Option<Value> {
    let agent = agent_message(item)?;
    let parts = agent.get("content")?.as_array()?;
    let message = parts.iter().any(|part| {
        part.get("type").and_then(Value::as_str) == Some("input_text")
            && part
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.starts_with("Message Type: MESSAGE\n"))
    });
    if !message {
        return None;
    }
    let matching = |part: &Value| {
        part.get("type").and_then(Value::as_str) == Some("encrypted_content")
            && part
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|seal| !seal.is_empty() && refused(seal_hash(seal)))
    };
    if !parts.iter().any(&matching) {
        return None;
    }
    let author = agent
        .get("author")
        .and_then(Value::as_str)
        .filter(|author| !author.is_empty())
        .unwrap_or("another agent");
    let note = format!(
        "[This message from {author} was sealed by the server that answered that agent, and only that server can open it, so it was left out here. An agent's final result still reaches the agent that spawned it as text.]"
    );
    let mut replacement = agent.clone();
    replacement["content"] = Value::Array(
        parts
            .iter()
            .map(|part| {
                if matching(part) {
                    serde_json::json!({"type": "input_text", "text": note})
                } else {
                    part.clone()
                }
            })
            .collect(),
    );
    if item.get("type").and_then(Value::as_str) == Some("agent_message") {
        return Some(replacement);
    }
    let mut wrapper = item.clone();
    wrapper["content"] = Value::Array(vec![replacement]);
    Some(wrapper)
}

fn seal_hash(encrypted: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    encrypted.hash(&mut hasher);
    hasher.finish()
}

fn foreign_reasoning(text: &str) -> bool {
    static EXPRESSION: OnceLock<regex::Regex> = OnceLock::new();
    EXPRESSION
        .get_or_init(|| {
            regex::Regex::new(
                r"(?i)invalid_encrypted_content|invalid[ _]encrypted[ _](summary|content)|encrypted[ _]content.{0,80}could not be (verified|decrypted)|could not (decrypt|verify).{0,40}encrypted[ _]content",
            )
            .expect("foreign reasoning expression")
        })
        .is_match(text)
}

fn expired(at: SystemTime, now: SystemTime) -> bool {
    now.duration_since(at).unwrap_or(Duration::ZERO) > STICK_KEEP
}

#[cfg(test)]
mod tests {
    use hofund_codec::{Codec, OpenAiResponses};
    use serde_json::Value;

    use super::{SealedHook, SealedState, foreign_reasoning, without_sealed};
    use crate::affinity::Conversation;
    use crate::{Attempt, AttemptHook, MemberAttempt};

    fn attempt() -> Attempt {
        Attempt {
            member: MemberAttempt {
                provider_id: "second".to_owned(),
                ..MemberAttempt::default()
            },
            inbound_protocol: "responses".to_owned(),
        }
    }

    fn body() -> Vec<u8> {
        br#"{"model":"demo","input":[{"type":"reasoning","encrypted_content":"old-seal"},{"type":"compaction","encrypted_content":"old-compaction"},{"type":"message","role":"user","content":"next"}]}"#.to_vec()
    }

    #[test]
    fn cs_sealed_reasoning_retries_once_and_leaves_new_seals() {
        let state = SealedState::default();
        let conversation = Conversation::new("demo|session", 2, false).with_body(body());
        let hook = SealedHook::new(state.clone(), &conversation, None);
        let replacement = hook
            .retry(
                &attempt(),
                400,
                br#"{"error":{"message":"invalid encrypted summary"}}"#,
            )
            .expect("retry without foreign seals");
        let value: serde_json::Value = serde_json::from_slice(&replacement).expect("json");
        let input = value["input"].as_array().expect("input");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert!(
            hook.retry(&attempt(), 400, b"invalid_encrypted_content")
                .is_none()
        );

        let later = br#"{"input":[{"type":"reasoning","encrypted_content":"old-seal"},{"type":"reasoning","encrypted_content":"new-seal"},{"type":"compaction_summary","encrypted_content":"old-compaction"},{"type":"message","role":"user","content":"next"}]}"#;
        let prepared = hook
            .prepare(&attempt(), later)
            .expect("known seals removed");
        let value: serde_json::Value = serde_json::from_slice(&prepared).expect("json");
        let input = value["input"].as_array().expect("input");
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["encrypted_content"], "new-seal");
        assert_eq!(input[1]["type"], "message");
        assert!(hook.prepare(&attempt(), &replacement).is_none());
    }

    #[test]
    fn cs_foreign_reasoning_wordings_and_item_bounds() {
        for wording in [
            "invalid_encrypted_content",
            "invalid encrypted summary",
            "invalid encrypted content",
            "encrypted content for item rs_1 could not be verified",
            "encrypted_content could not be decrypted",
            "Could not decrypt the provided encrypted_content.",
            "could not verify this encrypted content",
        ] {
            assert!(foreign_reasoning(wording), "{wording}");
        }
        assert!(!foreign_reasoning("invalid request"));
        assert!(without_sealed(br#"{"input":"plain"}"#).is_none());
        assert!(without_sealed(br#"{"messages":[]}"#).is_none());
        let hook = SealedHook::new(
            SealedState::default(),
            &Conversation::new("demo|empty", 1, false).with_body(br#"{"input":[]}"#.to_vec()),
            None,
        );
        assert!(
            hook.retry(&attempt(), 400, b"invalid_encrypted_content")
                .is_none()
        );
    }

    #[test]
    fn d272_foreign_agent_message_seal_is_replaced_and_remembered() {
        let message = serde_json::json!({
            "type": "agent_message",
            "id": "amsg_old",
            "author": "/root/sub",
            "recipient": "/root",
            "content": [
                {"type": "input_text", "text": "Message Type: MESSAGE\nTask name: /root\nSender: /root/sub\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": "old-agent-seal"}
            ]
        });
        let task = serde_json::json!({
            "type": "agent_message",
            "id": "amsg_task",
            "author": "/root",
            "recipient": "/root/sub",
            "content": [
                {"type": "input_text", "text": "Message Type: NEW_TASK\nTask name: /root/sub\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": "sealed-task"}
            ]
        });
        let original = serde_json::json!({"input": [message.clone(), task.clone()]});
        let body = serde_json::to_vec(&original).expect("body");
        let hook = SealedHook::new(
            SealedState::default(),
            &Conversation::new("demo|agent", 2, false).with_body(body),
            None,
        );
        let replacement = hook
            .retry(&attempt(), 400, b"invalid_encrypted_content on amsg_old")
            .expect("retry without the foreign agent message seal");
        let replacement: serde_json::Value =
            serde_json::from_slice(&replacement).expect("replacement");
        assert_eq!(replacement["input"][0]["id"], "amsg_old");
        assert_eq!(replacement["input"][0]["content"][0], message["content"][0]);
        assert_eq!(replacement["input"][0]["content"][1]["type"], "input_text");
        let note = replacement["input"][0]["content"][1]["text"]
            .as_str()
            .expect("note");
        assert!(note.contains("/root/sub"));
        assert!(note.contains("left out"));
        assert!(note.contains("final result"));
        assert_eq!(replacement["input"][1], task);

        let mut current = message;
        current["content"][1]["encrypted_content"] = "new-agent-seal".into();
        current["id"] = "amsg_new".into();
        let later = serde_json::to_vec(&serde_json::json!({"input": [
            original["input"][0].clone(), current.clone(), task
        ]}))
        .expect("later body");
        let prepared = hook
            .prepare(&attempt(), &later)
            .expect("the refused seal is remembered");
        let prepared: serde_json::Value = serde_json::from_slice(&prepared).expect("prepared");
        assert_eq!(prepared["input"][0]["content"][1]["type"], "input_text");
        assert_eq!(prepared["input"][1], current);
        assert_eq!(
            prepared["input"][2]["content"][1]["encrypted_content"],
            "sealed-task"
        );

        let other = Attempt {
            member: MemberAttempt {
                provider_id: "first".to_owned(),
                ..MemberAttempt::default()
            },
            inbound_protocol: "responses".to_owned(),
        };
        assert!(hook.prepare(&other, &later).is_none());
    }

    #[test]
    fn d272_sealed_new_task_alone_does_not_retry() {
        let body = serde_json::to_vec(&serde_json::json!({"input": [{
            "type": "agent_message",
            "content": [
                {"type": "input_text", "text": "Message Type: NEW_TASK\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": "sealed-task"}
            ]
        }]}))
        .expect("body");
        let hook = SealedHook::new(
            SealedState::default(),
            &Conversation::new("demo|task", 2, false).with_body(body),
            None,
        );
        assert!(
            hook.retry(&attempt(), 400, b"invalid_encrypted_content")
                .is_none()
        );
    }

    fn d272_message(id: &str, seal: &str) -> Value {
        serde_json::json!({
            "type": "agent_message",
            "id": id,
            "author": "/root/sub",
            "recipient": "/root",
            "content": [
                {"type": "input_text", "text": "Message Type: MESSAGE\nTask name: /root\nSender: /root/sub\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": seal}
            ]
        })
    }

    fn d272_task() -> Value {
        serde_json::json!({
            "type": "agent_message",
            "id": "amsg_task",
            "author": "/root",
            "recipient": "/root/sub",
            "content": [
                {"type": "input_text", "text": "Message Type: NEW_TASK\nTask name: /root/sub\nPayload:\n"},
                {"type": "encrypted_content", "encrypted_content": "sealed-task"}
            ]
        })
    }

    fn d272_agent_item(item: &Value) -> Option<&Value> {
        if item.get("type").and_then(Value::as_str) == Some("agent_message") {
            return Some(item);
        }
        let parts = item.get("content")?.as_array()?;
        let [part] = parts.as_slice() else {
            return None;
        };
        (part.get("type").and_then(Value::as_str) == Some("agent_message")).then_some(part)
    }

    fn d272_agent_with_id<'a>(body: &'a Value, id: &str) -> &'a Value {
        body["input"]
            .as_array()
            .expect("input")
            .iter()
            .filter_map(d272_agent_item)
            .find(|item| item["id"] == id)
            .expect("agent item")
    }

    #[test]
    fn d272_prepare_handles_codec_wrapped_agent_messages() {
        let old = d272_message("amsg_old", "old-agent-seal");
        let task = d272_task();
        let original = serde_json::to_vec(&serde_json::json!({
            "model": "demo",
            "input": [old.clone(), task.clone()]
        }))
        .expect("original");
        let hook = SealedHook::new(
            SealedState::default(),
            &Conversation::new("demo|wrapped", 2, false).with_body(original),
            None,
        );
        hook.retry(&attempt(), 400, b"invalid_encrypted_content")
            .expect("initial retry");

        let new = d272_message("amsg_new", "new-agent-seal");
        let later = serde_json::to_vec(&serde_json::json!({
            "model": "demo",
            "input": [old, new, task]
        }))
        .expect("later");
        let request = OpenAiResponses
            .decode_request(&later)
            .expect("decode request");
        let encoded = OpenAiResponses
            .encode_request(&request)
            .expect("encode request");
        let wrapped: Value = serde_json::from_slice(&encoded).expect("encoded body");
        assert_eq!(wrapped["input"][0]["type"], "message");
        assert_eq!(d272_agent_with_id(&wrapped, "amsg_old")["id"], "amsg_old");

        let prepared = hook
            .prepare(&attempt(), &encoded)
            .expect("wrapped refused seal removed");
        let prepared: Value = serde_json::from_slice(&prepared).expect("prepared body");
        let old = d272_agent_with_id(&prepared, "amsg_old");
        assert_eq!(old["content"][1]["type"], "input_text");
        assert!(
            old["content"][1]["text"]
                .as_str()
                .is_some_and(|note| note.contains("left out"))
        );
        assert_eq!(
            d272_agent_with_id(&prepared, "amsg_new")["content"][1]["encrypted_content"],
            "new-agent-seal"
        );
        assert_eq!(
            d272_agent_with_id(&prepared, "amsg_task")["content"][1]["encrypted_content"],
            "sealed-task"
        );
    }

    #[test]
    fn d272_kept_new_task_seal_is_not_recorded_as_refused() {
        let original = serde_json::to_vec(&serde_json::json!({
            "input": [d272_message("amsg_old", "old-agent-seal"), d272_task()]
        }))
        .expect("original");
        let hook = SealedHook::new(
            SealedState::default(),
            &Conversation::new("demo|removed-only", 2, false).with_body(original),
            None,
        );
        hook.retry(&attempt(), 400, b"invalid_encrypted_content")
            .expect("initial retry");

        let later = br#"{"input":[{"type":"reasoning","encrypted_content":"sealed-task"}]}"#;
        assert!(hook.prepare(&attempt(), later).is_none());
    }
}
