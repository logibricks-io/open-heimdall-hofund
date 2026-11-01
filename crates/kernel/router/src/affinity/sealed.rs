//! Recovery when a member cannot read another member's sealed reasoning.

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

    fn refused(&self, key: &str, who: &str, body: &[u8]) {
        let seals = sealed_items(body)
            .into_iter()
            .map(|(_, seal)| seal)
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
        self.state.refused(&self.key, &who, &self.body);
        Some(replacement)
    }

    fn answered(&self, attempt: &Attempt) {
        if let Some(inner) = &self.inner {
            inner.answered(attempt);
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
    let before = input.len();
    input.retain(|item| {
        let Some((_, seal)) = sealed_item(item) else {
            return true;
        };
        !refused(seal)
    });
    if input.len() == before {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

fn sealed_items(body: &[u8]) -> Vec<(String, u64)> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return Vec::new();
    };
    value
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let (encrypted, seal) = sealed_item(item)?;
            Some((encrypted.to_owned(), seal))
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
    let mut hasher = DefaultHasher::new();
    encrypted.hash(&mut hasher);
    Some((encrypted, hasher.finish()))
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
}
