//! What a Codex request's input becomes on its way.
//!
//! Codex keeps a conversation's items and sends them back each turn. Items
//! written by one model are not always readable by the next, so the input
//! is put right before it goes: ids the ChatGPT backend refuses are made
//! ones it takes, and a summary hofund wrote when it compacted a hofund
//! model's conversation is opened again as a message.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hofund_plugin_sdk::json::{self, Value, json};
use hofund_plugin_sdk::sigv4::payload_hash;

/// What starts the `encrypted_content` of a compaction hofund wrote.
pub(crate) const MARK: &str = "hofund1:";

/// What a compacted conversation's summary is opened with. From Codex
/// (openai/codex, Apache-2.0), as Codex itself opens its summaries.
pub(crate) const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// What a hofund model is asked to compact with. From Codex
/// (openai/codex, Apache-2.0), as Codex asks its own models.
pub(crate) const COMPACT_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.\n\nInclude:\n- Current progress and key decisions made\n- Important context, constraints, or user preferences\n- What remains to be done (clear next steps)\n- Any critical data, examples, or references needed to continue\n\nBe concise, structured, and focused on helping the next LLM seamlessly continue the work.";

/// The longest `call_id` the backend takes.
const CALL_ID_MOST: usize = 64;

/// The call items whose ids the backend wants with a prefix of their own.
const CALL_PREFIXES: [(&str, &str); 2] =
    [("tool_search_call", "tsc_"), ("custom_tool_call", "ctc_")];

/// The `encrypted_content` a compaction carries `summary` in.
pub(crate) fn sealed(summary: &str) -> String {
    format!("{MARK}{}", STANDARD.encode(summary))
}

/// A body for one of Codex's own models, as the backend takes it, or
/// `None` when it goes as it came.
pub(crate) fn for_backend(body: &[u8]) -> Option<Vec<u8>> {
    let mut value = json::from_slice::<Value>(body).ok()?;
    let items = value.get_mut("input").and_then(Value::as_array_mut)?;
    let mut changed = call_item_ids(items);
    changed |= bound_call_ids(items);
    changed |= plain_item_ids(items);
    changed |= open_summaries(items, false).0;
    changed.then(|| value.to_string().into_bytes())
}

/// A body for one of hofund's models with its summaries opened, or `None`
/// when it goes as it came, and whether it asks to compact. A compaction
/// is asked without tools and for the whole reply, not a stream.
pub(crate) fn for_hofund(body: &[u8]) -> (Option<Vec<u8>>, bool) {
    let Ok(mut value) = json::from_slice::<Value>(body) else {
        return (None, false);
    };
    let Some(items) = value.get_mut("input").and_then(Value::as_array_mut) else {
        return (None, false);
    };
    let (changed, compact) = open_summaries(items, true);
    if compact {
        items.retain(|item| kind(item) != "additional_tools");
        if let Some(object) = value.as_object_mut() {
            object.insert("tools".to_owned(), json!([]));
            object.insert("tool_choice".to_owned(), json!("auto"));
            object.insert("parallel_tool_calls".to_owned(), json!(false));
            object.insert("stream".to_owned(), json!(false));
        }
    }
    (changed.then(|| value.to_string().into_bytes()), compact)
}

fn kind(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn text<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Gives tool-search and custom tool calls the id prefix the backend
/// wants: what follows the first `_`, or the whole id when nothing does,
/// after it.
fn call_item_ids(items: &mut [Value]) -> bool {
    let mut changed = false;
    for item in items.iter_mut() {
        let Some(prefix) = CALL_PREFIXES
            .iter()
            .find(|(name, _)| *name == kind(item))
            .map(|(_, prefix)| *prefix)
        else {
            continue;
        };
        let id = text(item, "id");
        if id.is_empty() || id.starts_with(prefix) {
            continue;
        }
        let tail = id
            .split_once('_')
            .map(|(_, tail)| tail)
            .filter(|tail| !tail.is_empty())
            .unwrap_or(id);
        let fixed = format!("{prefix}{tail}");
        item["id"] = Value::String(fixed);
        changed = true;
    }
    changed
}

/// Puts a `call_id` past the backend's length as the hex SHA-256 of it,
/// the same for the call and its output.
fn bound_call_ids(items: &mut [Value]) -> bool {
    let mut changed = false;
    for item in items.iter_mut() {
        let id = text(item, "call_id");
        if id.len() <= CALL_ID_MOST {
            continue;
        }
        let bounded = payload_hash(id.trim().as_bytes());
        item["call_id"] = Value::String(bounded);
        changed = true;
    }
    changed
}

/// Whether `c` may be in an item id the backend takes.
const fn plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Makes item ids plain: each other character becomes `_`. A reasoning
/// item with such an id was written elsewhere and is dropped.
fn plain_item_ids(items: &mut Vec<Value>) -> bool {
    let before = items.len();
    items.retain(|item| kind(item) != "reasoning" || text(item, "id").chars().all(plain));
    let mut changed = items.len() != before;
    for item in items.iter_mut() {
        let id = text(item, "id");
        if id.chars().all(plain) {
            continue;
        }
        let fixed: String = id.chars().map(|c| if plain(c) { c } else { '_' }).collect();
        item["id"] = Value::String(fixed);
        changed = true;
    }
    changed
}

/// The summary in `sealed`, or `None` when hofund did not write it, and
/// `Some(None)` when it did and it cannot be read.
fn summary(sealed: &str) -> Option<Option<String>> {
    let data = sealed.strip_prefix(MARK)?;
    Some(
        STANDARD
            .decode(data)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok()),
    )
}

/// A user message of `text`.
fn user_message(text: &str) -> Value {
    json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
}

/// Whether a reasoning item for one of Codex's own models was written
/// elsewhere: it shows its content, or has nothing sealed.
fn foreign_reasoning(item: &Value) -> bool {
    let shown = match item.get("content") {
        None | Some(Value::Null) => false,
        Some(Value::Array(parts)) => !parts.is_empty(),
        Some(_) => true,
    };
    shown || text(item, "encrypted_content").is_empty()
}

/// Opens hofund's summaries as messages, and drops one that cannot be
/// read. For a hofund model a `compaction_trigger` becomes the compaction
/// prompt; for Codex's own models a reasoning item written elsewhere is
/// dropped. Says whether anything changed and whether to compact.
fn open_summaries(items: &mut Vec<Value>, hofund: bool) -> (bool, bool) {
    let mut changed = false;
    let mut compact = false;
    let taken = std::mem::take(items);
    for item in taken {
        match kind(&item) {
            "compaction" | "compaction_summary" => {
                match summary(text(&item, "encrypted_content")) {
                    None => items.push(item),
                    Some(opened) => {
                        changed = true;
                        if let Some(summary) = opened {
                            items.push(user_message(&format!("{SUMMARY_PREFIX}\n{summary}")));
                        }
                    }
                }
            }
            "compaction_trigger" if hofund => {
                items.push(user_message(COMPACT_PROMPT));
                changed = true;
                compact = true;
            }
            "reasoning" if !hofund && foreign_reasoning(&item) => changed = true,
            _ => items.push(item),
        }
    }
    (changed, compact)
}
