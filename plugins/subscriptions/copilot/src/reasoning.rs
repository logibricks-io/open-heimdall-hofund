//! Sealed Chat reasoning for one Copilot account.
//!
//! A Chat reply can carry `reasoning_opaque` beside its tool calls. No other
//! protocol has a field for it, and a client that does not know the field
//! drops it, so the next turn would be sent without the model's earlier
//! reasoning. This module reads the value from a Chat 200 as the body passes
//! (a stream, or a reply sent whole) and keeps it per account, model, and
//! tool call id. The next Chat call to the same account and model, after
//! translation, puts it back on the assistant message that carries that call.
//! A message that already has `reasoning_opaque` keeps its own. A reply with
//! no tool call id is not kept.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::wire::{Observe, Rewrite};

use crate::models::Book;

/// Directory under the plugin state directory.
const DIR_NAME: &str = "copilot-reasoning";
/// Bytes of the hash used as the file name.
const KEY_BYTES: usize = 16;
/// A conversation not taken up again in this long is sent without its old reasoning.
const KEEP_FOR: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// How often a write scans for files past [`KEEP_FOR`].
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);
/// Temp files a failed write left behind are removed after this.
const TEMP_FOR: Duration = Duration::from_secs(60 * 60);
/// Largest opaque kept. A live reply was about a thousand characters.
const LARGEST: usize = 1 << 20;
/// How much of a reply sent whole is read for the field.
const WHOLE_MOST: usize = 8 << 20;

struct Place<'a> {
    dir: &'a Path,
    pruned: &'a Mutex<Option<SystemTime>>,
    account: &'a str,
    model: &'a str,
    now: SystemTime,
}

/// The rewrite and the body look for one Chat call.
///
/// `None` when this plugin has no state directory. The call is then sent
/// unchanged and nothing is kept.
pub(crate) fn hooks(
    book: &Arc<Book>,
    host: &HostContext,
    account: &AccountId,
    model: &str,
) -> Option<(Rewrite, Observe)> {
    let dir = host.dirs().state_dir().ok()?.join(DIR_NAME);
    let account = account.as_str().to_owned();
    let heard_model = Arc::new(Mutex::new(model.to_owned()));
    let clock = host.clock().clone();

    let restore_dir = dir.clone();
    let restore_account = account.clone();
    let restore_model = Arc::clone(&heard_model);
    let restore_clock = clock.clone();
    let rewrite: Rewrite = Arc::new(move |object| {
        if let Some(written) = object.get("model").and_then(Value::as_str) {
            *restore_model
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = written.to_owned();
        }
        restore(&restore_dir, &restore_account, object, restore_clock.now());
        Vec::new()
    });

    let tap = Arc::new(Mutex::new(Tap::default()));
    let book = Arc::clone(book);
    let observe: Observe = Arc::new(move |chunk| {
        let model = heard_model
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut tap = tap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let place = Place {
            dir: &dir,
            pruned: &book.opaque_pruned_at,
            account: &account,
            model: &model,
            now: clock.now(),
        };
        tap.push(chunk, &place);
    });
    Some((rewrite, observe))
}

/// The opaque stored for `id` of `account`'s `model`, when one is kept.
#[cfg(test)]
pub(crate) fn read(
    host: &HostContext,
    account: &AccountId,
    model: &str,
    id: &str,
) -> Option<String> {
    let dir = host.dirs().state_dir().ok()?.join(DIR_NAME);
    kept(&dir, account.as_str(), model, id, host.clock().now())
}

fn restore(dir: &Path, account: &str, object: &mut Map<String, Value>, now: SystemTime) {
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages {
        let Some(message) = message.as_object_mut() else {
            continue;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if message.contains_key("reasoning_opaque") {
            continue;
        }
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        let found = calls.iter().find_map(|call| {
            let id = call.get("id").and_then(Value::as_str)?;
            kept(dir, account, &model, id, now)
        });
        if let Some(opaque) = found {
            message.insert("reasoning_opaque".to_owned(), Value::String(opaque));
        }
    }
}

fn kept(dir: &Path, account: &str, model: &str, id: &str, now: SystemTime) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    let path = dir.join(file_name(account, model, id));
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    touch(&path, now);
    String::from_utf8(bytes).ok()
}

fn keep(place: &Place<'_>, id: &str, opaque: &str) {
    if id.is_empty() || opaque.is_empty() || opaque.len() > LARGEST {
        return;
    }
    let name = file_name(place.account, place.model, id);
    let path = place.dir.join(&name);
    if let Ok(existing) = std::fs::read(&path)
        && existing == opaque.as_bytes()
    {
        return;
    }
    if !write_whole(place.dir, &name, opaque.as_bytes(), place.now) {
        return;
    }
    prune(place.dir, place.pruned, place.now);
}

fn write_whole(dir: &Path, name: &str, bytes: &[u8], now: SystemTime) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    tighten_dir(dir);
    let Ok(id) = hofund_plugin_sdk::uuid::v4() else {
        return false;
    };
    let tmp = dir.join(format!(".tmp-{id}"));
    if std::fs::write(&tmp, bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    tighten_file(&tmp);
    let dest = dir.join(name);
    if std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    touch(&dest, now);
    true
}

fn touch(path: &Path, now: SystemTime) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(now);
    }
}

fn prune(dir: &Path, pruned: &Mutex<Option<SystemTime>>, now: SystemTime) {
    {
        let mut at = pruned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(prev) = *at
            && now.duration_since(prev).unwrap_or(Duration::MAX) < PRUNE_EVERY
        {
            return;
        }
        *at = Some(now);
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            continue;
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        let temp = entry.file_name().as_encoded_bytes().first() == Some(&b'.');
        if age > KEEP_FOR || (temp && age > TEMP_FOR) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn file_name(account: &str, model: &str, id: &str) -> String {
    let mut bytes = Vec::with_capacity(account.len() + model.len() + id.len() + 2);
    bytes.extend_from_slice(account.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(model.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(id.as_bytes());
    let digest = sha256(&bytes);
    encode_url_key(&digest[..KEY_BYTES])
}

/// URL-safe base64 without padding. The name is the first 16 hash bytes.
fn encode_url_key(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let (chunks, rest) = bytes.as_chunks::<3>();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        push_six(&mut out, TABLE, n >> 18);
        push_six(&mut out, TABLE, n >> 12);
        push_six(&mut out, TABLE, n >> 6);
        push_six(&mut out, TABLE, n);
    }
    if let Some(&first) = rest.first() {
        let mut n = u32::from(first) << 16;
        if let Some(&second) = rest.get(1) {
            n |= u32::from(second) << 8;
        }
        push_six(&mut out, TABLE, n >> 18);
        push_six(&mut out, TABLE, n >> 12);
        if rest.len() == 2 {
            push_six(&mut out, TABLE, n >> 6);
        }
    }
    out
}

fn push_six(out: &mut String, table: &[u8; 64], bits: u32) {
    let index = usize::from(u8::try_from(bits & 63).expect("six bits"));
    out.push(char::from(table[index]));
}

#[cfg(unix)]
fn tighten_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(unix)]
fn tighten_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn tighten_dir(_path: &Path) {}

#[cfg(not(unix))]
fn tighten_file(_path: &Path) {}

#[derive(Default)]
struct Tap {
    buf: Vec<u8>,
    whole: bool,
    sure: bool,
    done: bool,
    choices: HashMap<i64, Choice>,
}

#[derive(Default)]
struct Choice {
    opaque: String,
    ids: Vec<String>,
    kept: HashMap<String, String>,
}

impl Tap {
    fn push(&mut self, chunk: Option<&[u8]>, place: &Place<'_>) {
        match chunk {
            Some(bytes) if !bytes.is_empty() && !self.done => {
                self.buf.extend_from_slice(bytes);
                self.look(place);
            }
            None if !self.done && self.whole => {
                self.done = true;
                let buf = std::mem::take(&mut self.buf);
                self.read_whole(&buf, place);
            }
            _ => {}
        }
    }

    fn look(&mut self, place: &Place<'_>) {
        if !self.sure {
            let trimmed = trim_start_ascii(&self.buf);
            if trimmed.is_empty() {
                return;
            }
            self.sure = true;
            self.whole = trimmed[0] == b'{';
        }
        if self.whole {
            if self.buf.len() > WHOLE_MOST {
                self.done = true;
                self.buf.clear();
            }
            return;
        }
        while let Some(end) = self.buf.iter().position(|byte| *byte == b'\n') {
            let line = trim_ascii(&self.buf[..end]).to_vec();
            self.buf.drain(..=end);
            let Some(data) = line.strip_prefix(b"data:") else {
                continue;
            };
            if contains(data, b"\"reasoning_opaque\"") || contains(data, b"\"tool_calls\"") {
                self.read_chunk(trim_ascii(data), place);
            }
        }
        if self.buf.len() > LARGEST + WHOLE_MOST {
            self.done = true;
            self.buf.clear();
        }
    }

    fn read_chunk(&mut self, data: &[u8], place: &Place<'_>) {
        let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(data) else {
            return;
        };
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            let index = choice.get("index").and_then(Value::as_i64).unwrap_or(0);
            let slot = self.choice(index);
            if let Some(delta) = choice.get("delta") {
                if let Some(opaque) = delta
                    .get("reasoning_opaque")
                    .and_then(Value::as_str)
                    .filter(|opaque| !opaque.is_empty())
                {
                    slot.opaque = opaque.to_owned();
                }
                push_ids(&mut slot.ids, delta.get("tool_calls"));
            }
            keep_choice(slot, place);
        }
    }

    fn read_whole(&mut self, bytes: &[u8], place: &Place<'_>) {
        let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(bytes) else {
            return;
        };
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            let index = choice.get("index").and_then(Value::as_i64).unwrap_or(0);
            let slot = self.choice(index);
            if let Some(message) = choice.get("message") {
                slot.opaque = message
                    .get("reasoning_opaque")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                push_ids(&mut slot.ids, message.get("tool_calls"));
            }
            keep_choice(slot, place);
        }
    }

    fn choice(&mut self, index: i64) -> &mut Choice {
        self.choices.entry(index).or_default()
    }
}

fn keep_choice(choice: &mut Choice, place: &Place<'_>) {
    if choice.opaque.is_empty() {
        return;
    }
    let mut index = 0;
    while index < choice.ids.len() {
        let id = choice.ids[index].clone();
        index += 1;
        if choice
            .kept
            .get(&id)
            .is_some_and(|kept| kept == &choice.opaque)
        {
            continue;
        }
        keep(place, &id, &choice.opaque);
        let opaque = choice.opaque.clone();
        choice.kept.insert(id, opaque);
    }
}

fn push_ids(ids: &mut Vec<String>, calls: Option<&Value>) {
    let Some(calls) = calls.and_then(Value::as_array) else {
        return;
    };
    for call in calls {
        if let Some(id) = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            ids.push(id.to_owned());
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|window| window == needle)
}

fn trim_start_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        .map_or(start, |last| last + 1);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d262_key_name_is_the_url_safe_hash_prefix() {
        assert_eq!(
            file_name("d262-echo", "gemini-4-hydrogen", "call_paris"),
            "NCYhjp1bXJ_PwotJlip6aw"
        );
        assert_eq!(encode_url_key(&[0x00; 16]), "AAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(encode_url_key(&[0xff; 16]), "_____________________w");
    }

    fn place<'a>(
        dir: &'a Path,
        pruned: &'a Mutex<Option<SystemTime>>,
        now: SystemTime,
    ) -> Place<'a> {
        Place {
            dir,
            pruned,
            account: "d262-echo",
            model: "gemini-4-hydrogen",
            now,
        }
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn d262_store_writes_whole_and_rejects_an_oversized_value() {
        let dir = tempfile::tempdir().expect("dir");
        let pruned = Mutex::new(None);
        let now = at(10);
        let here = place(dir.path(), &pruned, now);
        keep(&here, "call-1", "seal");
        assert_eq!(
            kept(dir.path(), "d262-echo", "gemini-4-hydrogen", "call-1", now).as_deref(),
            Some("seal")
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|name| !name.to_string_lossy().starts_with('.')),
            "a temp file was left: {names:?}"
        );
        assert!(
            names
                .iter()
                .all(|name| !name.to_string_lossy().contains("call")),
            "the call id is in the file name: {names:?}"
        );
        keep(&here, "call-2", &"x".repeat(LARGEST + 1));
        assert!(kept(dir.path(), "d262-echo", "gemini-4-hydrogen", "call-2", now).is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file = dir
                .path()
                .join(file_name("d262-echo", "gemini-4-hydrogen", "call-1"));
            let mode = std::fs::metadata(&file).expect("file").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(dir.path())
                .expect("dir")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
    }

    #[test]
    fn d262_store_prunes_unread_files_and_old_temps() {
        let dir = tempfile::tempdir().expect("dir");
        let pruned = Mutex::new(None);
        let start = at(40 * 24 * 60 * 60);
        let later = start + PRUNE_EVERY + Duration::from_secs(1);
        keep(&place(dir.path(), &pruned, start), "fresh", "seal");

        let stale = dir.path().join("stale");
        std::fs::write(&stale, b"old").expect("stale");
        touch(&stale, later - KEEP_FOR - Duration::from_secs(1));
        let exact = dir.path().join("exact");
        std::fs::write(&exact, b"edge").expect("exact");
        touch(&exact, later - KEEP_FOR);
        let temp = dir.path().join(".tmp-left");
        std::fs::write(&temp, b"half").expect("temp");
        touch(&temp, later - TEMP_FOR - Duration::from_secs(1));
        let young = dir.path().join(".tmp-young");
        std::fs::write(&young, b"new").expect("young");
        touch(&young, later - Duration::from_secs(60));
        std::fs::create_dir(dir.path().join("nested")).expect("nested");

        let soon = start + Duration::from_secs(30 * 60);
        keep(&place(dir.path(), &pruned, soon), "other", "next");
        assert!(stale.exists(), "pruned again inside the hour");
        assert!(temp.exists(), "pruned again inside the hour");

        keep(&place(dir.path(), &pruned, later), "third", "more");
        assert!(!stale.exists(), "unread file stayed");
        assert!(exact.exists(), "a file at exactly 14 days was removed");
        assert!(!temp.exists(), "an old temp stayed");
        assert!(young.exists(), "a young temp was removed");
        assert!(dir.path().join("nested").is_dir());
        assert!(
            kept(dir.path(), "d262-echo", "gemini-4-hydrogen", "fresh", later).is_some(),
            "a file written at the start was removed"
        );

        let reread = later + Duration::from_secs(60);
        assert!(
            kept(
                dir.path(),
                "d262-echo",
                "gemini-4-hydrogen",
                "fresh",
                reread
            )
            .is_some()
        );
        keep(
            &place(dir.path(), &pruned, reread + KEEP_FOR),
            "fourth",
            "again",
        );
        assert!(
            kept(
                dir.path(),
                "d262-echo",
                "gemini-4-hydrogen",
                "fresh",
                reread + KEEP_FOR
            )
            .is_some(),
            "a read did not keep the file"
        );
    }

    #[test]
    fn d262_same_bytes_do_not_refresh_the_unread_clock() {
        let dir = tempfile::tempdir().expect("dir");
        let pruned = Mutex::new(None);
        let start = at(50_000);
        keep(&place(dir.path(), &pruned, start), "call-1", "seal");
        keep(
            &place(dir.path(), &pruned, start + KEEP_FOR),
            "call-1",
            "seal",
        );
        keep(
            &place(
                dir.path(),
                &pruned,
                start + KEEP_FOR + PRUNE_EVERY + Duration::from_secs(2),
            ),
            "call-2",
            "other",
        );
        assert!(
            kept(
                dir.path(),
                "d262-echo",
                "gemini-4-hydrogen",
                "call-1",
                start
            )
            .is_none()
        );
    }

    #[test]
    fn d262_tap_keeps_a_split_stream_and_a_whole_reply() {
        let dir = tempfile::tempdir().expect("dir");
        let pruned = Mutex::new(None);
        let now = at(9);
        let here = place(dir.path(), &pruned, now);
        let mut tap = Tap::default();
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_opaque\":\"seal\",\"tool_calls\":[{\"id\":\"call_paris\"}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"id\":\"call_tokyo\"}]}}]}\n\n",
        );
        let (left, right) = stream.split_at(40);
        tap.push(Some(left.as_bytes()), &here);
        tap.push(Some(right.as_bytes()), &here);
        tap.push(None, &here);
        assert_eq!(
            kept(
                dir.path(),
                "d262-echo",
                "gemini-4-hydrogen",
                "call_paris",
                now
            )
            .as_deref(),
            Some("seal")
        );
        assert_eq!(
            kept(
                dir.path(),
                "d262-echo",
                "gemini-4-hydrogen",
                "call_tokyo",
                now
            )
            .as_deref(),
            Some("seal")
        );

        let mut whole = Tap::default();
        let body = br#"{"choices":[{"index":0,"message":{"reasoning_opaque":"whole","tool_calls":[{"id":"call_whole"}]}}]}"#;
        whole.push(Some(body), &here);
        assert!(
            kept(
                dir.path(),
                "d262-echo",
                "gemini-4-hydrogen",
                "call_whole",
                now
            )
            .is_none()
        );
        whole.push(None, &here);
        assert_eq!(
            kept(
                dir.path(),
                "d262-echo",
                "gemini-4-hydrogen",
                "call_whole",
                now
            )
            .as_deref(),
            Some("whole")
        );

        let mut bare = Tap::default();
        bare.push(
            Some(br#"{"choices":[{"message":{"reasoning_opaque":"no-call"}}]}"#),
            &here,
        );
        bare.push(None, &here);
        assert!(kept(dir.path(), "d262-echo", "gemini-4-hydrogen", "", now).is_none());
        let leftover = std::fs::read_dir(dir.path())
            .expect("list")
            .filter_map(Result::ok)
            .any(|entry| {
                std::fs::read(entry.path()).ok().as_deref() == Some(b"no-call".as_slice())
            });
        assert!(!leftover, "an opaque with no tool call was kept");
    }

    #[test]
    fn d262_tap_drops_a_whole_reply_past_the_bound() {
        let dir = tempfile::tempdir().expect("dir");
        let pruned = Mutex::new(None);
        let now = at(3);
        let here = place(dir.path(), &pruned, now);
        let mut tap = Tap::default();
        let mut body = vec![b'{'];
        body.resize(WHOLE_MOST + 1, b' ');
        tap.push(Some(&body), &here);
        tap.push(None, &here);
        let count = std::fs::read_dir(dir.path()).expect("list").count();
        assert_eq!(count, 0);
    }
}
