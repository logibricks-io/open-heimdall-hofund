//! Resume, idle reuse, session shelve, and retire (reference stages C and E).
//!
//! The idle/resume map key is a SHA-256 `turnKey` over the account, model,
//! effort/think-off bits, tool choice, system prompt, tools, and messages
//! (tool-call ids excluded). A one-off turn (stop, no tools, no prior
//! assistant reply) is aborted, not idled. Effort bits follow `hofund_bridge_effort`; `setEffort` and
//! `lostMedia` are applied in this slice.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hofund_plugin_api::{AccountId, ContentPart, Message, Request, Role};
use regex::Regex;
use ring::digest::{Context, SHA256};

use super::park::Bridge;

/// How many idle runs are kept (reference `idleMost`).
pub(crate) const IDLE_MOST: usize = 6;
/// How long an idle or shelved session is kept (reference `idleLongest`).
pub(crate) const IDLE_LONGEST: Duration = Duration::from_secs(60 * 60);
/// How many shelved sessions are kept (reference `shelfMost`).
pub(crate) const SHELF_MOST: usize = 64;

/// A successful idle resume: the run and the messages since the last reply.
#[derive(Debug, Clone)]
pub(crate) struct Resume {
    /// Idle run id taken off the idle map.
    pub run_id: u64,
    /// Messages after the last non-tool assistant reply.
    pub since: Vec<Message>,
}

/// A Claude Code session kept after its idle run was evicted.
#[derive(Debug, Clone)]
pub(crate) struct SavedSession {
    /// Claude Code session id, for `--resume`.
    pub session: String,
    /// Project folders the session was saved in (may be empty in tests).
    #[allow(dead_code)]
    pub sessions: Vec<PathBuf>,
    /// Account that owned the run.
    pub owner: String,
    /// Conversation key (model-independent).
    pub conv: String,
    /// `turnKey` under which it is shelved.
    pub key: String,
    /// When the run first went idle.
    pub at: Instant,
}

/// Per-run resume/idle bookkeeping, keyed by the same id as a parked run.
#[derive(Debug, Default)]
struct RunMeta {
    owner: String,
    idle_key: Option<String>,
    idle_at: Option<Instant>,
    conv_key: Option<String>,
    session_id: Option<String>,
    session_paths: Vec<PathBuf>,
    told: String,
    told_messages: Vec<Message>,
    /// Effort last applied to this run (`--effort` or `setEffort`).
    effort: String,
    resuming: bool,
    closed: bool,
    segment: bool,
    /// Tools the run was started with (used by later aside/offers checks).
    #[allow(dead_code)]
    tools: Vec<String>,
    /// Room slot held for this process, released when the run is aborted.
    room: Option<super::room::RoomGuard>,
}

/// Idle map, shelf, and claim state for one [`Bridge`].
#[derive(Debug, Default)]
pub(crate) struct Sessions {
    idle: Mutex<HashMap<String, u64>>,
    shelf: Mutex<HashMap<String, SavedSession>>,
    meta: Mutex<HashMap<u64, RunMeta>>,
}

impl Sessions {
    fn lock_idle(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        self.idle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_shelf(&self) -> std::sync::MutexGuard<'_, HashMap<String, SavedSession>> {
        self.shelf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_meta(&self) -> std::sync::MutexGuard<'_, HashMap<u64, RunMeta>> {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn expire(&self, now: Instant) {
        let stale: Vec<(String, u64)> = self
            .lock_idle()
            .iter()
            .filter_map(|(key, id)| {
                let meta = self.lock_meta();
                let at = meta.get(id).and_then(|m| m.idle_at)?;
                (now.duration_since(at) > IDLE_LONGEST).then(|| (key.clone(), *id))
            })
            .collect();
        for (key, id) in stale {
            self.lock_idle().remove(&key);
            if let Some(meta) = self.lock_meta().get_mut(&id) {
                meta.idle_key = None;
                meta.closed = true;
                meta.room = None;
            }
        }
        self.lock_shelf()
            .retain(|_, saved| now.duration_since(saved.at) <= IDLE_LONGEST);
    }
}

impl Bridge {
    /// Ensures a run id has session metadata for `owner`.
    pub(crate) fn ensure_session_run(
        &self,
        run_id: u64,
        owner: &AccountId,
        tool_names: Vec<String>,
    ) {
        let mut meta = self.sessions.lock_meta();
        meta.entry(run_id).or_insert_with(|| RunMeta {
            owner: owner.as_str().to_owned(),
            tools: tool_names,
            ..RunMeta::default()
        });
    }

    /// Records the conversation the run last heard, for [`Bridge::follows`].
    pub(crate) fn heard(&self, run_id: u64, messages: &[Message]) {
        let told = history_key(messages);
        if let Some(meta) = self.sessions.lock_meta().get_mut(&run_id) {
            meta.told = told;
            meta.told_messages = messages.to_vec();
        }
    }

    /// Records the effort this run was started at, or last told via `setEffort`.
    pub(crate) fn set_effort(&self, run_id: u64, effort: impl Into<String>) {
        if let Some(meta) = self.sessions.lock_meta().get_mut(&run_id) {
            meta.effort = effort.into();
        }
    }

    /// Effort last applied to this run.
    #[must_use]
    pub(crate) fn effort(&self, run_id: u64) -> String {
        self.sessions
            .lock_meta()
            .get(&run_id)
            .map(|meta| meta.effort.clone())
            .unwrap_or_default()
    }

    /// Stores a Claude Code session id captured from the stream.
    pub(crate) fn set_session(&self, run_id: u64, session: impl Into<String>, paths: Vec<PathBuf>) {
        if let Some(meta) = self.sessions.lock_meta().get_mut(&run_id) {
            meta.session_id = Some(session.into());
            meta.session_paths = paths;
        }
    }

    /// Attaches the room slot this process holds until it is aborted.
    pub(crate) fn set_room_guard(&self, run_id: u64, guard: super::room::RoomGuard) {
        if let Some(meta) = self.sessions.lock_meta().get_mut(&run_id) {
            meta.room = Some(guard);
        }
    }

    /// Claims a tool-result handoff. Returns false when the run is closed,
    /// already resuming, or busy with a turn. A failed claim must not abort
    /// the parked process.
    pub(crate) fn claim_resume(&self, run_id: u64) -> bool {
        let mut meta = self.sessions.lock_meta();
        let Some(run) = meta.get_mut(&run_id) else {
            return false;
        };
        if run.closed || run.resuming || run.segment {
            return false;
        }
        run.resuming = true;
        true
    }

    /// Gives up a claim whose history did not follow the run.
    pub(crate) fn released(&self, run_id: u64) {
        if let Some(run) = self.sessions.lock_meta().get_mut(&run_id) {
            run.resuming = false;
        }
    }

    /// Whether `messages` go on from the conversation the run last heard.
    ///
    /// An empty `told` (tests that never called [`Bridge::heard`]) follows.
    /// Image-only rewrites (`lostMedia`) also follow.
    pub(crate) fn follows(&self, run_id: u64, messages: &[Message]) -> bool {
        let (told, told_messages) = self
            .sessions
            .lock_meta()
            .get(&run_id)
            .map(|m| (m.told.clone(), m.told_messages.clone()))
            .unwrap_or_default();
        if told.is_empty() {
            return true;
        }
        follows_told(&told, messages) || super::lost_media::lost_media(&told_messages, messages)
    }

    /// Looks up an idle run for this request's next turn (stage C).
    pub(crate) fn resume(&self, request: &Request, owner: &AccountId) -> Option<Resume> {
        self.sessions.expire(Instant::now());
        let (key, since) = next_turn(owner.as_str(), request)?;
        let run_id = {
            let mut idle = self.sessions.lock_idle();
            idle.remove(&key)
        }?;
        {
            let mut meta = self.sessions.lock_meta();
            let run = meta.get_mut(&run_id)?;
            if run.closed || run.owner != owner.as_str() {
                return None;
            }
            run.idle_key = None;
            run.idle_at = None;
            run.segment = true;
            run.resuming = true;
        }
        Some(Resume {
            run_id,
            since: since.to_vec(),
        })
    }

    /// Marks a turn finished: park on tools, abort one-offs and failures,
    /// otherwise keep the run idle under the next turn's key.
    pub(crate) fn ended(
        &self,
        run_id: u64,
        owner: &AccountId,
        request: &Request,
        said: &str,
        stop: TurnStop,
        ok: bool,
    ) {
        self.sessions.expire(Instant::now());
        match (ok, stop) {
            (false, _) => {
                self.abort_session(run_id);
                return;
            }
            (true, TurnStop::Tool) => {
                if let Some(meta) = self.sessions.lock_meta().get_mut(&run_id) {
                    meta.segment = false;
                    meta.resuming = false;
                }
                return;
            }
            (true, TurnStop::Other) => {
                self.abort_session(run_id);
                return;
            }
            (true, TurnStop::Stop) => {}
        }
        if request.tools.is_empty() && !has_reply(&request.messages) {
            self.abort_session(run_id);
            return;
        }
        let reply = assistant_message(said);
        let mut msgs = request.messages.clone();
        msgs.push(reply);
        let key = turn_key(owner.as_str(), request, &msgs);
        let keys = conversation_keys(owner.as_str(), &msgs);
        let conv = keys.last().cloned().unwrap_or_default();
        self.keep_idle(run_id, owner, key, conv);
    }

    /// How many runs are waiting for a next turn (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn idle_len(&self) -> usize {
        self.sessions.lock_idle().len()
    }

    /// Whether `run_id` is on the idle map (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_idle(&self, run_id: u64) -> bool {
        self.sessions.lock_idle().values().any(|id| *id == run_id)
    }

    /// The shelved session for `key`, if any (tests).
    #[cfg(test)]
    pub(crate) fn shelved(&self, key: &str) -> Option<SavedSession> {
        self.sessions.lock_shelf().get(key).cloned()
    }

    /// Takes a shelved session for this request's next turn, if one exists.
    pub(crate) fn unshelve(
        &self,
        request: &Request,
        owner: &AccountId,
    ) -> Option<(SavedSession, Vec<Message>)> {
        self.sessions.expire(Instant::now());
        let (key, since) = next_turn(owner.as_str(), request)?;
        let saved = self.sessions.lock_shelf().remove(&key)?;
        if saved.owner != owner.as_str() {
            self.sessions.lock_shelf().insert(key, saved);
            return None;
        }
        Some((saved, since.to_vec()))
    }

    /// Lets go of idle runs and shelved sessions at an earlier point of this
    /// conversation, which a new run now carries on. An aside (no tools, or
    /// `tool_choice=none`) retires none of them.
    pub(crate) fn retire(&self, owner: &AccountId, request: &Request) {
        if aside(request) {
            return;
        }
        let keys = conversation_keys(owner.as_str(), &request.messages);
        if keys.is_empty() {
            return;
        }
        let mut drop_ids = Vec::new();
        {
            let mut idle = self.sessions.lock_idle();
            let meta = self.sessions.lock_meta();
            let stale: Vec<(String, u64)> = idle
                .iter()
                .filter_map(|(key, id)| {
                    let run = meta.get(id)?;
                    if run.owner == owner.as_str()
                        && run
                            .conv_key
                            .as_ref()
                            .is_some_and(|conv| keys.iter().any(|k| k == conv))
                    {
                        Some((key.clone(), *id))
                    } else {
                        None
                    }
                })
                .collect();
            drop(meta);
            for (key, id) in stale {
                idle.remove(&key);
                drop_ids.push(id);
            }
        }
        self.sessions.lock_shelf().retain(|_, saved| {
            !(saved.owner == owner.as_str() && keys.iter().any(|k| k == &saved.conv))
        });
        for id in drop_ids {
            self.abort_session(id);
        }
    }

    /// Lets go of the longest-waiting idle run to make room, shelving its
    /// session when it has one. Returns whether a run was let go.
    pub(crate) fn evict_idle_for_room(&self) -> bool {
        self.sessions.expire(Instant::now());
        let oldest = {
            let idle = self.sessions.lock_idle();
            let meta = self.sessions.lock_meta();
            idle.iter()
                .filter_map(|(key, id)| {
                    let at = meta.get(id)?.idle_at?;
                    Some((key.clone(), *id, at))
                })
                .min_by_key(|(_, _, at)| *at)
                .map(|(key, id, _)| (key, id))
        };
        let Some((key, id)) = oldest else {
            return false;
        };
        self.shelve_and_drop_idle(&key, id);
        self.abort_session(id);
        true
    }

    fn keep_idle(&self, run_id: u64, owner: &AccountId, key: String, conv: String) {
        let mut drop_ids = Vec::new();
        {
            let mut idle = self.sessions.lock_idle();
            let mut meta = self.sessions.lock_meta();
            let Some(run) = meta.get_mut(&run_id) else {
                return;
            };
            if run.closed {
                return;
            }
            if run.owner.is_empty() {
                run.owner = owner.as_str().to_owned();
            }
            if let Some(old_key) = run.idle_key.clone()
                && old_key != key
                && idle.get(&old_key) == Some(&run_id)
            {
                idle.remove(&old_key);
            }
            if let Some(old) = idle.insert(key.clone(), run_id)
                && old != run_id
            {
                drop_ids.push(old);
            }
            if let Some(run) = meta.get_mut(&run_id) {
                run.idle_key = Some(key);
                run.idle_at = Some(Instant::now());
                run.segment = false;
                run.resuming = false;
                if !conv.is_empty() {
                    run.conv_key = Some(conv);
                }
            }
            while idle.len() > IDLE_MOST {
                let oldest = idle
                    .iter()
                    .filter_map(|(k, id)| {
                        let at = meta.get(id)?.idle_at?;
                        Some((k.clone(), *id, at))
                    })
                    .min_by_key(|(_, _, at)| *at);
                let Some((old_key, old_id, _)) = oldest else {
                    break;
                };
                idle.remove(&old_key);
                drop_ids.push(old_id);
                // Leave idle_key set so shelve_run can still find it.
            }
        }
        for id in drop_ids {
            self.shelve_run(id);
            self.abort_session(id);
        }
        self.room.nudge();
    }

    fn shelve_and_drop_idle(&self, key: &str, run_id: u64) {
        self.shelve_run(run_id);
        self.sessions.lock_idle().remove(key);
        if let Some(meta) = self.sessions.lock_meta().get_mut(&run_id)
            && meta.idle_key.as_deref() == Some(key)
        {
            meta.idle_key = None;
        }
    }

    fn shelve_run(&self, run_id: u64) {
        let Some(saved) = self.saved_from_run(run_id) else {
            return;
        };
        let mut shelf = self.sessions.lock_shelf();
        shelf.insert(saved.key.clone(), saved);
        while shelf.len() > SHELF_MOST {
            if let Some(old_key) = shelf
                .iter()
                .min_by_key(|(_, s)| s.at)
                .map(|(k, _)| k.clone())
            {
                shelf.remove(&old_key);
            } else {
                break;
            }
        }
    }

    fn saved_from_run(&self, run_id: u64) -> Option<SavedSession> {
        let meta = self.sessions.lock_meta();
        let run = meta.get(&run_id)?;
        let session = run.session_id.clone().filter(|s| !s.is_empty())?;
        // Prefer the current idle key; if already cleared, we cannot shelve
        // under a stable turn key, so skip.
        let key = run.idle_key.clone()?;
        Some(SavedSession {
            session,
            sessions: run.session_paths.clone(),
            owner: run.owner.clone(),
            conv: run.conv_key.clone().unwrap_or_default(),
            key,
            at: run.idle_at.unwrap_or_else(Instant::now),
        })
    }

    fn abort_session(&self, run_id: u64) {
        if let Some(mut run) = self.sessions.lock_meta().remove(&run_id) {
            run.closed = true;
            if let Some(key) = run.idle_key.take() {
                self.sessions.lock_idle().remove(&key);
            }
            run.room = None;
        }
        self.abort(run_id);
    }
}

/// How a turn ended, for [`Bridge::ended`].
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum TurnStop {
    /// A complete assistant reply with no pending tool call.
    Stop,
    /// The reply asked for tools; the run stays parked.
    Tool,
    /// Any other stop reason ends the run.
    Other,
}

/// SHA-256 turn key for `msgs` (a prefix of the request ending on a reply).
#[must_use]
pub(crate) fn turn_key(owner: &str, request: &Request, msgs: &[Message]) -> String {
    let mut hasher = Hasher::new();
    write_settings(&mut hasher, owner, request);
    hash_messages(&mut hasher, msgs, None);
    hasher.hex()
}

/// Keys of the conversation after each assistant reply, for the account
/// alone — not the model, effort, or tools.
#[must_use]
pub(crate) fn conversation_keys(owner: &str, msgs: &[Message]) -> Vec<String> {
    let mut hasher = Hasher::new();
    hasher.write_str(owner);
    hasher.write(b"\x00");
    let mut keys = Vec::new();
    hash_messages(
        &mut hasher,
        msgs,
        Some(&mut |i, h| {
            if msgs[i].role == Role::Assistant {
                keys.push(h.hex());
            }
        }),
    );
    keys
}

/// History of `msgs` as [`Bridge::follows`] compares it.
#[must_use]
pub(crate) fn history_key(msgs: &[Message]) -> String {
    let mut hasher = Hasher::new();
    hash_messages(&mut hasher, msgs, None);
    hasher.hex()
}

/// The conversation this request goes on from, and the messages since.
///
/// `None` when it is not a new turn after a reply: a first turn, or tool
/// results for a call made before the last non-tool assistant reply.
#[must_use]
pub(crate) fn next_turn<'a>(owner: &str, request: &'a Request) -> Option<(String, &'a [Message])> {
    let msgs = request.messages.as_slice();
    if msgs.is_empty() || msgs.last().is_some_and(|m| m.role == Role::Assistant) {
        return None;
    }
    let mut j = msgs.len();
    while j > 0 {
        j -= 1;
        if msgs[j].role == Role::Assistant && !calls_tool(&msgs[j]) {
            let since = &msgs[j + 1..];
            let mut calls = std::collections::HashSet::new();
            for message in since {
                for part in &message.content {
                    match part {
                        ContentPart::ToolCall(call) => {
                            calls.insert(call.id.clone());
                        }
                        ContentPart::ToolResult(result) if !calls.contains(&result.id) => {
                            return None;
                        }
                        _ => {}
                    }
                }
            }
            return Some((turn_key(owner, request, &msgs[..=j]), since));
        }
    }
    None
}

/// Whether any message is an assistant reply.
#[must_use]
pub(crate) fn has_reply(msgs: &[Message]) -> bool {
    msgs.iter().any(|m| m.role == Role::Assistant)
}

/// Whether this assistant message called a tool.
#[must_use]
pub(crate) fn calls_tool(message: &Message) -> bool {
    message.role == Role::Assistant
        && message
            .content
            .iter()
            .any(|part| matches!(part, ContentPart::ToolCall(_)))
}

fn aside(request: &Request) -> bool {
    request.tools.is_empty() || tool_choice_label(request) == "none"
}

fn follows_told(told: &str, messages: &[Message]) -> bool {
    let mut hasher = Hasher::new();
    let mut found = false;
    hash_messages(
        &mut hasher,
        messages,
        Some(&mut |_, h| {
            if h.hex() == told {
                found = true;
            }
        }),
    );
    found
}

fn assistant_message(said: &str) -> Message {
    let mut message = Message::new(Role::Assistant);
    if !said.is_empty() {
        message.content = vec![ContentPart::text(said)];
    }
    message
}

fn write_settings(hasher: &mut Hasher, owner: &str, request: &Request) {
    let flags = super::flags::from_request(request);
    let effort = flags.bit();
    let think_off = flags.think_off;
    let tool_choice = tool_choice_label(request);
    let system = without_billing_header(&system_text(request));
    let tools = serde_json::to_string(&request.tools).unwrap_or_else(|_| "[]".into());
    let web_search = request
        .tools
        .iter()
        .any(|tool| tool.name == "WebSearch" || tool.name == "web_search");
    let schema = request
        .extensions
        .get("json_schema")
        .map(|value| value.to_string())
        .unwrap_or_default();
    // owner \0 model \0 effortBit \0 thinkOff \0 toolChoice \0 system \0 tools \0 web \0 schema
    let header = format!(
        "{owner}\x00{}\x00{effort}\x00{think_off}\x00{tool_choice}\x00{system}\x00{tools}\x00{web_search}\x00{schema}",
        request.model
    );
    hasher.write_str(&header);
    if request.extensions.get("lean-prompt").is_some() {
        hasher.write_str("\x00lean-prompt");
    }
}

fn tool_choice_label(request: &Request) -> String {
    let Some(choice) = request.tool_choice.as_ref() else {
        return String::new();
    };
    let value = serde_json::to_value(choice).unwrap_or(serde_json::Value::Null);
    let mode = value
        .get("mode")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    match mode.get("mode").and_then(serde_json::Value::as_str) {
        Some("auto") => "auto".into(),
        Some("none") => "none".into(),
        Some("required") => "required".into(),
        Some("named") => mode
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned(),
        _ => String::new(),
    }
}

fn system_text(request: &Request) -> String {
    request
        .messages
        .iter()
        .filter(|message| matches!(message.role, Role::System | Role::Developer))
        .flat_map(|message| {
            message.content.iter().filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn without_billing_header(system: &str) -> String {
    billing_header().replace(system, "").into_owned()
}

fn billing_header() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^x-anthropic-billing-header:(\s*[A-Za-z_]+=[^;\s]*;)*\s*")
            .expect("billing header pattern")
    })
}

type AfterMessage<'a> = &'a mut dyn FnMut(usize, &Hasher);

fn hash_messages(hasher: &mut Hasher, msgs: &[Message], mut after: Option<AfterMessage<'_>>) {
    let words = message_words(msgs);
    let mut role = String::new();
    for (i, word) in words.iter().enumerate() {
        if !word.is_empty() {
            let this_role = role_label(&msgs[i]);
            if this_role != role {
                role.clone_from(&this_role);
                hasher.write(b"\x00");
                hasher.write_str(&role);
                hasher.write(b":");
            }
            hasher.write_str(word);
            hasher.write(b" ");
        }
        if let Some(callback) = after.as_mut() {
            callback(i, hasher);
        }
    }
}

fn role_label(message: &Message) -> String {
    match message.role {
        Role::Assistant => "assistant".into(),
        Role::System => "system".into(),
        Role::Developer => "developer".into(),
        Role::User | Role::Tool => "user".into(),
    }
}

fn message_words(msgs: &[Message]) -> Vec<String> {
    let parts = message_parts(msgs);
    parts
        .into_iter()
        .map(|parts| {
            parts
                .into_iter()
                .map(|part| part.words)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

struct WordPart {
    words: String,
}

fn message_parts(msgs: &[Message]) -> Vec<Vec<WordPart>> {
    let mut out: Vec<Vec<WordPart>> = (0..msgs.len()).map(|_| Vec::new()).collect();
    let mut n = 0_usize;
    let mut calls = HashMap::<String, usize>::new();
    for (i, message) in msgs.iter().enumerate() {
        if matches!(message.role, Role::System | Role::Developer) {
            continue;
        }
        for part in &message.content {
            let s = match part {
                ContentPart::Text(text) => text.text.clone(),
                ContentPart::ToolCall(call) => {
                    n += 1;
                    calls.insert(call.id.clone(), n);
                    format!("\x01call {} #{n}", call.name)
                }
                ContentPart::ToolResult(result) => {
                    let k = calls
                        .get(&result.id)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "?".into());
                    let text: String = result
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            ContentPart::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("\x01result #{k} {text}")
                }
                ContentPart::Image(image) => match &image.source {
                    hofund_plugin_api::ImageSource::Url { url } => {
                        format!("\x01image 0 {url}")
                    }
                    hofund_plugin_api::ImageSource::Base64 { data, .. } => {
                        format!("\x01image {} ", data.len())
                    }
                },
                ContentPart::Thinking(thinking) => thinking.text.clone(),
                ContentPart::Unknown(_) => continue,
            };
            let words = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if !words.is_empty() {
                out[i].push(WordPart { words });
            }
        }
    }
    out
}

struct Hasher {
    context: Context,
}

impl Hasher {
    fn new() -> Self {
        Self {
            context: Context::new(&SHA256),
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.context.update(bytes);
    }

    fn write_str(&mut self, text: &str) {
        self.write(text.as_bytes());
    }

    fn hex(&self) -> String {
        hex_lower(self.context.clone().finish().as_ref())
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        IDLE_MOST, SavedSession, TurnStop, conversation_keys, has_reply, history_key, next_turn,
        turn_key, without_billing_header,
    };
    use crate::bridge::Bridge;
    use hofund_plugin_api::{AccountId, ContentPart, Message, Request, Role};
    use serde_json::json;

    const OWNER: &str = "a-00000000000000aa";
    const OTHER: &str = "a-00000000000000bb";

    fn account(id: &'static str) -> AccountId {
        AccountId::from_static(id)
    }

    fn req(messages: serde_json::Value) -> Request {
        serde_json::from_value(json!({
            "model": "claude-sonnet-4-5",
            "messages": messages,
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}]
        }))
        .expect("request")
    }

    fn first_turn() -> Request {
        req(json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]))
    }

    fn second_turn() -> Request {
        req(json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            {"role": "user", "content": [{"type": "text", "text": "again"}]}
        ]))
    }

    fn tool_result_follow() -> Request {
        req(json!([
            {"role": "user", "content": [{"type": "text", "text": "list"}]},
            {"role": "assistant", "content": [{
                "type": "tool_call",
                "id": "toolu_1",
                "name": "Bash",
                "arguments": {"form": "json", "value": {"command": "ls"}}
            }]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "id": "toolu_1",
                "content": [{"type": "text", "text": "ok"}]
            }]}
        ]))
    }

    fn park(bridge: &Bridge, owner: &AccountId) -> u64 {
        let id = bridge.park_run(owner.clone(), vec!["Bash".into()]);
        bridge.ensure_session_run(id, owner, vec!["Bash".into()]);
        id
    }

    #[test]
    fn next_turn_is_none_on_a_first_turn_and_on_tool_results() {
        assert!(next_turn(OWNER, &first_turn()).is_none());
        assert!(next_turn(OWNER, &tool_result_follow()).is_none());
        let second = second_turn();
        let (key, since) = next_turn(OWNER, &second).expect("second turn");
        assert_eq!(since.len(), 1);
        assert_eq!(since[0].role, Role::User);
        let prefix = second_turn().messages[..2].to_vec();
        assert_eq!(key, turn_key(OWNER, &second_turn(), &prefix));
        assert_ne!(key, turn_key(OTHER, &second_turn(), &prefix));
    }

    #[test]
    fn turn_keys_differ_when_the_model_or_tools_differ() {
        let msgs = second_turn().messages[..2].to_vec();
        let a = turn_key(OWNER, &second_turn(), &msgs);
        let mut other = second_turn();
        other.model = "claude-opus-4-5".into();
        assert_ne!(a, turn_key(OWNER, &other, &msgs));
        let mut no_tools = second_turn();
        no_tools.tools.clear();
        assert_ne!(a, turn_key(OWNER, &no_tools, &msgs));
    }

    fn tool_call(id: &str) -> ContentPart {
        serde_json::from_value(json!({
            "type": "tool_call",
            "id": id,
            "name": "Bash",
            "arguments": {"form": "json", "value": {}}
        }))
        .expect("tool call")
    }

    #[test]
    fn hash_ignores_tool_call_ids() {
        let mut left = second_turn();
        left.messages[1].content = vec![tool_call("client_a")];
        let mut right = left.clone();
        right.messages[1].content = vec![tool_call("client_b")];
        assert_eq!(history_key(&left.messages), history_key(&right.messages));
    }

    #[test]
    fn billing_header_is_stripped_from_the_system_prompt() {
        let stripped = without_billing_header(
            "x-anthropic-billing-header: cc_version=1; cc_entrypoint=cli; cch=abc; rules",
        );
        assert_eq!(stripped, "rules");
        assert_eq!(
            without_billing_header("Rules mention x-anthropic-billing-header: here"),
            "Rules mention x-anthropic-billing-header: here"
        );
    }

    #[test]
    fn a_completed_turn_with_tools_is_kept_idle_and_resumed() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = park(&bridge, &owner);
        let request = first_turn();
        bridge.heard(run, &request.messages);
        bridge.set_session(run, "sess_1", Vec::new());
        bridge.ended(run, &owner, &request, "hello", TurnStop::Stop, true);
        assert!(bridge.is_idle(run));
        assert_eq!(bridge.idle_len(), 1);

        let found = bridge.resume(&second_turn(), &owner).expect("resume");
        assert_eq!(found.run_id, run);
        assert_eq!(found.since.len(), 1);
        assert!(!bridge.is_idle(run));
        assert!(bridge.resume(&second_turn(), &owner).is_none());
    }

    #[test]
    fn a_one_off_turn_is_aborted_not_idled() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = park(&bridge, &owner);
        let mut request = first_turn();
        request.tools.clear();
        assert!(!has_reply(&request.messages));
        bridge.ended(run, &owner, &request, "title", TurnStop::Stop, true);
        assert!(!bridge.is_idle(run));
        assert_eq!(bridge.idle_len(), 0);
    }

    #[test]
    fn a_failed_or_tool_turn_is_not_idled() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = park(&bridge, &owner);
        bridge.ended(run, &owner, &first_turn(), "", TurnStop::Stop, false);
        assert_eq!(bridge.idle_len(), 0);

        let run = park(&bridge, &owner);
        bridge.ended(run, &owner, &first_turn(), "", TurnStop::Tool, true);
        assert_eq!(bridge.idle_len(), 0);
    }

    #[test]
    fn a_seventh_idle_run_is_evicted_and_shelved() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let mut keys = Vec::new();
        let mut ids = Vec::new();
        for i in 0..=IDLE_MOST {
            let run = park(&bridge, &owner);
            let request = req(json!([{
                "role": "user",
                "content": [{"type": "text", "text": format!("turn-{i}")}]
            }]));
            bridge.set_session(run, format!("sess_{i}"), Vec::new());
            bridge.ended(run, &owner, &request, "ok", TurnStop::Stop, true);
            let mut msgs = request.messages.clone();
            msgs.push({
                let mut message = Message::new(Role::Assistant);
                message.content = vec![ContentPart::text("ok")];
                message
            });
            keys.push(turn_key(OWNER, &request, &msgs));
            ids.push(run);
            // Keep insertion order distinct even on a very fast clock.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(bridge.idle_len(), IDLE_MOST);
        assert!(!bridge.is_idle(ids[0]));
        let shelved = bridge.shelved(&keys[0]).expect("shelved oldest");
        assert_eq!(shelved.session, "sess_0");

        let follow = req(json!([
            {"role": "user", "content": [{"type": "text", "text": "turn-0"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            {"role": "user", "content": [{"type": "text", "text": "next"}]}
        ]));
        let (saved, since) = bridge.unshelve(&follow, &owner).expect("unshelve");
        assert_eq!(saved.session, "sess_0");
        assert!(saved.sessions.is_empty());
        assert_eq!(since.len(), 1);
        assert!(bridge.shelved(&keys[0]).is_none());
    }

    #[test]
    fn retire_drops_an_earlier_idle_run_of_the_same_conversation() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = park(&bridge, &owner);
        let request = first_turn();
        bridge.ended(run, &owner, &request, "hello", TurnStop::Stop, true);
        assert_eq!(bridge.idle_len(), 1);
        let follow = second_turn();
        // A follow-up that starts a fresh run retires the earlier idle.
        bridge.retire(&owner, &follow);
        assert_eq!(bridge.idle_len(), 0);
    }

    #[test]
    fn claim_resume_rejects_a_second_claim_and_released_allows_another() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = park(&bridge, &owner);
        assert!(bridge.claim_resume(run));
        assert!(!bridge.claim_resume(run));
        bridge.released(run);
        assert!(bridge.claim_resume(run));
    }

    #[test]
    fn follows_accepts_a_prefix_and_rejects_a_rewrite() {
        let bridge = Bridge::new();
        let owner = account(OWNER);
        let run = park(&bridge, &owner);
        let told = first_turn();
        bridge.heard(run, &told.messages);
        let mut continued = second_turn();
        // second_turn starts with the same user message, then assistant, then more.
        // heard only the first user message: prefix of second_turn should match
        // after the first message is hashed.
        assert!(bridge.follows(run, &continued.messages));
        continued.messages[0].content = vec![ContentPart::text("rewritten")];
        assert!(!bridge.follows(run, &continued.messages));
        let fresh = park(&bridge, &owner);
        assert!(bridge.follows(fresh, &told.messages));
    }

    #[test]
    fn conversation_keys_grow_after_each_assistant_reply() {
        let keys = conversation_keys(OWNER, &second_turn().messages);
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn saved_session_is_debug() {
        let saved = SavedSession {
            session: "s".into(),
            sessions: Vec::new(),
            owner: OWNER.into(),
            conv: "c".into(),
            key: "k".into(),
            at: std::time::Instant::now(),
        };
        assert!(format!("{saved:?}").contains("s"));
    }
}
