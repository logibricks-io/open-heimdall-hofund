//! Responses turns on a kept WebSocket when the model list says so.
//!
//! A model whose `supported_endpoints` include `ws:/responses` sends
//! `response.create` on a socket at `<api>/responses`. The next turn of the
//! same conversation and session token sends `previous_response_id` and only
//! the new input. A refused handshake, a clean close before a terminal event,
//! a read that sits past the host's read budget, or an error frame that names
//! no HTTP status, uses the HTTP path instead. The host clock closes an idle
//! socket and one that has reached its age, and each account keeps a bounded
//! number of them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use hofund_plugin_api::{
    AccountId, BodyStream, Completion, ContentPart, HostContext, PluginError, ProtocolId, Request,
    WebSocketMessage, WebSocketRequest, WebSocketSession,
};
use hofund_plugin_sdk::digest;
use hofund_plugin_sdk::json::{self, Map, Value};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::transport::HttpResponse;
use hofund_plugin_sdk::wire::{self, Target};

use crate::models::Book;
use crate::session::Session;
use crate::upstream::initiator;

/// How long a kept socket waits for its next turn.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// How long a socket is used, from its handshake.
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// How many sockets one account keeps.
pub(crate) const MAX_PER_ACCOUNT: usize = 32;
/// Largest assembled turn. A larger reply goes over HTTP.
const MAX_TURN: usize = 8 * 1024 * 1024;

/// A turn that should go over HTTP, or a failure whose status must stand.
pub(crate) enum TurnFail {
    Fallback,
    Failed(PluginError),
}

/// Sockets one subscription keeps, keyed by account.
#[derive(Default)]
pub(crate) struct Pool {
    inner: Mutex<HashMap<AccountId, Vec<Kept>>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Pool").finish_non_exhaustive()
    }
}

struct Kept {
    key: String,
    session: WebSocketSession,
    opened: Duration,
    used: Duration,
    fields: BTreeMap<String, String>,
    input: Vec<[u8; 32]>,
    output: Vec<[u8; 32]>,
    sealed: Vec<bool>,
    reply_id: String,
}

struct Shape {
    fields: BTreeMap<String, String>,
    sums: Vec<[u8; 32]>,
}

struct Read {
    sse: Vec<u8>,
    output: Vec<[u8; 32]>,
    sealed: Vec<bool>,
    reply_id: String,
    track: bool,
}

impl Pool {
    pub(crate) fn close_idle(&self, account: &AccountId, now: Duration) {
        self.reap(account, now);
    }

    /// How many sockets `account` still holds.
    #[cfg(test)]
    pub(crate) fn held(&self, account: &AccountId) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(account)
            .map(Vec::len)
            .unwrap_or(0)
    }

    fn reap(&self, account: &AccountId, now: Duration) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(list) = guard.get_mut(account) else {
            return;
        };
        list.retain(|kept| fresh(kept, now));
        if list.is_empty() {
            guard.remove(account);
        }
    }

    fn take(&self, account: &AccountId, key: &str, now: Duration) -> Option<Kept> {
        self.reap(account, now);
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let list = guard.get_mut(account)?;
        let index = list.iter().position(|kept| kept.key == key)?;
        let kept = list.remove(index);
        if list.is_empty() {
            guard.remove(account);
        }
        fresh(&kept, now).then_some(kept)
    }

    fn put(&self, account: AccountId, kept: Kept) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let list = guard.entry(account).or_default();
        if let Some(index) = list.iter().position(|item| item.key == kept.key) {
            list.remove(index);
        }
        list.push(kept);
        while list.len() > MAX_PER_ACCOUNT {
            let oldest = list
                .iter()
                .enumerate()
                .min_by_key(|(_, item)| item.used)
                .map(|(index, _)| index)
                .unwrap_or(0);
            list.remove(oldest);
        }
    }
}

fn fresh(kept: &Kept, now: Duration) -> bool {
    now.saturating_sub(kept.used) < IDLE && now.saturating_sub(kept.opened) < MAX_AGE
}

/// Sends `request` over the Responses socket, or says to use HTTP.
pub(crate) async fn try_responses(
    book: &Book,
    host: &HostContext,
    account: &AccountId,
    session: &Session,
    headers: &[(String, String)],
    request: &Request,
    inbound: ProtocolId,
) -> Result<Completion, TurnFail> {
    let now = host.clock().monotonic();
    book.sockets.close_idle(account, now);
    let Ok(body) = encoded(session, request, inbound) else {
        return Err(TurnFail::Fallback);
    };
    let Some(value) = json::from_slice::<Value>(&body).ok() else {
        return Err(TurnFail::Fallback);
    };
    let shape = shape_of(&value);
    let url = match responses_url(&session.base) {
        Ok(url) => url,
        Err(_) => return Err(TurnFail::Fallback),
    };
    let bearer = secret_text(&session.token);
    let auto = header_value(headers, "Copilot-Session-Token");
    let key = keep_key(&url, bearer, auto, &conversation_key(request));
    let taken = book.sockets.take(account, &key, now);
    let mut outbound = value.clone();
    let mut opened_at = now;
    let mut live = if let Some(kept) = taken {
        if let Some(delta) = shape
            .as_ref()
            .and_then(|shape| delta_body(&kept, &value, shape))
        {
            outbound = delta;
        }
        opened_at = kept.opened;
        kept.session
    } else {
        match host
            .http()
            .open_websocket(handshake(&url, headers, bearer))
            .await
        {
            Ok(session) => session,
            Err(_) => return Err(TurnFail::Fallback),
        }
    };
    let message = response_create(&outbound, initiator(request));
    if live.send_text(message).await.is_err() {
        return Err(TurnFail::Fallback);
    }
    let read = match read_turn(&mut live).await {
        Ok(read) => read,
        Err(error) => return Err(error),
    };
    if let Some(shape) = shape.filter(|_| !read.reply_id.is_empty() && read.track) {
        book.sockets.put(
            account.clone(),
            Kept {
                key,
                session: live,
                opened: opened_at,
                used: now,
                fields: shape.fields,
                input: shape.sums,
                output: read.output,
                sealed: read.sealed,
                reply_id: read.reply_id,
            },
        );
    }
    decode_sse(read.sse, inbound, request.stream)
        .await
        .map_err(TurnFail::Failed)
}

fn encoded(
    session: &Session,
    request: &Request,
    inbound: ProtocolId,
) -> Result<Vec<u8>, PluginError> {
    let target = Target::new(ProtocolId::OPENAI_RESPONSES, session.base.clone());
    wire::prepare_outbound(&target, request.clone(), inbound)
}

fn handshake(url: &str, headers: &[(String, String)], bearer: &str) -> WebSocketRequest {
    let mut request = WebSocketRequest::get(url)
        .lifetime(IDLE, MAX_AGE)
        .sensitive_header("Authorization", format!("Bearer {bearer}"));
    for (name, value) in headers {
        if skip_handshake(name) || name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        request = if name.eq_ignore_ascii_case("Copilot-Session-Token") {
            request.sensitive_header(name, value)
        } else {
            request.header(name, value)
        };
    }
    request
}

fn skip_handshake(name: &str) -> bool {
    name.eq_ignore_ascii_case("x-initiator")
        || name.eq_ignore_ascii_case("openai-beta")
        || name.eq_ignore_ascii_case("content-type")
        || name.eq_ignore_ascii_case("content-length")
        || name.eq_ignore_ascii_case("accept")
        || name.eq_ignore_ascii_case("host")
        || name.eq_ignore_ascii_case("connection")
}

async fn read_turn(session: &mut WebSocketSession) -> Result<Read, TurnFail> {
    let mut read = Read {
        sse: Vec::new(),
        output: Vec::new(),
        sealed: Vec::new(),
        reply_id: String::new(),
        track: true,
    };
    let mut saw_content = false;
    let mut terminal = false;
    loop {
        let message = session.recv().await.map_err(|_| TurnFail::Fallback)?;
        let Some(WebSocketMessage::Text(text)) = message else {
            if message.is_none() {
                break;
            }
            continue;
        };
        let Ok(value) = json::from_str::<Value>(&text) else {
            if !fits(&read.sse, &text) {
                return Err(TurnFail::Fallback);
            }
            push_sse(&mut read.sse, &text);
            continue;
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "responsesapi.websocket_timing" {
            continue;
        }
        if kind == "error" && !saw_content {
            return Err(classify_error(&value));
        }
        if kind == "response.output_item.done" {
            saw_content = true;
            note_item(&value, &mut read);
        }
        if kind == "response.completed"
            && let Some(id) = value
                .pointer("/response/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        {
            read.reply_id = id.to_owned();
        }
        if !fits(&read.sse, &text) {
            return Err(TurnFail::Fallback);
        }
        push_sse(&mut read.sse, &text);
        if matches!(
            kind,
            "response.completed"
                | "response.failed"
                | "response.incomplete"
                | "response.done"
                | "error"
        ) {
            terminal = true;
            break;
        }
    }
    if !terminal {
        return Err(TurnFail::Fallback);
    }
    Ok(read)
}

fn fits(sse: &[u8], frame: &str) -> bool {
    sse.len().saturating_add(frame.len()).saturating_add(32) <= MAX_TURN
}

fn note_item(value: &Value, read: &mut Read) {
    let Some(item) = value.get("item") else {
        read.track = false;
        return;
    };
    let Some(sum) = item_sum(item) else {
        read.track = false;
        return;
    };
    read.sealed
        .push(item.get("type").and_then(Value::as_str) == Some("reasoning"));
    read.output.push(sum);
}

fn classify_error(value: &Value) -> TurnFail {
    if value.pointer("/error/code").and_then(Value::as_str)
        == Some("websocket_connection_limit_reached")
    {
        return TurnFail::Fallback;
    }
    let Some(status) = error_status(value) else {
        return TurnFail::Fallback;
    };
    let body = json::to_vec(value).unwrap_or_default();
    TurnFail::Failed(wire::failure_from_body(status, &body))
}

fn error_status(value: &Value) -> Option<u16> {
    let status = value
        .get("status")
        .and_then(Value::as_u64)
        .or_else(|| value.get("status_code").and_then(Value::as_u64))?;
    u16::try_from(status)
        .ok()
        .filter(|status| (400..=599).contains(status))
}

fn push_sse(out: &mut Vec<u8>, frame: &str) {
    let compact = json::from_str::<Value>(frame)
        .ok()
        .and_then(|value| json::to_string(&value).ok())
        .unwrap_or_else(|| frame.to_owned());
    let kind = json::from_str::<Value>(&compact)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    if !kind.is_empty() {
        out.extend_from_slice(b"event: ");
        out.extend_from_slice(kind.as_bytes());
        out.push(b'\n');
    }
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(compact.as_bytes());
    out.extend_from_slice(b"\n\n");
}

async fn decode_sse(
    bytes: Vec<u8>,
    inbound: ProtocolId,
    stream: bool,
) -> Result<Completion, PluginError> {
    let response = HttpResponse::new(BodyStream::from_bytes(bytes));
    wire::decode_upstream_response(response, ProtocolId::OPENAI_RESPONSES, inbound, stream).await
}

fn responses_url(base: &str) -> Result<String, PluginError> {
    let url = format!("{}/responses", base.trim_end_matches('/'));
    if let Some(rest) = url.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = url.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else {
        Err(PluginError::config("the API base is not an HTTP URL"))
    }
}

fn response_create(body: &Value, initiator: &str) -> String {
    let mut object = Map::new();
    object.insert(
        "type".to_owned(),
        Value::String("response.create".to_owned()),
    );
    object.insert("initiator".to_owned(), Value::String(initiator.to_owned()));
    if let Some(fields) = body.as_object() {
        for (key, value) in fields {
            if matches!(key.as_str(), "stream" | "type" | "initiator") {
                continue;
            }
            object.insert(key.clone(), value.clone());
        }
    }
    json::to_string(&Value::Object(object)).unwrap_or_else(|_| "{}".to_owned())
}

fn shape_of(body: &Value) -> Option<Shape> {
    let object = body.as_object()?;
    if object.contains_key("previous_response_id") {
        return None;
    }
    let input = object.get("input")?.as_array()?.clone();
    let mut fields = BTreeMap::new();
    for (key, value) in object {
        if matches!(key.as_str(), "input" | "client_metadata" | "stream_options") {
            continue;
        }
        fields.insert(key.clone(), canonical(value)?);
    }
    let mut sums = Vec::with_capacity(input.len());
    for item in &input {
        sums.push(item_sum(item)?);
    }
    Some(Shape { fields, sums })
}

fn delta_body(kept: &Kept, body: &Value, shape: &Shape) -> Option<Value> {
    if kept.reply_id.is_empty()
        || kept.fields != shape.fields
        || shape.sums.len() < kept.input.len()
    {
        return None;
    }
    if shape.sums[..kept.input.len()] != kept.input[..] {
        return None;
    }
    let mut index = kept.input.len();
    for (slot, sum) in kept.output.iter().enumerate() {
        if index < shape.sums.len() && shape.sums[index] == *sum {
            index += 1;
            continue;
        }
        if kept.sealed.get(slot).copied().unwrap_or(false) {
            continue;
        }
        return None;
    }
    let mut next = body.clone();
    let object = next.as_object_mut()?;
    let input = object.get("input")?.as_array()?.clone();
    if index > input.len() {
        return None;
    }
    object.insert("input".to_owned(), Value::Array(input[index..].to_vec()));
    object.insert(
        "previous_response_id".to_owned(),
        Value::String(kept.reply_id.clone()),
    );
    Some(next)
}

fn item_sum(item: &Value) -> Option<[u8; 32]> {
    let mut object = item.as_object()?.clone();
    object.remove("id");
    object.remove("status");
    let bare = bare_value(&Value::Object(object));
    Some(digest::sha256(canonical(&bare)?.as_bytes()))
}

fn bare_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut kept = Map::new();
            for (key, child) in object {
                if key == "annotations" || key == "logprobs" || child.is_null() {
                    continue;
                }
                if child.as_array().is_some_and(Vec::is_empty) {
                    continue;
                }
                kept.insert(key.clone(), bare_value(child));
            }
            Value::Object(kept)
        }
        Value::Array(items) => Value::Array(items.iter().map(bare_value).collect()),
        other => other.clone(),
    }
}

fn canonical(value: &Value) -> Option<String> {
    json::to_string(&sorted(value)).ok()
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort();
            let mut kept = Map::new();
            for key in keys {
                kept.insert(key.clone(), sorted(&object[key]));
            }
            Value::Object(kept)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn keep_key(url: &str, bearer: &str, session_token: &str, conversation: &str) -> String {
    let mut bytes = Vec::new();
    for part in [url, bearer, session_token, conversation] {
        bytes.extend_from_slice(part.as_bytes());
        bytes.push(0);
    }
    hex(&digest::sha256(&bytes))
}

fn conversation_key(request: &Request) -> String {
    if let Some(conversation) = &request.conversation
        && (!conversation.session.is_empty() || !conversation.subagent.is_empty())
    {
        return format!("{}\0{}", conversation.session, conversation.subagent);
    }
    let opening = request.messages.iter().find_map(|message| {
        message.content.iter().find_map(|part| match part {
            ContentPart::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
    });
    match opening {
        Some(text) => format!("open:{}", hex(&digest::sha256(text.as_bytes()))),
        None => "open".to_owned(),
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}
