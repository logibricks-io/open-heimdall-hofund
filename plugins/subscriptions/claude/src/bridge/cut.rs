//! Transcript cuts and the local resume reply Claude Code inserts itself.
//!
//! A cancelled turn, or a native resume, adds one assistant text part,
//! `No response requested.`. That line is not an upstream reply, so a turn
//! key taken after it matches no waiting run. Cuts copy a session file only
//! up to a known end: the length of the saved session a run started from, or
//! the preceding assistant row whose `stop_reason` is `end_turn`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use hofund_plugin_api::{ContentPart, Message, Request, Role};
use serde_json::Value;

use super::session::{self, turn_key};

/// Assistant text Claude Code writes locally, not as an upstream reply.
pub(crate) const LOCAL_RESUME_REPLY: &str = "No response requested.";

/// Whether `message` is Claude Code's local resume notice.
#[must_use]
pub(crate) fn local_resume_reply(message: &Message) -> bool {
    if message.role != Role::Assistant || message.content.len() != 1 {
        return false;
    }
    match &message.content[0] {
        ContentPart::Text(text) => text.text == LOCAL_RESUME_REPLY,
        _ => false,
    }
}

/// Whether the client opted into its own cache marks.
///
/// The gateway overwrites `hofund_client_cache_marks`. A missing flag is not
/// a mark: ordinary `cache_control` blocks are Claude Code's own.
#[must_use]
pub(crate) fn client_cache_marks(request: &Request) -> bool {
    request
        .extensions
        .get("hofund_client_cache_marks")
        .and_then(Value::as_bool)
        == Some(true)
}

/// The 499 a same-key request returns when its own client has gone.
#[must_use]
pub(crate) fn client_closed() -> hofund_plugin_api::PluginError {
    hofund_plugin_api::PluginError::status_error(499, "the client closed the request")
}

/// Message id of a stream whose reply reached `end_turn`.
#[must_use]
pub(crate) fn completed_reply_id(stdout: &str) -> Option<String> {
    let mut id = None;
    let mut ended = false;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let event = value.get("event").unwrap_or(&value);
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(found) = event
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                {
                    id = Some(found.to_owned());
                }
            }
            Some("message_delta")
                if event.pointer("/delta/stop_reason").and_then(Value::as_str)
                    == Some("end_turn") =>
            {
                ended = true;
            }
            _ => {}
        }
    }
    ended.then_some(id).flatten()
}

/// `next_turn` with Claude Code's local replies passed over.
///
/// `None` when no local reply was passed, the turn is not new, or a tool
/// result in the suffix answers a call from before that suffix.
#[must_use]
pub(crate) fn past_local_reply<'a>(
    owner: &str,
    request: &'a Request,
) -> Option<(String, &'a [Message])> {
    let msgs = request.messages.as_slice();
    if msgs.is_empty()
        || msgs
            .last()
            .is_some_and(|message| message.role == Role::Assistant)
    {
        return None;
    }
    let mut index = msgs.len() as isize - 1;
    let mut passed = false;
    while index >= 0 {
        let message = &msgs[index as usize];
        let skip = message.role != Role::Assistant
            || session::calls_tool(message)
            || local_resume_reply(message);
        if !skip {
            break;
        }
        passed = passed || local_resume_reply(message);
        index -= 1;
    }
    if index < 0 || !passed {
        return None;
    }
    let at = index as usize;
    let since = &msgs[at + 1..];
    if !self_contained(since) {
        return None;
    }
    Some((turn_key(owner, request, &msgs[..=at]), since))
}

/// The turn a run took at `took`.
///
/// `next_turn` when `took` is that key or not the key before a local reply.
/// Otherwise the messages since the upstream reply, which include a cancelled
/// user message and the local reply.
#[must_use]
pub(crate) fn turn_from<'a>(
    owner: &str,
    request: &'a Request,
    took: &str,
) -> Option<(String, &'a [Message])> {
    let (key, since) = session::next_turn(owner, request)?;
    if took.is_empty() || key == took {
        return Some((key, since));
    }
    if let Some((prefix, prefix_since)) = past_local_reply(owner, request)
        && prefix == took
    {
        return Some((prefix, prefix_since));
    }
    Some((key, since))
}

/// No result in `msgs` answers a call made before `msgs`.
#[must_use]
pub(crate) fn self_contained(msgs: &[Message]) -> bool {
    let mut calls = std::collections::HashSet::new();
    for message in msgs {
        for part in &message.content {
            match part {
                ContentPart::ToolCall(call) => {
                    calls.insert(call.id.clone());
                }
                ContentPart::ToolResult(result) if !calls.contains(&result.id) => {
                    return false;
                }
                _ => {}
            }
        }
    }
    true
}

/// Where a session file ends after the last whole assistant row of `msg`.
///
/// `completed` keeps only a row whose `stop_reason` is `end_turn`.
#[must_use]
pub(crate) fn cut_after_reply(bytes: &[u8], msg: &str, completed: bool) -> Option<usize> {
    if msg.is_empty() {
        return None;
    }
    let mut end = None;
    let mut at = 0;
    while at < bytes.len() {
        let Some(newline) = bytes[at..].iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let line = &bytes[at..at + newline];
        let next = at + newline + 1;
        if line_is_reply(line, msg, completed) {
            end = Some(next);
        }
        at = next;
    }
    end
}

fn line_is_reply(line: &[u8], msg: &str, completed: bool) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    if value
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }
    let Some(message) = value.get("message") else {
        return false;
    };
    if message.get("id").and_then(Value::as_str) != Some(msg) {
        return false;
    }
    if completed && message.get("stop_reason").and_then(Value::as_str) != Some("end_turn") {
        return false;
    }
    true
}

/// Byte length at which `bytes` already ended, when that offset is a newline.
#[must_use]
pub(crate) fn cut_at_len(bytes: &[u8], len: u64) -> Option<usize> {
    let len = usize::try_from(len).ok()?;
    if len == 0 || bytes.len() < len {
        return None;
    }
    (bytes[len - 1] == b'\n').then_some(len)
}

/// Copies `bytes[..end]` beside `original` under a new session id.
///
/// The copy rewrites `sessionId` and is mode `0600` on Unix.
pub(crate) fn write_cut(
    dirs: &[PathBuf],
    original: &str,
    bytes_end: usize,
    bytes: &[u8],
) -> Option<String> {
    if !session_id_ok(original) || bytes_end == 0 || bytes_end > bytes.len() {
        return None;
    }
    let id = cut_id(original, &bytes[..bytes_end]);
    if !session_id_ok(&id) || id == original {
        return None;
    }
    let from = format!("\"sessionId\":\"{original}\"");
    let to = format!("\"sessionId\":\"{id}\"");
    let mut copied = bytes[..bytes_end].to_vec();
    replace_all(&mut copied, from.as_bytes(), to.as_bytes());
    for dir in dirs {
        let path = dir.join(format!("{id}.jsonl"));
        if write_private(&path, &copied).is_ok() {
            return Some(id);
        }
    }
    None
}

/// Project directories under `config_dir` that already hold `{session}.jsonl`.
#[must_use]
pub(crate) fn session_dirs(config_dir: &Path, session: &str) -> Vec<PathBuf> {
    if !session_id_ok(session) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(config_dir.join("projects")) else {
        return Vec::new();
    };
    let name = format!("{session}.jsonl");
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join(&name).is_file() {
            found.push(path);
        }
    }
    found
}

/// Whether `id` is a single path segment a session file may use.
#[must_use]
pub(crate) fn session_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
}

/// Reads the first session file named `{session}.jsonl` under `dirs`.
#[must_use]
pub(crate) fn read_session(dirs: &[PathBuf], session: &str) -> Option<Vec<u8>> {
    if !session_id_ok(session) {
        return None;
    }
    let name = format!("{session}.jsonl");
    for dir in dirs {
        if let Ok(bytes) = std::fs::read(dir.join(name.as_str())) {
            return Some(bytes);
        }
    }
    None
}

/// Length of the first session file, or 0 when none is there.
#[must_use]
pub(crate) fn session_len(dirs: &[PathBuf], session: &str) -> u64 {
    if !session_id_ok(session) {
        return 0;
    }
    let name = format!("{session}.jsonl");
    for dir in dirs {
        if let Ok(meta) = std::fs::metadata(dir.join(name.as_str())) {
            return meta.len();
        }
    }
    0
}

fn cut_id(original: &str, bytes: &[u8]) -> String {
    use ring::digest::{Context, SHA256};
    let mut context = Context::new(&SHA256);
    context.update(original.as_bytes());
    context.update(b"\0");
    context.update(bytes);
    let digest = context.finish();
    let hex = hex_lower(digest.as_ref());
    format!("c{}", &hex[..32])
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

fn replace_all(bytes: &mut Vec<u8>, from: &[u8], to: &[u8]) {
    if from.is_empty() || from == to {
        return;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(from) {
            out.extend_from_slice(to);
            index += from.len();
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    *bytes = out;
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

/// Removes a copy this bridge wrote. A live session file is left in place.
pub(crate) fn delete_copy(dirs: &[PathBuf], session: &str) {
    if !session_id_ok(session) {
        return;
    }
    for dir in dirs {
        let _ = std::fs::remove_file(dir.join(format!("{session}.jsonl")));
    }
}
