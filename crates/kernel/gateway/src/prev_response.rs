//! In-memory continuation history for Responses requests.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hofund_router::{Attempt, AttemptError, AttemptHook};
use serde_json::Value;

const CAP: usize = 256 << 20;
const TTL: Duration = Duration::from_secs(6 * 60 * 60);
const REPLY_CAP: usize = 64 << 20;

type Items = Arc<[Arc<Value>]>;
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Clone)]
pub(crate) struct PrevStore {
    inner: Arc<Mutex<State>>,
}

struct State {
    entries: HashMap<Key, Arc<Turn>>,
    order: VecDeque<Key>,
    size: usize,
    cap: usize,
    ttl: Duration,
    now: Clock,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    scope: String,
    id: String,
}

struct Turn {
    items: Items,
    own: usize,
    at: Instant,
    upstream_id: String,
    route: String,
}

pub(crate) struct Expansion {
    pub(crate) body: Vec<u8>,
    pub(crate) ask: Option<Arc<Ask>>,
    pub(crate) capture: Option<Capture>,
}

pub(crate) struct Ask {
    pub(crate) id: String,
    pub(crate) total: usize,
    pub(crate) fresh: usize,
    pub(crate) found: bool,
    pub(crate) upstream_id: String,
    pub(crate) route: String,
    store_true: bool,
    store_off: bool,
    answered: Mutex<Option<String>>,
}

pub(crate) struct Capture {
    store: PrevStore,
    scope: String,
    input: Items,
    own: usize,
    ask: Arc<Ask>,
}

pub(crate) struct PrevAttempt {
    ask: Arc<Ask>,
}

pub(crate) struct ReplyCapture {
    capture: Capture,
    bytes: Vec<u8>,
    cut: bool,
    limit: usize,
}

impl PrevStore {
    pub(crate) fn new() -> Self {
        Self::with(CAP, TTL, Arc::new(Instant::now))
    }

    fn with(cap: usize, ttl: Duration, now: Clock) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State {
                entries: HashMap::new(),
                order: VecDeque::new(),
                size: 0,
                cap,
                ttl,
                now,
            })),
        }
    }

    fn get(&self, scope: &str, id: &str) -> Option<Arc<Turn>> {
        let key = Key {
            scope: scope.to_owned(),
            id: id.to_owned(),
        };
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let now = (state.now)();
        let expired = state
            .entries
            .get(&key)
            .is_some_and(|turn| elapsed(now, turn.at) > state.ttl);
        if expired {
            state.drop_key(&key);
            return None;
        }
        let turn = state.entries.get(&key)?.clone();
        state.order.retain(|old| old != &key);
        state.order.push_front(key);
        Some(turn)
    }

    fn put(&self, scope: String, id: String, turn: Turn) {
        if turn.own
            > self
                .inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .cap
                / 2
        {
            return;
        }
        let key = Key { scope, id };
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        state.drop_key(&key);
        let now = (state.now)();
        state.size += turn.own;
        state
            .entries
            .insert(key.clone(), Arc::new(Turn { at: now, ..turn }));
        state.order.push_front(key);
        while state.size > state.cap {
            let Some(old) = state.order.back().cloned() else {
                break;
            };
            state.drop_key(&old);
        }
        loop {
            let expired = state.order.back().is_some_and(|old| {
                state
                    .entries
                    .get(old)
                    .is_some_and(|turn| elapsed(now, turn.at) > state.ttl)
            });
            if !expired {
                break;
            }
            let old = state.order.back().cloned().expect("old entry");
            state.drop_key(&old);
        }
    }

    pub(crate) fn expand(&self, scope: &str, body: &[u8]) -> Expansion {
        let original = body.to_vec();
        let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(body) else {
            return Expansion::unchanged(original);
        };
        let Some(fresh) = input_items(object.get("input")) else {
            return Expansion::unchanged(original);
        };
        let id = object
            .get("previous_response_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or_default()
            .to_owned();
        let prior = (!id.is_empty()).then(|| self.get(scope, &id)).flatten();
        let found = prior.is_some();
        let (upstream_id, route) = prior.as_ref().map_or_else(
            || (String::new(), String::new()),
            |turn| (turn.upstream_id.clone(), turn.route.clone()),
        );
        let mut input = prior
            .as_ref()
            .map_or_else(Vec::new, |turn| turn.items.to_vec());
        let prior_len = input.len();
        input.extend(fresh.iter().cloned());
        let total = input.len();
        if found {
            object.insert(
                "input".to_owned(),
                Value::Array(input.iter().map(|item| (**item).clone()).collect()),
            );
            object.remove("previous_response_id");
        }
        let store_true = object.get("store").and_then(Value::as_bool) == Some(true);
        let store_off = object.get("store").and_then(Value::as_bool) == Some(false);
        let body = if found {
            serde_json::to_vec(&Value::Object(object)).unwrap_or(original)
        } else {
            original
        };
        let own = if found {
            fresh.iter().map(item_bytes).sum()
        } else {
            body.len()
        };
        let input: Items = input.into();
        let ask = Arc::new(Ask {
            id,
            total: if found { total } else { 0 },
            fresh: if found { fresh.len() } else { 0 },
            found,
            upstream_id,
            route,
            store_true,
            store_off,
            answered: Mutex::new(None),
        });
        let capture = (!store_off).then(|| Capture {
            store: self.clone(),
            scope: scope.to_owned(),
            input,
            own,
            ask: Arc::clone(&ask),
        });
        debug_assert!(!found || prior_len + fresh.len() == total);
        Expansion {
            body,
            ask: Some(ask),
            capture,
        }
    }
}

impl State {
    fn drop_key(&mut self, key: &Key) {
        if let Some(turn) = self.entries.remove(key) {
            self.size = self.size.saturating_sub(turn.own);
        }
        self.order.retain(|old| old != key);
    }
}

impl Expansion {
    fn unchanged(body: Vec<u8>) -> Self {
        Self {
            body,
            ask: None,
            capture: None,
        }
    }
}

impl Ask {
    pub(crate) fn attempt(self: &Arc<Self>) -> Arc<dyn AttemptHook> {
        Arc::new(PrevAttempt {
            ask: Arc::clone(self),
        })
    }
}

impl AttemptHook for PrevAttempt {
    fn before(&self, attempt: &Attempt) -> Result<(), AttemptError> {
        if !self.ask.id.is_empty()
            && !self.ask.found
            && (!attempt.relays_responses() || attempt.member.subscription)
        {
            return Err(AttemptError::PreviousResponseNotFound {
                id: self.ask.id.clone(),
            });
        }
        Ok(())
    }

    fn prepare(&self, attempt: &Attempt, body: &[u8]) -> Option<Vec<u8>> {
        if !self.ask.found
            || self.ask.upstream_id.is_empty()
            || self.ask.route != attempt.member.route
            || !attempt.member.keeps_responses
            || !attempt.relays_responses()
            || !self.ask.store_true
        {
            return None;
        }
        let Value::Object(mut object) = serde_json::from_slice::<Value>(body).ok()? else {
            return None;
        };
        let items = object.get("input")?.as_array()?;
        if items.len() != self.ask.total || self.ask.fresh > items.len() {
            return None;
        }
        let fresh = items[items.len() - self.ask.fresh..].to_vec();
        object.insert("input".to_owned(), Value::Array(fresh));
        object.insert(
            "previous_response_id".to_owned(),
            Value::String(self.ask.upstream_id.clone()),
        );
        serde_json::to_vec(&Value::Object(object)).ok()
    }

    fn refused(&self, _attempt: &Attempt, status: u16, body: &[u8]) -> bool {
        if !matches!(status, 400 | 404 | 422) {
            return false;
        }
        let text = String::from_utf8_lossy(body).to_ascii_lowercase();
        text.contains("previous_response") || text.contains("previousresponse")
    }

    fn answered(&self, attempt: &Attempt) {
        if attempt.relays_responses() && attempt.member.keeps_responses && !self.ask.store_off {
            *self
                .ask
                .answered
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(attempt.member.route.clone());
        }
    }
}

impl ReplyCapture {
    pub(crate) fn new(capture: Option<Capture>) -> Option<Self> {
        capture.map(|capture| Self {
            capture,
            bytes: Vec::new(),
            cut: false,
            limit: REPLY_CAP,
        })
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
        if self.cut {
            return;
        }
        if chunk.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.cut = true;
            self.bytes = Vec::new();
            return;
        }
        self.bytes.extend_from_slice(chunk);
    }

    pub(crate) fn finish(mut self) {
        if self.cut || self.bytes.is_empty() {
            return;
        }
        let Some((id, output)) = parse_reply(&self.bytes) else {
            return;
        };
        self.capture.own += output.iter().map(item_bytes).sum::<usize>();
        let mut items = self.capture.input.to_vec();
        items.extend(output);
        let route = self
            .capture
            .ask
            .answered
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap_or_default();
        let upstream_id = if route.is_empty() {
            String::new()
        } else {
            id.clone()
        };
        self.capture.store.put(
            self.capture.scope,
            id,
            Turn {
                items: items.into(),
                own: self.capture.own,
                at: Instant::now(),
                upstream_id,
                route,
            },
        );
        self.bytes.clear();
    }
}

fn input_items(value: Option<&Value>) -> Option<Vec<Arc<Value>>> {
    match value? {
        Value::String(text) => Some(vec![Arc::new(serde_json::json!({
            "type": "message",
            "role": "user",
            "content": text,
        }))]),
        Value::Array(items) => Some(items.iter().cloned().map(Arc::new).collect()),
        _ => None,
    }
}

fn parse_reply(bytes: &[u8]) -> Option<(String, Vec<Arc<Value>>)> {
    let response = if looks_like_sse(bytes) {
        parse_sse_response(bytes)?
    } else {
        serde_json::from_slice::<Value>(bytes).ok()?
    };
    let object = response.as_object()?;
    let id = object.get("id")?.as_str()?.to_owned();
    if id.is_empty()
        || matches!(
            object.get("status").and_then(Value::as_str),
            Some("failed" | "cancelled")
        )
    {
        return None;
    }
    let output = object
        .get("output")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |items| {
            items.iter().cloned().map(Arc::new).collect()
        });
    Some((id, output))
}

fn looks_like_sse(bytes: &[u8]) -> bool {
    bytes.starts_with(b"event:")
        || bytes.starts_with(b"data:")
        || bytes.starts_with(b":")
        || bytes.starts_with(b"\n")
        || bytes.starts_with(b"\r\n")
}

fn parse_sse_response(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?.replace("\r\n", "\n");
    let mut streamed = BTreeMap::<usize, Value>::new();
    let mut response = None;
    for frame in text.split("\n\n") {
        let mut event = "";
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(value) = line.strip_prefix("event:") {
                event = value.trim_start();
            } else if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.trim_start());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or(event);
        match kind {
            "response.output_item.done" => {
                if let (Some(index), Some(item)) = (
                    value.get("output_index").and_then(Value::as_u64),
                    value.get("item"),
                ) {
                    streamed.insert(index as usize, item.clone());
                }
            }
            "response.completed" | "response.incomplete" => {
                response = value.get("response").cloned();
            }
            _ => {}
        }
    }
    let mut response = response?;
    if let Some(object) = response.as_object_mut() {
        let mut output: BTreeMap<usize, Value> = object
            .get("output")
            .and_then(Value::as_array)
            .map(|items| items.iter().cloned().enumerate().collect())
            .unwrap_or_default();
        output.extend(streamed);
        object.insert(
            "output".to_owned(),
            Value::Array(output.into_values().collect()),
        );
    }
    Some(response)
}

fn item_bytes(item: &Arc<Value>) -> usize {
    serde_json::to_vec(item.as_ref()).map_or(0, |bytes| bytes.len())
}

fn elapsed(now: Instant, then: Instant) -> Duration {
    now.checked_duration_since(then).unwrap_or_default()
}

pub(crate) fn caller_scope(
    key_id: Option<&str>,
    via: Option<IpAddr>,
    peer: Option<IpAddr>,
) -> String {
    let mut scope = format!("key:{}", key_id.unwrap_or_default());
    if let Some(via) = via {
        scope.push_str(&format!("\0via:{via}"));
    }
    if key_id.is_none() && peer.is_some_and(|ip| !ip.is_loopback()) {
        scope.push_str(&format!("\0ip:{}", peer.expect("peer")));
    }
    scope
}

pub(crate) fn request_scope() -> String {
    caller_scope(Some("default"), None, None)
}

pub(crate) fn not_found_body(id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "error": {
            "message": format!("Previous response with id '{id}' not found."),
            "type": "invalid_request_error",
            "param": "previous_response_id",
            "code": "previous_response_not_found",
        }
    }))
    .unwrap_or_else(|_| b"{\"error\":{\"code\":\"previous_response_not_found\"}}".to_vec())
}

#[cfg(test)]
mod tests;
