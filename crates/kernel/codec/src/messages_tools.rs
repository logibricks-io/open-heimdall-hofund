//! Mend a Messages request whose tool exchange would be refused.
//!
//! A call the following run of user messages does not answer gets a synthetic
//! error result.
//! A result that names no call of the message before becomes the text it
//! said. A second answer to one call is dropped, and a result that follows
//! other content of its message moves first. Words an assistant message
//! wrote after its first call move before that call; thinking stays, and
//! empty text is dropped. A system or developer message between a call and
//! its results is placed after the results, and the real result stays a
//! result. Two calls that share an id stay two ids, each answered.
//!
//! A body that is already in shape is returned unchanged: nothing is parsed
//! into maps and nothing is written again. Create, token counting, and a
//! translated build all encode through this function.

use std::collections::HashSet;

use serde_json::Value;

/// Text of the synthetic result for a call the next message does not answer.
const INTERRUPTED: &str =
    "[The result of this tool call is unavailable: the turn was interrupted.]";

const STAND_IN: &str = "Tool result received.";

const MAX_DEPTH: usize = 128;

struct Call {
    final_id: String,
    original: String,
}

struct Member<'a> {
    key: String,
    value: &'a [u8],
    value_start: usize,
    value_end: usize,
}

/// Returns `body` with its Messages tool exchange mended.
///
/// The returned bytes equal `body` when the exchange is already in shape,
/// or when the body is not a Messages object this pass can read.
#[must_use]
pub fn pair_messages_tool_blocks(body: &[u8]) -> Vec<u8> {
    if !contains(body, br#""tool_use""#) && !contains(body, br#""tool_result""#) {
        return body.to_vec();
    }
    let Some(messages) = top_member(body, "messages") else {
        return body.to_vec();
    };
    if messages.first() != Some(&b'[') {
        return body.to_vec();
    }
    let Some(mended) = paired_messages(messages) else {
        return body.to_vec();
    };
    with_top_value(body, "messages", &mended)
}

fn paired_messages(msgs: &[u8]) -> Option<Vec<u8>> {
    let elements = elements(msgs)?;
    let mut out = Vec::new();
    let mut changed = false;
    let mut pending: Vec<Call> = Vec::new();
    let mut answered = HashSet::new();
    let mut held: Vec<Vec<u8>> = Vec::new();
    let mut index = 0;
    while index < elements.len() {
        let element = elements[index];
        if element.first() != Some(&b'{') {
            return None;
        }
        let (role, content) = message_view(element)?;
        if (role == "system" || role == "developer") && !pending.is_empty() {
            held.push(element.to_vec());
            changed = true;
            index += 1;
            continue;
        }
        if role == "assistant" {
            if flush_unanswered(&mut out, &pending, &answered) {
                changed = true;
            }
            out.append(&mut held);
            pending.clear();
            answered.clear();
            match mend_assistant(element, content, &mut pending) {
                AssistantMend::Invalid => return None,
                AssistantMend::Same => out.push(element.to_vec()),
                AssistantMend::Changed(message) => {
                    changed = true;
                    out.push(message);
                }
            }
            index += 1;
            continue;
        }
        if role == "user" && !pending.is_empty() {
            let mut run = vec![element];
            index += 1;
            while index < elements.len() {
                let next = elements[index];
                if next.first() != Some(&b'{') {
                    return None;
                }
                let (next_role, _) = message_view(next)?;
                if next_role != "user" {
                    break;
                }
                run.push(next);
                index += 1;
            }
            let mut drafts = Vec::with_capacity(run.len());
            for user in run {
                let (_, user_content) = message_view(user)?;
                drafts.push(draft_user(user, user_content, &pending, &mut answered)?);
            }
            // Every message in the run has been matched. Synthetics are only
            // the calls none of them answered, and they go in the first user
            // message so the message after the assistant still pairs.
            let missing = missing_results(&pending, &answered);
            let mut emitted = Vec::with_capacity(drafts.len());
            for (offset, draft) in drafts.iter().enumerate() {
                let extra: &[Vec<u8>] = if offset == 0 { &missing } else { &[] };
                match emit_user(draft, extra) {
                    UserMend::Invalid => return None,
                    UserMend::Same => emitted.push(draft.element.to_vec()),
                    UserMend::Changed(message) => {
                        changed = true;
                        emitted.push(message);
                    }
                }
            }
            release_held(&mut out, emitted, &mut held);
            pending.clear();
            answered.clear();
            continue;
        }
        match mend_user(element, content, &pending, &mut answered, true) {
            UserMend::Invalid => return None,
            UserMend::Same => out.push(element.to_vec()),
            UserMend::Changed(message) => {
                changed = true;
                out.push(message);
            }
        }
        out.append(&mut held);
        pending.clear();
        answered.clear();
        index += 1;
    }
    if flush_unanswered(&mut out, &pending, &answered) {
        changed = true;
    }
    out.append(&mut held);
    changed.then(|| raw_list(&out))
}

enum AssistantMend {
    Invalid,
    Same,
    Changed(Vec<u8>),
}

enum UserMend {
    Invalid,
    Same,
    Changed(Vec<u8>),
}

fn flush_unanswered(out: &mut Vec<Vec<u8>>, pending: &[Call], answered: &HashSet<String>) -> bool {
    let missing = missing_results(pending, answered);
    if missing.is_empty() {
        return false;
    }
    out.push(results_message(&missing));
    true
}

fn mend_assistant(
    element: &[u8],
    content: Option<&[u8]>,
    pending: &mut Vec<Call>,
) -> AssistantMend {
    let Some(content) = content else {
        return AssistantMend::Same;
    };
    if content.first() != Some(&b'[') {
        return AssistantMend::Same;
    }
    let Some(blocks) = elements(content) else {
        return AssistantMend::Invalid;
    };
    let mut owned = Vec::with_capacity(blocks.len());
    let mut changed = false;
    let mut taken = HashSet::new();
    for block in &blocks {
        let (kind, id, _) = block_ids(block);
        if kind == "tool_use" && !id.is_empty() {
            taken.insert(id);
        }
    }
    let mut seen = HashSet::new();
    for block in blocks {
        let (kind, id, _) = block_ids(block);
        if kind == "tool_use" {
            let (final_id, rewritten) = unique_call(block, &id, &mut taken, &mut seen);
            if rewritten.is_some() {
                changed = true;
            }
            pending.push(Call {
                final_id,
                original: id,
            });
            owned.push(rewritten.unwrap_or_else(|| block.to_vec()));
        } else {
            owned.push(block.to_vec());
        }
    }
    if let Some(reordered) = calls_last(&owned) {
        return AssistantMend::Changed(with_top_value(element, "content", &raw_list(&reordered)));
    }
    if changed {
        return AssistantMend::Changed(with_top_value(element, "content", &raw_list(&owned)));
    }
    AssistantMend::Same
}

fn unique_call(
    block: &[u8],
    id: &str,
    taken: &mut HashSet<String>,
    seen: &mut HashSet<String>,
) -> (String, Option<Vec<u8>>) {
    if id.is_empty() || seen.insert(id.to_owned()) {
        return (id.to_owned(), None);
    }
    let final_id = distinct_id(id, taken);
    let rewritten = with_string_member(block, "id", &final_id);
    (final_id, Some(rewritten))
}

/// Mints `{original}__2`, then `__3`, and so on.
///
/// Every id in the message is already in `taken`, so a real `{id}__2` is not
/// given to an earlier duplicate. `checked_add` is what makes the `__dup`
/// fallback reachable: `saturating_add` never wraps.
fn distinct_id(original: &str, taken: &mut HashSet<String>) -> String {
    let mut n = 2u32;
    loop {
        let candidate = format!("{original}__{n}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        match n.checked_add(1) {
            Some(next) => n = next,
            None => {
                let candidate = format!("{original}__dup");
                if taken.insert(candidate.clone()) {
                    return candidate;
                }
                let mut extra = 2u32;
                loop {
                    let candidate = format!("{original}__dup{extra}");
                    if taken.insert(candidate.clone()) {
                        return candidate;
                    }
                    extra = extra.checked_add(1).unwrap_or(2);
                }
            }
        }
    }
}

fn calls_last(blocks: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let mut head = Vec::new();
    let mut calls = Vec::new();
    let mut called = false;
    let mut moved = false;
    for block in blocks {
        let (kind, _, _) = block_ids(block);
        if kind == "tool_use" {
            called = true;
            calls.push(block.clone());
            continue;
        }
        if !called || kind == "thinking" || kind == "redacted_thinking" {
            if called {
                calls.push(block.clone());
            } else {
                head.push(block.clone());
            }
            continue;
        }
        moved = true;
        if kind == "text" && text_of(block).is_none_or(|text| text.trim().is_empty()) {
            continue;
        }
        head.push(block.clone());
    }
    if !moved {
        return None;
    }
    head.extend(calls);
    Some(head)
}

struct UserDraft<'a> {
    element: &'a [u8],
    content: Option<&'a [u8]>,
    kept: Vec<Vec<u8>>,
    rest: Vec<Vec<u8>>,
    touched: bool,
}

fn draft_user<'a>(
    element: &'a [u8],
    content: Option<&'a [u8]>,
    pending: &[Call],
    answered: &mut HashSet<String>,
) -> Option<UserDraft<'a>> {
    let mut kept = Vec::new();
    let mut rest = Vec::new();
    let mut touched = false;
    match content {
        Some(content) if content.first() == Some(&b'[') => {
            let blocks = elements(content)?;
            for block in blocks {
                place_block(block, pending, answered, &mut kept, &mut rest, &mut touched);
            }
        }
        Some(content) if content.first() == Some(&b'"') => rest.push(content.to_vec()),
        _ => {}
    }
    Some(UserDraft {
        element,
        content,
        kept,
        rest,
        touched,
    })
}

fn mend_user(
    element: &[u8],
    content: Option<&[u8]>,
    pending: &[Call],
    answered: &mut HashSet<String>,
    finish: bool,
) -> UserMend {
    let Some(draft) = draft_user(element, content, pending, answered) else {
        return UserMend::Invalid;
    };
    let missing = if finish {
        missing_results(pending, answered)
    } else {
        Vec::new()
    };
    emit_user(&draft, &missing)
}

fn emit_user(draft: &UserDraft<'_>, missing: &[Vec<u8>]) -> UserMend {
    if !draft.touched && missing.is_empty() {
        return UserMend::Same;
    }
    let mut rest = draft.rest.clone();
    if rest.len() == 1 && rest[0].first() == Some(&b'"') {
        rest = text_blocks_from_json_string(Some(&rest[0]));
    }
    let mut blocks = draft.kept.clone();
    blocks.extend(missing.iter().cloned());
    blocks.extend(rest);
    let content = raw_list(&blocks);
    // A message that arrived without `content` still keeps its other members.
    let message = if draft.content.is_none() {
        with_member(draft.element, "content", &content)
    } else {
        with_top_value(draft.element, "content", &content)
    };
    UserMend::Changed(message)
}

fn release_held(out: &mut Vec<Vec<u8>>, run: Vec<Vec<u8>>, held: &mut Vec<Vec<u8>>) {
    if held.is_empty() {
        out.extend(run);
        return;
    }
    // The steer stood between the call and its results. It follows the last
    // result, so a later text-only user in this run does not come first.
    let Some(last) = run
        .iter()
        .rposition(|message| message_has_tool_result(message))
    else {
        out.extend(run);
        out.append(held);
        return;
    };
    out.extend(run[..last + 1].iter().cloned());
    out.append(held);
    out.extend(run.into_iter().skip(last + 1));
}

fn message_has_tool_result(message: &[u8]) -> bool {
    let Some((_, Some(content))) = message_view(message) else {
        return false;
    };
    if content.first() != Some(&b'[') {
        return false;
    }
    let Some(blocks) = elements(content) else {
        return false;
    };
    blocks
        .iter()
        .any(|block| block_ids(block).0 == "tool_result")
}

fn place_block(
    block: &[u8],
    pending: &[Call],
    answered: &mut HashSet<String>,
    kept: &mut Vec<Vec<u8>>,
    rest: &mut Vec<Vec<u8>>,
    touched: &mut bool,
) {
    let (kind, _, mut use_id) = block_ids(block);
    if kind != "tool_result" {
        rest.push(block.to_vec());
        return;
    }
    let mut owned = block.to_vec();
    if let Some(call) = match_call(pending, answered, &use_id)
        && call != use_id
    {
        owned = with_string_member(&owned, "tool_use_id", &call);
        use_id = call;
        *touched = true;
    }
    if !open(pending, &use_id) {
        rest.extend(raw_result_said(&owned));
        *touched = true;
        return;
    }
    if answered.contains(&use_id) {
        *touched = true;
        return;
    }
    answered.insert(use_id);
    if !rest.is_empty() {
        *touched = true;
    }
    kept.push(owned);
}

fn match_call(pending: &[Call], answered: &HashSet<String>, use_id: &str) -> Option<String> {
    if let Some(call) = pending
        .iter()
        .find(|call| call.final_id == use_id && !answered.contains(&call.final_id))
    {
        return Some(call.final_id.clone());
    }
    pending
        .iter()
        .find(|call| call.original == use_id && !answered.contains(&call.final_id))
        .map(|call| call.final_id.clone())
}

fn open(pending: &[Call], id: &str) -> bool {
    pending.iter().any(|call| call.final_id == id)
}

fn missing_results(pending: &[Call], answered: &HashSet<String>) -> Vec<Vec<u8>> {
    pending
        .iter()
        .filter(|call| !answered.contains(&call.final_id))
        .map(|call| synthetic_result(&call.final_id))
        .collect()
}

fn message_view(element: &[u8]) -> Option<(String, Option<&[u8]>)> {
    let members = members_of(trim(element))?;
    let mut role = String::new();
    let mut content = None;
    for member in members {
        if member.key == "role" {
            if let Some(text) = json_string(member.value) {
                role = text;
            }
        } else if member.key == "content" {
            content = Some(member.value);
        }
    }
    Some((role, content))
}

fn block_ids(block: &[u8]) -> (String, String, String) {
    let mut kind = String::new();
    let mut id = String::new();
    let mut use_id = String::new();
    let Some(members) = members_of(trim(block)) else {
        return (kind, id, use_id);
    };
    for member in members {
        let Some(text) = json_string(member.value) else {
            continue;
        };
        match member.key.as_str() {
            "type" => kind = text,
            "id" => id = text,
            "tool_use_id" => use_id = text,
            _ => {}
        }
    }
    (kind, id, use_id)
}

fn text_of(block: &[u8]) -> Option<String> {
    json_string(member_value(block, "text")?)
}

fn raw_result_said(block: &[u8]) -> Vec<Vec<u8>> {
    let Some(content) = member_value(block, "content") else {
        return text_blocks_from_json_string(None);
    };
    if content.first() == Some(&b'[')
        && let Some(parts) = elements(content)
        && !parts.is_empty()
    {
        return parts.into_iter().map(<[u8]>::to_vec).collect();
    }
    if content.first() == Some(&b'"')
        && let Some(text) = json_string(content)
        && !text.trim().is_empty()
    {
        return vec![text_block(&text)];
    }
    text_blocks_from_json_string(None)
}

fn text_blocks_from_json_string(raw: Option<&[u8]>) -> Vec<Vec<u8>> {
    let text = raw
        .and_then(json_string)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| STAND_IN.to_owned());
    vec![text_block(&text)]
}

fn text_block(text: &str) -> Vec<u8> {
    to_json(&serde_json::json!({"type": "text", "text": text}))
}

fn synthetic_result(id: &str) -> Vec<u8> {
    to_json(&serde_json::json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": INTERRUPTED,
        "is_error": true
    }))
}

fn results_message(blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = br#"{"role":"user","content":"#.to_vec();
    out.extend(raw_list(blocks));
    out.push(b'}');
    out
}

fn raw_list(elements: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(b'[');
    for (index, element) in elements.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        out.extend_from_slice(element);
    }
    out.push(b']');
    out
}

fn to_json(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec())
}

fn with_string_member(object: &[u8], key: &str, value: &str) -> Vec<u8> {
    with_top_value(object, key, &to_json(&Value::String(value.to_owned())))
}

fn with_member(object: &[u8], key: &str, value: &[u8]) -> Vec<u8> {
    let start = skip_ws(object, 0);
    let raw = trim(&object[start..]);
    let Some(members) = members_of(raw) else {
        return object.to_vec();
    };
    if members.iter().any(|member| member.key == key) {
        return with_top_value(object, key, value);
    }
    if raw.last() != Some(&b'}') {
        return object.to_vec();
    }
    let close_at = start + raw.len() - 1;
    let mut out = Vec::with_capacity(object.len() + key.len() + value.len() + 4);
    out.extend_from_slice(&object[..close_at]);
    if !members.is_empty() {
        out.push(b',');
    }
    out.push(b'"');
    out.extend_from_slice(key.as_bytes());
    out.extend_from_slice(b"\":");
    out.extend_from_slice(value);
    out.extend_from_slice(&object[close_at..]);
    out
}

fn with_top_value(object: &[u8], key: &str, value: &[u8]) -> Vec<u8> {
    let start = skip_ws(object, 0);
    let raw = trim(&object[start..]);
    let Some(members) = members_of(raw) else {
        return object.to_vec();
    };
    let Some(member) = members.iter().rev().find(|member| member.key == key) else {
        return object.to_vec();
    };
    let mut out =
        Vec::with_capacity(object.len() - (member.value_end - member.value_start) + value.len());
    out.extend_from_slice(&object[..start + member.value_start]);
    out.extend_from_slice(value);
    out.extend_from_slice(&object[start + member.value_end..]);
    out
}

fn top_member<'a>(body: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let raw = trim(body);
    members_of(raw)?
        .into_iter()
        .rev()
        .find(|member| member.key == key)
        .map(|member| member.value)
}

fn member_value<'a>(object: &'a [u8], key: &str) -> Option<&'a [u8]> {
    members_of(trim(object))?
        .into_iter()
        .rev()
        .find(|member| member.key == key)
        .map(|member| member.value)
}

fn members_of(raw: &[u8]) -> Option<Vec<Member<'_>>> {
    if raw.first() != Some(&b'{') {
        return None;
    }
    let mut index = skip_ws(raw, 1);
    let mut members = Vec::new();
    if raw.get(index) == Some(&b'}') {
        return Some(members);
    }
    loop {
        if raw.get(index) != Some(&b'"') {
            return None;
        }
        let key_at = index;
        index = skip_string(raw, index)?;
        let key = json_string(&raw[key_at..index])?;
        index = skip_ws(raw, index);
        if raw.get(index) != Some(&b':') {
            return None;
        }
        index = skip_ws(raw, index + 1);
        let value_start = index;
        index = skip_value(raw, index, 0)?;
        let value_end = index;
        members.push(Member {
            key,
            value: &raw[value_start..value_end],
            value_start,
            value_end,
        });
        index = skip_ws(raw, index);
        match raw.get(index) {
            Some(b'}') => return Some(members),
            Some(b',') => index = skip_ws(raw, index + 1),
            _ => return None,
        }
    }
}

fn elements(array: &[u8]) -> Option<Vec<&[u8]>> {
    let raw = trim(array);
    if raw.first() != Some(&b'[') {
        return None;
    }
    let mut index = skip_ws(raw, 1);
    if raw.get(index) == Some(&b']') {
        return Some(Vec::new());
    }
    let mut items = Vec::new();
    loop {
        let start = index;
        index = skip_value(raw, index, 0)?;
        items.push(&raw[start..index]);
        index = skip_ws(raw, index);
        match raw.get(index) {
            Some(b']') => return Some(items),
            Some(b',') => index = skip_ws(raw, index + 1),
            _ => return None,
        }
    }
}

fn json_string(raw: &[u8]) -> Option<String> {
    if raw.first() != Some(&b'"') {
        return None;
    }
    serde_json::from_slice(raw).ok()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_ws(data: &[u8], mut index: usize) -> usize {
    while data.get(index).is_some_and(|byte| is_ws(*byte)) {
        index += 1;
    }
    index
}

fn trim(data: &[u8]) -> &[u8] {
    let start = skip_ws(data, 0);
    let mut end = data.len();
    while end > start && is_ws(data[end - 1]) {
        end -= 1;
    }
    &data[start..end]
}

fn skip_value(data: &[u8], index: usize, depth: usize) -> Option<usize> {
    if depth > MAX_DEPTH {
        return None;
    }
    match data.get(index)? {
        b'"' => skip_string(data, index),
        b'{' => skip_object(data, index, depth),
        b'[' => skip_array(data, index, depth),
        b't' => skip_literal(data, index, b"true"),
        b'f' => skip_literal(data, index, b"false"),
        b'n' => skip_literal(data, index, b"null"),
        b'-' | b'0'..=b'9' => skip_number(data, index),
        _ => None,
    }
}

fn skip_object(data: &[u8], index: usize, depth: usize) -> Option<usize> {
    if data.get(index) != Some(&b'{') {
        return None;
    }
    let mut index = skip_ws(data, index + 1);
    if data.get(index) == Some(&b'}') {
        return Some(index + 1);
    }
    loop {
        if data.get(index) != Some(&b'"') {
            return None;
        }
        index = skip_string(data, index)?;
        index = skip_ws(data, index);
        if data.get(index) != Some(&b':') {
            return None;
        }
        index = skip_ws(data, index + 1);
        index = skip_value(data, index, depth + 1)?;
        index = skip_ws(data, index);
        match data.get(index) {
            Some(b'}') => return Some(index + 1),
            Some(b',') => index = skip_ws(data, index + 1),
            _ => return None,
        }
    }
}

fn skip_array(data: &[u8], index: usize, depth: usize) -> Option<usize> {
    if data.get(index) != Some(&b'[') {
        return None;
    }
    let mut index = skip_ws(data, index + 1);
    if data.get(index) == Some(&b']') {
        return Some(index + 1);
    }
    loop {
        index = skip_value(data, index, depth + 1)?;
        index = skip_ws(data, index);
        match data.get(index) {
            Some(b']') => return Some(index + 1),
            Some(b',') => index = skip_ws(data, index + 1),
            _ => return None,
        }
    }
}

fn skip_string(data: &[u8], mut index: usize) -> Option<usize> {
    if data.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;
    while index < data.len() {
        match data[index] {
            b'"' => return Some(index + 1),
            b'\\' => {
                index += 1;
                match data.get(index)? {
                    b'u' => {
                        if index + 4 >= data.len() {
                            return None;
                        }
                        index += 5;
                    }
                    _ => index += 1,
                }
            }
            _ => index += 1,
        }
    }
    None
}

fn skip_literal(data: &[u8], index: usize, literal: &[u8]) -> Option<usize> {
    let end = index.checked_add(literal.len())?;
    (data.get(index..end) == Some(literal)).then_some(end)
}

fn skip_number(data: &[u8], mut index: usize) -> Option<usize> {
    if data.get(index) == Some(&b'-') {
        index += 1;
    }
    if data.get(index) == Some(&b'0') {
        index += 1;
    } else if data.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
        while data.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
    } else {
        return None;
    }
    if data.get(index) == Some(&b'.') {
        index += 1;
        let fraction = index;
        while data.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
        if index == fraction {
            return None;
        }
    }
    if data
        .get(index)
        .is_some_and(|byte| *byte == b'e' || *byte == b'E')
    {
        index += 1;
        if data
            .get(index)
            .is_some_and(|byte| *byte == b'+' || *byte == b'-')
        {
            index += 1;
        }
        let exponent = index;
        while data.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
        if index == exponent {
            return None;
        }
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::{INTERRUPTED, pair_messages_tool_blocks};
    use crate::{AnthropicMessages, Codec, OpenAiChat, OpenAiResponses};
    use serde_json::Value;

    const HEAD: &str = concat!(
        r#"{"model":"fake/m1","max_tokens":32000,"stream":true,"#,
        r#""system":[{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","cache_control":{"type":"ephemeral"}}],"#,
        r#""tools":[{"name":"Bash","description":"Run a command","input_schema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}},"#,
        r#"{"name":"Read","description":"Read a file","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}}}}],"#,
        r#""metadata":{"user_id":"user_abc_account__session_1"},"messages":"#
    );

    fn paired_body() -> String {
        format!(
            "{HEAD}[{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"list the files & read go.mod\"}}]}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"thinking\",\"thinking\":\"two calls\",\"signature\":\"EqQBCkYIBxgC\"}},{{\"type\":\"tool_use\",\"id\":\"toolu_01A\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}},{{\"type\":\"tool_use\",\"id\":\"toolu_01B\",\"name\":\"Read\",\"input\":{{\"file_path\":\"go.mod\"}}}}]}},{{\"content\":[{{\"tool_use_id\":\"toolu_01A\",\"type\":\"tool_result\",\"content\":\"go.mod\\nmain.go\"}},{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_01B\",\"content\":[{{\"type\":\"text\",\"text\":\"module x\"}}],\"cache_control\":{{\"type\":\"ephemeral\"}}}},{{\"type\":\"text\",\"text\":\"now build it\"}}],\"role\":\"user\"}}]}}"
        )
    }

    fn cut_body() -> String {
        format!(
            "{HEAD}[{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"list the files and read go.mod\"}}]}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_01A\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}},{{\"type\":\"tool_use\",\"id\":\"toolu_01B\",\"name\":\"Read\",\"input\":{{\"file_path\":\"go.mod\"}}}}]}},{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_01A\",\"content\":\"go.mod\\nmain.go\"}}]}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Done.\"}}]}},{{\"role\":\"user\",\"content\":\"go on\"}}]}}"
        )
    }

    fn messages_of(body: &[u8]) -> Vec<Vec<u8>> {
        let value = top_messages(body);
        super::elements(value)
            .expect("messages")
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect()
    }

    fn top_messages(body: &[u8]) -> &[u8] {
        super::top_member(body, "messages").expect("messages")
    }

    fn json(body: &[u8]) -> Value {
        serde_json::from_slice(body).expect("json")
    }

    #[test]
    fn d228_paired_body_keeps_its_bytes() {
        let body = paired_body();
        let out = pair_messages_tool_blocks(body.as_bytes());
        assert_eq!(out, body.as_bytes());
        let plain = format!("{HEAD}[{{\"role\":\"user\",\"content\":\"hi\"}}]}}");
        assert_eq!(
            pair_messages_tool_blocks(plain.as_bytes()),
            plain.as_bytes()
        );
    }

    #[test]
    fn d228_spaced_paired_body_keeps_its_bytes() {
        let body = b"{\n  \"model\": \"m\",\n  \"messages\": [\n    {\"role\": \"assistant\", \"content\": [{\"type\": \"tool_use\", \"id\": \"a\", \"name\": \"n\", \"input\": {}}]},\n    {\"role\": \"user\", \"content\": [{\"type\": \"tool_result\", \"tool_use_id\": \"a\", \"content\": \"ok\"}]}\n  ]\n}\n";
        assert_eq!(pair_messages_tool_blocks(body), body);
    }

    #[test]
    fn d228_cut_call_gets_a_synthetic_result_and_other_messages_stay() {
        let body = cut_body();
        let out = pair_messages_tool_blocks(body.as_bytes());
        let before = messages_of(body.as_bytes());
        let after = messages_of(&out);
        assert_eq!(after.len(), 5);
        for index in [0, 1, 3, 4] {
            assert_eq!(after[index], before[index], "message {index}");
        }
        let message = json(&after[2]);
        let content = message["content"].as_array().expect("content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "toolu_01A");
        assert_eq!(content[1]["tool_use_id"], "toolu_01B");
        assert_eq!(content[1]["is_error"], true);
        assert_eq!(content[1]["content"], INTERRUPTED);
        assert!(out.starts_with(HEAD.as_bytes()));
    }

    #[test]
    fn d228_orphan_result_becomes_text() {
        let body = format!(
            "{HEAD}[{{\"role\":\"user\",\"content\":\"read go.mod\"}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"Reading.\"}}]}},{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_01Gone\",\"content\":\"module x\"}},{{\"type\":\"text\",\"text\":\"and?\"}}]}}]}}"
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let message = json(&messages_of(&out)[2]);
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][0]["text"], "module x");
        assert_eq!(message["content"][1]["text"], "and?");
    }

    #[test]
    fn d228_calls_with_no_user_turn_get_their_own_results() {
        let body = format!(
            "{HEAD}[{{\"role\":\"user\",\"content\":\"go\"}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}}]}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Bash\",\"input\":{{\"command\":\"pwd\"}}}}]}}]}}"
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let roles: Vec<_> = messages_of(&out)
            .iter()
            .map(|message| json(message)["role"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(roles.join(" "), "user assistant user assistant user");
        assert!(String::from_utf8_lossy(&out).contains(INTERRUPTED));
    }

    #[test]
    fn d228_second_answer_is_dropped_and_a_late_result_moves_first() {
        let body = format!(
            "{HEAD}[{{\"role\":\"user\",\"content\":\"go\"}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{{\"command\":\"ls\"}}}}]}},{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"here\"}},{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"first\"}},{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\",\"content\":\"again\"}}]}}]}}"
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let message = json(&messages_of(&out)[2]);
        assert_eq!(message["content"].as_array().expect("content").len(), 2);
        assert_eq!(message["content"][0]["type"], "tool_result");
        assert_eq!(message["content"][0]["content"], "first");
        assert_eq!(message["content"][1]["text"], "here");
        assert!(!out.windows(b"again".len()).any(|window| window == b"again"));
    }

    #[test]
    fn d228_unreadable_body_keeps_its_bytes() {
        for body in [
            &br#"{"messages":[{"role":"user","content":"tool_use"}"#[..],
            &br#"{"model":"m","messages":"tool_use"}"#[..],
            &br#"{"messages":[1],"tool_use":true}"#[..],
            &b""[..],
        ] {
            assert_eq!(pair_messages_tool_blocks(body), body);
        }
    }

    #[test]
    fn d228_words_alone_follow_the_synthetic_result() {
        let body = format!(
            "{HEAD}[{{\"role\":\"user\",\"content\":\"go\"}},{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Bash\",\"input\":{{}}}}]}},{{\"role\":\"user\",\"content\":\"go on\"}}]}}"
        );
        let message = json(&messages_of(&pair_messages_tool_blocks(body.as_bytes()))[2]);
        assert_eq!(message["content"][0]["type"], "tool_result");
        assert_eq!(message["content"][0]["content"], INTERRUPTED);
        assert_eq!(message["content"][1]["text"], "go on");
    }

    #[test]
    fn d236_words_after_a_call_move_before_it() {
        let call = "call_00_wIys8uRO8314ixexw3r35173";
        let cases = [
            (
                format!(
                    r#"[{{"type":"thinking","thinking":"Write the mock.","signature":""}},{{"type":"tool_use","id":"{call}","name":"Write","input":{{}}}},{{"type":"text","text":"Now the server."}}]"#
                ),
                "thinking,text,tool_use",
            ),
            (
                format!(
                    r#"[{{"type":"thinking","thinking":"Two files.","signature":""}},{{"type":"tool_use","id":"{call}","name":"Write","input":{{}}}},{{"type":"text","text":"And the other."}},{{"type":"tool_use","id":"call_01_b","name":"Write","input":{{}}}}]"#
                ),
                "thinking,text,tool_use,tool_use",
            ),
            (
                format!(
                    r#"[{{"type":"text","text":"Writing."}},{{"type":"tool_use","id":"{call}","name":"Write","input":{{}}}},{{"type":"thinking","thinking":"Then the server.","signature":""}},{{"type":"text","text":"Next, the server."}}]"#
                ),
                "text,text,tool_use,thinking",
            ),
        ];
        for (content, order) in cases {
            let body = format!(
                r#"{{"model":"m","messages":[{{"role":"user","content":"refactor"}},{{"role":"assistant","content":{content}}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{call}","content":"File created successfully"}},{{"type":"tool_result","tool_use_id":"call_01_b","content":"File created successfully"}}]}}]}}"#
            );
            let out = pair_messages_tool_blocks(body.as_bytes());
            assert!(
                !String::from_utf8_lossy(&out).contains(INTERRUPTED),
                "{order}"
            );
            let kinds: Vec<_> = json(&messages_of(&out)[1])["content"]
                .as_array()
                .expect("content")
                .iter()
                .map(|block| block["type"].as_str().unwrap_or("").to_owned())
                .collect();
            assert_eq!(kinds.join(","), order);
        }
    }

    #[test]
    fn d236_empty_text_after_a_call_is_dropped_and_an_image_moves() {
        let body = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"Write it.","signature":""},{"type":"tool_use","id":"call_00","name":"Write","input":{}},{"type":"text","text":" "},{"type":"text","text":""},{"type":"image","source":{"type":"url","url":"https://example.test/a.png"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_00","content":"ok"}]}]}"#;
        let out = pair_messages_tool_blocks(body);
        let kinds: Vec<_> = json(&messages_of(&out)[0])["content"]
            .as_array()
            .expect("content")
            .iter()
            .map(|block| block["type"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(kinds.join(","), "thinking,image,tool_use");
        assert!(!String::from_utf8_lossy(&out).contains(INTERRUPTED));
    }

    #[test]
    fn d236_in_shape_call_order_keeps_its_bytes() {
        let body = r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"a","signature":""},{"type":"text","text":"b"},{"type":"tool_use","id":"x","name":"W","input":{}},{"type":"thinking","thinking":"c","signature":""},{"type":"tool_use","id":"y","name":"W","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"1"},{"type":"tool_result","tool_use_id":"y","content":"2"}]}]}"#;
        assert_eq!(pair_messages_tool_blocks(body.as_bytes()), body.as_bytes());
    }

    #[test]
    fn d236_system_and_developer_between_a_call_and_its_result_follow_the_result() {
        let assistant = r#"{"role":"assistant","content":[{"type":"thinking","thinking":"I will list the files.","signature":""},{"type":"text","text":"Let me look."},{"type":"tool_use","id":"call_00_jdrDF4PKrnFwMUMBRQlr4516","name":"Bash","input":{"command":"ls"}}]}"#;
        let result = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_00_jdrDF4PKrnFwMUMBRQlr4516","content":"a.go"}]}"#;
        let between = [
            r#"{"role":"system","content":[{"type":"text","text":"The user sent a new message while you were working:\nhi"}]}"#,
            r#"{"role":"system","content":[],"output_config":{"effort":"max"}}"#,
            r#"{"role":"system","output_config":{"effort":"max"}}"#,
            r#"{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_reference","name":"Read"}}]}"#,
            r#"{"role":"system","content":"note"}"#,
            r#"{"role":"developer","content":"steer"}"#,
        ];
        for message in between {
            let body = format!(r#"{{"model":"m","messages":[{assistant},{message},{result}]}}"#);
            let want_messages = format!("[{assistant},{result},{message}]");
            let out = pair_messages_tool_blocks(body.as_bytes());
            let got = top_messages(&out);
            assert_eq!(got, want_messages.as_bytes(), "{message}");
            assert!(String::from_utf8_lossy(&out).contains(r#""content":"a.go""#));
            assert!(!String::from_utf8_lossy(&out).contains(INTERRUPTED));
            let already = format!(r#"{{"model":"m","messages":[{assistant},{result},{message}]}}"#);
            assert_eq!(
                pair_messages_tool_blocks(already.as_bytes()),
                already.as_bytes(),
                "{message}"
            );
        }
    }

    #[test]
    fn d236_system_with_no_result_follows_the_synthetic_one() {
        let body = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call_00_jdrDF4PKrnFwMUMBRQlr4516","name":"Bash","input":{"command":"ls"}}]},{"role":"system","content":[{"type":"text","text":"The user sent a new message while you were working:\nhi"}]},{"role":"assistant","content":[{"type":"text","text":"done"}]}]}"#;
        let roles: Vec<_> = messages_of(&pair_messages_tool_blocks(body))
            .iter()
            .map(|message| json(message)["role"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(roles.join(","), "assistant,user,system,assistant");
    }

    #[test]
    fn d236_duplicate_call_ids_stay_two_ids_each_answered() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":"go"},{"role":"assistant","content":[{"type":"tool_use","id":"call_00_dup","name":"Bash","input":{"command":"date"}},{"type":"tool_use","id":"call_00_dup","name":"Bash","input":{"command":"date"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_00_dup","content":"ANSWER-ONE"},{"type":"tool_result","tool_use_id":"call_00_dup","content":"ANSWER-TWO"}]}]}"#;
        let out = pair_messages_tool_blocks(body);
        let value = json(&out);
        let calls: Vec<_> = value["messages"][1]["content"]
            .as_array()
            .expect("calls")
            .iter()
            .map(|block| block["id"].as_str().unwrap_or("").to_owned())
            .collect();
        let results: Vec<_> = value["messages"][2]["content"]
            .as_array()
            .expect("results")
            .iter()
            .map(|block| block["tool_use_id"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(results, calls);
        assert_eq!(calls[0], "call_00_dup");
        assert_ne!(calls[0], calls[1]);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("ANSWER-ONE"));
        assert!(text.contains("ANSWER-TWO"));
        assert!(!text.contains(INTERRUPTED));
        let again = pair_messages_tool_blocks(&out);
        assert_eq!(again, out);
    }

    #[test]
    fn d228_encode_mends_a_cut_call_for_create_and_count() {
        let request = AnthropicMessages
            .decode_request(cut_body().as_bytes())
            .expect("decode");
        let encoded = AnthropicMessages.encode_request(&request).expect("encode");
        let value = json(&encoded);
        assert_eq!(
            value["messages"][2]["content"][1]["tool_use_id"],
            "toolu_01B"
        );
        assert_eq!(value["messages"][2]["content"][1]["is_error"], true);
        assert_eq!(pair_messages_tool_blocks(&encoded), encoded);
    }

    #[test]
    fn d236_encode_moves_words_and_keeps_a_mid_conversation_system_after_the_result() {
        let body = r#"{"model":"m","max_tokens":16,"messages":[{"role":"user","content":"list"},{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"Bash","input":{}},{"type":"text","text":"after"}]},{"role":"system","content":"note"},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"a.go"}]}]}"#;
        let request = AnthropicMessages
            .decode_request(body.as_bytes())
            .expect("decode");
        let encoded = AnthropicMessages.encode_request(&request).expect("encode");
        let value = json(&encoded);
        assert!(value.get("system").is_none());
        assert_eq!(value["messages"][1]["content"][0]["text"], "after");
        assert_eq!(value["messages"][1]["content"][1]["type"], "tool_use");
        assert_eq!(value["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(value["messages"][2]["content"][0]["content"], "a.go");
        assert_eq!(value["messages"][3]["role"], "system");
        assert!(
            !encoded
                .windows(INTERRUPTED.len())
                .any(|window| window == INTERRUPTED.as_bytes())
        );
        assert_eq!(pair_messages_tool_blocks(&encoded), encoded);
    }

    #[test]
    fn d236_encode_keeps_a_leading_system_channel() {
        let body = r#"{"model":"m","max_tokens":8,"system":"rules","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"n","input":{}}]},{"role":"developer","content":"between"},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
        let request = AnthropicMessages
            .decode_request(body.as_bytes())
            .expect("decode");
        let value = json(&AnthropicMessages.encode_request(&request).expect("encode"));
        assert_eq!(value["system"], "rules");
        let messages = value["messages"].as_array().expect("messages");
        assert!(messages.iter().all(|message| {
            let role = message["role"].as_str().unwrap_or("");
            role != "system" && role != "developer"
        }));
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "t");
        assert_eq!(messages[2]["content"][0]["content"], "ok");
        assert_eq!(
            messages[2]["content"][1]["text"],
            "<system-reminder>\nbetween\n</system-reminder>"
        );
    }

    #[test]
    fn d236_messages_form_keeps_consecutive_same_role_cache_marks() {
        let body = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"one","cache_control":{"type":"ephemeral"}},{"role":"user","content":"two","cache_control":{"type":"ephemeral","ttl":"1h"}},{"role":"assistant","content":"a","cache_control":{"type":"ephemeral"}},{"role":"assistant","content":"b","cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#;
        let request = AnthropicMessages
            .decode_request(body.as_bytes())
            .expect("decode");
        let value = json(&AnthropicMessages.encode_request(&request).expect("encode"));
        let messages = value["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["content"][0]["text"], "one");
        assert_eq!(messages[0]["cache_control"]["type"], "ephemeral");
        assert!(messages[0]["cache_control"].get("ttl").is_none());
        assert_eq!(messages[1]["content"][0]["text"], "two");
        assert_eq!(messages[1]["cache_control"]["ttl"], "1h");
        assert_eq!(messages[2]["content"][0]["text"], "a");
        assert_eq!(messages[2]["cache_control"]["type"], "ephemeral");
        assert!(messages[2]["cache_control"].get("ttl").is_none());
        assert_eq!(messages[3]["content"][0]["text"], "b");
        assert_eq!(messages[3]["cache_control"]["ttl"], "5m");
    }

    #[test]
    fn d236_messages_form_developer_keeps_members_and_separates_text() {
        let body = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"},{"role":"developer","content":[{"type":"text","text":"steer"},{"type":"text","text":"more"}],"output_config":{"effort":"max"}},{"role":"user","content":"bye"}]}"#;
        let request = AnthropicMessages
            .decode_request(body.as_bytes())
            .expect("decode");
        let value = json(&AnthropicMessages.encode_request(&request).expect("encode"));
        let messages = value["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"][0]["text"], "hi");
        assert_eq!(messages[2]["content"][0]["text"], "bye");
        let reminder = &messages[1];
        assert_eq!(reminder["role"], "user");
        assert_eq!(
            reminder["content"][0]["text"],
            "<system-reminder>\nsteer\nmore\n</system-reminder>"
        );
        assert_eq!(reminder["output_config"]["effort"], "max");
    }

    #[test]
    fn d236_a_folded_developer_keeps_members_on_the_tool_result() {
        let body = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"n","input":{}}]},{"role":"developer","content":[{"type":"text","text":"steer"},{"type":"text","text":"more"}],"output_config":{"effort":"max"}},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}],"cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        let request = AnthropicMessages
            .decode_request(body.as_bytes())
            .expect("decode");
        let value = json(&AnthropicMessages.encode_request(&request).expect("encode"));
        let messages = value["messages"].as_array().expect("messages");
        assert!(messages.iter().all(|message| {
            let role = message["role"].as_str().unwrap_or("");
            role != "system" && role != "developer"
        }));
        let result = &messages[2];
        assert_eq!(result["content"][0]["type"], "tool_result");
        assert_eq!(result["content"][0]["tool_use_id"], "t");
        assert_eq!(
            result["content"][1]["text"],
            "<system-reminder>\nsteer\nmore\n</system-reminder>"
        );
        assert_eq!(result["output_config"]["effort"], "max");
        assert_eq!(result["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn d236_translated_chat_folds_a_mid_system_and_developer_into_reminders() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":"list"},{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{}"}}]},{"role":"system","content":"note"},{"role":"tool","tool_call_id":"call_1","content":"a.go"},{"role":"developer","content":"steer"}]}"#;
        let mut request = OpenAiChat.decode_request(body.as_bytes()).expect("decode");
        request.strip_extensions();
        let encoded = AnthropicMessages.encode_request(&request).expect("encode");
        assert_folded_exchange(&encoded, "call_1", &["note", "steer"]);
    }

    #[test]
    fn d236_translated_responses_fold_a_mid_developer_into_a_reminder() {
        let body = r#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"list"}]},{"type":"function_call","call_id":"call_1","name":"Bash","arguments":"{}"},{"type":"message","role":"developer","content":[{"type":"input_text","text":"steer"}]},{"type":"function_call_output","call_id":"call_1","output":"a.go"},{"type":"message","role":"system","content":[{"type":"input_text","text":"note"}]}]}"#;
        let mut request = OpenAiResponses
            .decode_request(body.as_bytes())
            .expect("decode");
        request.strip_extensions();
        let encoded = AnthropicMessages.encode_request(&request).expect("encode");
        assert_folded_exchange(&encoded, "call_1", &["steer", "note"]);
    }

    #[test]
    fn d236_translated_chat_keeps_two_tool_results_with_a_following_user() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":"list"},{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{}"}},{"id":"call_2","type":"function","function":{"name":"Read","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_1","content":"a.go"},{"role":"tool","tool_call_id":"call_2","content":"b.go"},{"role":"user","content":"thanks"}]}"#;
        let mut request = OpenAiChat.decode_request(body.as_bytes()).expect("decode");
        request.strip_extensions();
        let encoded = AnthropicMessages.encode_request(&request).expect("encode");
        assert_two_results(&encoded, "thanks");
    }

    #[test]
    fn d236_translated_responses_keeps_two_tool_results_with_a_following_user() {
        let body = r#"{"model":"m","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"list"}]},{"type":"function_call","call_id":"call_1","name":"Bash","arguments":"{}"},{"type":"function_call","call_id":"call_2","name":"Read","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"a.go"},{"type":"function_call_output","call_id":"call_2","output":"b.go"},{"type":"message","role":"user","content":[{"type":"input_text","text":"thanks"}]}]}"#;
        let mut request = OpenAiResponses
            .decode_request(body.as_bytes())
            .expect("decode");
        request.strip_extensions();
        let encoded = AnthropicMessages.encode_request(&request).expect("encode");
        assert_two_results(&encoded, "thanks");
    }

    #[test]
    fn d236_split_user_results_stay_in_their_messages_and_keep_marks() {
        let first = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"one"}],"cache_control":{"type":"ephemeral"}}"#;
        let second = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":"two"}],"cache_control":{"type":"ephemeral","ttl":"1h"}}"#;
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"a","name":"n","input":{{}}}},{{"type":"tool_use","id":"b","name":"n","input":{{}}}}]}}, {first}, {second}]}}"#
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let value = json(&out);
        let messages = value["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "a");
        assert_eq!(messages[1]["content"][0]["content"], "one");
        assert_eq!(messages[1]["cache_control"]["type"], "ephemeral");
        assert!(messages[1]["cache_control"].get("ttl").is_none());
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "b");
        assert_eq!(messages[2]["content"][0]["content"], "two");
        assert_eq!(messages[2]["cache_control"]["ttl"], "1h");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(first), "{text}");
        assert!(text.contains(second), "{text}");
        assert!(!text.contains(INTERRUPTED));
    }

    #[test]
    fn d236_unanswered_call_is_synthetic_in_the_first_user_message() {
        let first = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"one"},{"type":"text","text":"note"}],"cache_control":{"type":"ephemeral"}}"#;
        let second = r#"{"role":"user","content":"thanks","name":"ada"}"#;
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"a","name":"n","input":{{}}}},{{"type":"tool_use","id":"b","name":"n","input":{{}}}}]}}, {first}, {second}]}}"#
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let value = json(&out);
        let messages = value["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        let repaired = messages[1]["content"].as_array().expect("content");
        assert_eq!(repaired.len(), 3);
        assert_eq!(repaired[0]["type"], "tool_result");
        assert_eq!(repaired[0]["tool_use_id"], "a");
        assert_eq!(repaired[0]["content"], "one");
        assert!(repaired[0].get("is_error").is_none());
        assert_eq!(repaired[1]["type"], "tool_result");
        assert_eq!(repaired[1]["tool_use_id"], "b");
        assert_eq!(repaired[1]["is_error"], true);
        assert_eq!(repaired[1]["content"], INTERRUPTED);
        assert_eq!(repaired[2]["type"], "text");
        assert_eq!(repaired[2]["text"], "note");
        assert_eq!(messages[1]["cache_control"]["type"], "ephemeral");
        assert!(messages[1]["cache_control"].get("ttl").is_none());
        assert_eq!(messages[2]["content"], "thanks");
        assert_eq!(messages[2]["name"], "ada");
        assert!(messages[2].get("tool_use_id").is_none());
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(second), "{text}");
        assert_eq!(text.matches(INTERRUPTED).count(), 1);
        assert!(!text[text.find(second).expect("second")..].contains(INTERRUPTED));
    }

    #[test]
    fn d236_held_system_is_released_after_the_last_result() {
        let steer = r#"{"role":"system","content":"steer","name":"held"}"#;
        let result = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"one"}],"cache_control":{"type":"ephemeral"}}"#;
        let thanks = r#"{"role":"user","content":"thanks","name":"ada"}"#;
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"a","name":"n","input":{{}}}},{{"type":"tool_use","id":"b","name":"n","input":{{}}}}]}}, {steer}, {result}, {thanks}]}}"#
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let value = json(&out);
        let messages = value["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "assistant");
        let repaired = messages[1]["content"].as_array().expect("content");
        assert_eq!(repaired[0]["type"], "tool_result");
        assert_eq!(repaired[0]["tool_use_id"], "a");
        assert_eq!(repaired[0]["content"], "one");
        assert_eq!(repaired[1]["type"], "tool_result");
        assert_eq!(repaired[1]["tool_use_id"], "b");
        assert_eq!(repaired[1]["content"], INTERRUPTED);
        assert_eq!(messages[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(messages[2]["role"], "system");
        assert_eq!(messages[2]["content"], "steer");
        assert_eq!(messages[2]["name"], "held");
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "thanks");
        assert_eq!(messages[3]["name"], "ada");
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(steer), "{text}");
        assert!(text.contains(thanks), "{text}");
        assert!(
            !json(&messages_of(&out)[3])
                .to_string()
                .contains(INTERRUPTED)
        );
    }

    #[test]
    fn d236_a_receiving_message_without_content_keeps_its_members() {
        let user = r#"{"role":"user","cache_control":{"type":"ephemeral"},"output_config":{"effort":"max"}}"#;
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"assistant","content":[{{"type":"tool_use","id":"a","name":"n","input":{{}}}}]}}, {user}]}}"#
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let value = json(&out);
        let messages = value["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(messages[1]["output_config"]["effort"], "max");
        let content = messages[1]["content"].as_array().expect("content");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "a");
        assert_eq!(content[0]["is_error"], true);
        assert_eq!(content[0]["content"], INTERRUPTED);
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.contains(r#""cache_control":{"type":"ephemeral"}"#),
            "{text}"
        );
        assert!(
            text.contains(r#""output_config":{"effort":"max"}"#),
            "{text}"
        );
    }

    #[test]
    fn d236_hand_built_exchanges_pair_every_call_and_keep_members() {
        let chat = r#"{"model":"m","messages":[{"role":"system","content":"rules"},{"role":"user","content":"list"},{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{}"}},{"id":"call_2","type":"function","function":{"name":"Read","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_1","content":"a.go","cache_control":{"type":"ephemeral"}},{"role":"developer","content":[{"type":"text","text":"steer"},{"type":"text","text":"more"}]},{"role":"tool","tool_call_id":"call_2","content":"b.go"},{"role":"user","content":"thanks","name":"ada"}]}"#;
        let chat_request = OpenAiChat.decode_request(chat.as_bytes()).expect("decode");
        let chat_encoded = AnthropicMessages
            .encode_request(&chat_request)
            .expect("encode");
        assert_answer_run(&chat_encoded, &["call_1", "call_2"], &["a.go", "b.go"]);
        let chat_value = json(&chat_encoded);
        assert_eq!(chat_value["system"], "rules");
        let chat_answer = answer_message(&chat_value, 0);
        assert_eq!(chat_answer["name"], "ada");
        assert_eq!(chat_answer["cache_control"]["type"], "ephemeral");
        assert!(
            chat_answer["content"]
                .as_array()
                .expect("content")
                .iter()
                .any(|block| block["text"] == "<system-reminder>\nsteermore\n</system-reminder>")
        );

        let responses = r#"{"model":"m","input":[{"type":"message","role":"system","content":[{"type":"input_text","text":"rules"}]},{"type":"message","role":"user","content":[{"type":"input_text","text":"list"}]},{"type":"function_call","call_id":"call_1","name":"Bash","arguments":"{}"},{"type":"function_call","call_id":"call_2","name":"Read","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"a.go","name":"left"},{"type":"message","role":"developer","content":[{"type":"input_text","text":"steer"}]},{"type":"function_call_output","call_id":"call_2","output":"b.go"},{"type":"message","role":"user","content":[{"type":"input_text","text":"thanks"}],"name":"ada"}]}"#;
        let responses_request = OpenAiResponses
            .decode_request(responses.as_bytes())
            .expect("decode");
        let responses_encoded = AnthropicMessages
            .encode_request(&responses_request)
            .expect("encode");
        assert_answer_run(&responses_encoded, &["call_1", "call_2"], &["a.go", "b.go"]);
        let responses_value = json(&responses_encoded);
        let responses_answer = answer_message(&responses_value, 0);
        assert_eq!(responses_answer["name"], "ada");
        let left = responses_answer["content"]
            .as_array()
            .expect("content")
            .iter()
            .find(|block| block["tool_use_id"] == "call_1")
            .expect("call_1");
        assert_eq!(left["name"], "left");
        assert_eq!(left["type"], "tool_result");

        let messages = r#"{"model":"m","max_tokens":8,"system":"rules","messages":[{"role":"user","content":"list","cache_control":{"type":"ephemeral"}},{"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"Bash","input":{}},{"type":"tool_use","id":"call_2","name":"Read","input":{}}],"cache_control":{"type":"ephemeral","ttl":"5m"}},{"role":"developer","content":[{"type":"text","text":"steer"},{"type":"text","text":"more"}],"output_config":{"effort":"max"}},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"a.go"}],"cache_control":{"type":"ephemeral","ttl":"1h"}},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_2","content":"b.go"}],"cache_control":{"type":"ephemeral","ttl":"5m"}}]}"#;
        let messages_request = AnthropicMessages
            .decode_request(messages.as_bytes())
            .expect("decode");
        let messages_encoded = AnthropicMessages
            .encode_request(&messages_request)
            .expect("encode");
        assert_answer_run(&messages_encoded, &["call_1", "call_2"], &["a.go", "b.go"]);
        let messages_value = json(&messages_encoded);
        assert_eq!(messages_value["system"], "rules");
        let messages_list = messages_value["messages"].as_array().expect("messages");
        assert_eq!(messages_list[0]["cache_control"]["type"], "ephemeral");
        assert!(messages_list[0]["cache_control"].get("ttl").is_none());
        assert_eq!(messages_list[1]["cache_control"]["ttl"], "5m");
        assert_eq!(messages_list.len(), 4);
        assert_eq!(messages_list[2]["cache_control"]["ttl"], "1h");
        assert_eq!(messages_list[2]["output_config"]["effort"], "max");
        assert_eq!(messages_list[3]["cache_control"]["ttl"], "5m");
        assert!(messages_list[3].get("output_config").is_none());
        assert_eq!(
            messages_list[2]["content"][1]["text"],
            "<system-reminder>\nsteer\nmore\n</system-reminder>"
        );
    }

    #[test]
    fn d236_duplicate_id_does_not_steal_a_later_real_suffix() {
        let body = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"x","name":"n","input":{}},{"type":"tool_use","id":"x","name":"n","input":{}},{"type":"tool_use","id":"x__2","name":"n","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"one"},{"type":"tool_result","tool_use_id":"x__2","content":"real"}]}]}"#;
        let out = pair_messages_tool_blocks(body);
        let value = json(&out);
        let calls: Vec<_> = value["messages"][0]["content"]
            .as_array()
            .expect("calls")
            .iter()
            .map(|block| block["id"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(calls[0], "x");
        assert_eq!(calls[2], "x__2");
        assert_ne!(calls[1], "x");
        assert_ne!(calls[1], "x__2");
        let results = value["messages"][1]["content"].as_array().expect("results");
        assert_eq!(results[0]["tool_use_id"], "x");
        assert_eq!(results[0]["content"], "one");
        let real = results
            .iter()
            .find(|block| block["content"] == "real")
            .expect("real result");
        assert_eq!(real["tool_use_id"], "x__2");
        assert_eq!(real["type"], "tool_result");
    }

    #[test]
    fn d228_orphan_result_with_a_block_array_keeps_its_image() {
        let image =
            r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aaa"}}"#;
        let body = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"gone","content":[{{"type":"text","text":"said"}},{image}]}}]}}]}}"#
        );
        let out = pair_messages_tool_blocks(body.as_bytes());
        let message = json(&messages_of(&out)[0]);
        let content = message["content"].as_array().expect("content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "said");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["data"], "aaa");
        assert!(
            !out.windows(b"tool_result".len())
                .any(|window| window == b"tool_result")
        );
        assert!(
            out.windows(image.len())
                .any(|window| window == image.as_bytes())
        );
    }

    #[test]
    fn d228_a_changed_message_keeps_its_other_members_and_the_tail() {
        let assistant = r#"{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"n","input":{}},{"type":"text","text":"after"}],"stop_reason":"tool_use"}"#;
        let user = r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"ok"}],"cache_control":{"type":"ephemeral"}}"#;
        let tail = r#","metadata":{"user_id":"abc"}"#;
        let body = format!(r#"{{"model":"m","messages":[{assistant},{user}]{tail}}}"#);
        let out = pair_messages_tool_blocks(body.as_bytes());
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains(r#""stop_reason":"tool_use""#));
        assert!(text.contains(user));
        assert!(out.ends_with(format!("{tail}}}").as_bytes()));
        let kinds: Vec<_> = json(&messages_of(&out)[0])["content"]
            .as_array()
            .expect("content")
            .iter()
            .map(|block| block["type"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(kinds.join(","), "text,tool_use");
    }

    #[test]
    fn d236_a_messages_system_turn_without_content_omits_content() {
        let body = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"n","input":{}}]},{"role":"system","output_config":{"effort":"max"}},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}]}"#;
        let request = AnthropicMessages
            .decode_request(body.as_bytes())
            .expect("decode");
        let value = json(&AnthropicMessages.encode_request(&request).expect("encode"));
        assert_eq!(value["messages"][2]["content"][0]["type"], "tool_result");
        let system = &value["messages"][3];
        assert_eq!(system["role"], "system");
        assert!(system.get("content").is_none(), "{system}");
        assert_eq!(system["output_config"]["effort"], "max");
    }

    fn assert_folded_exchange(encoded: &[u8], call_id: &str, notes: &[&str]) {
        let value = json(encoded);
        let messages = value["messages"].as_array().expect("messages");
        assert!(
            messages.iter().all(|message| {
                let role = message["role"].as_str().unwrap_or("");
                role != "system" && role != "developer"
            }),
            "{value}"
        );
        let roles: Vec<_> = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap_or("").to_owned())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user"]);
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["id"], call_id);
        let result = &messages[2]["content"];
        let paired = result
            .as_array()
            .expect("content")
            .iter()
            .find(|block| block["type"] == "tool_result")
            .expect("result");
        assert_eq!(paired["tool_use_id"], call_id);
        assert_eq!(paired["content"], "a.go");
        let text = result.to_string();
        for note in notes {
            assert!(
                text.contains(&format!("<system-reminder>\\n{note}\\n</system-reminder>")),
                "{text}"
            );
        }
        assert!(
            !encoded
                .windows(INTERRUPTED.len())
                .any(|window| window == INTERRUPTED.as_bytes())
        );
    }

    fn assert_two_results(encoded: &[u8], tail: &str) {
        assert_answer_run(encoded, &["call_1", "call_2"], &["a.go", "b.go"]);
        let value = json(encoded);
        let messages = value["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3, "{value}");
        let answer = &messages[2];
        assert!(
            answer["content"]
                .as_array()
                .expect("content")
                .iter()
                .any(|block| block["text"] == tail),
            "{answer}"
        );
    }

    fn assert_answer_run(encoded: &[u8], calls: &[&str], outputs: &[&str]) {
        let value = json(encoded);
        let messages = value["messages"].as_array().expect("messages");
        assert!(
            messages.iter().all(|message| {
                let role = message["role"].as_str().unwrap_or("");
                role != "system" && role != "developer"
            }),
            "{value}"
        );
        let assistant = messages
            .iter()
            .position(|message| {
                message["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"))
            })
            .expect("assistant");
        let mut found = Vec::new();
        for message in messages.iter().skip(assistant + 1) {
            if message["role"] != "user" {
                break;
            }
            let Some(blocks) = message["content"].as_array() else {
                continue;
            };
            for block in blocks {
                if block["type"] != "tool_result" {
                    continue;
                }
                found.push((
                    block["tool_use_id"].as_str().unwrap_or("").to_owned(),
                    block["content"].as_str().unwrap_or("").to_owned(),
                ));
            }
        }
        assert_eq!(found.len(), calls.len(), "{found:?} {value}");
        for (call, output) in calls.iter().zip(outputs.iter()) {
            assert!(
                found
                    .iter()
                    .any(|(id, content)| id == call && content == output),
                "{call} {found:?}"
            );
        }
        let ids: Vec<_> = found.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids.len(),
            ids.iter().collect::<std::collections::BTreeSet<_>>().len()
        );
        assert!(!String::from_utf8_lossy(encoded).contains(INTERRUPTED));
    }

    fn answer_message(value: &Value, assistant_ordinal: usize) -> &Value {
        let messages = value["messages"].as_array().expect("messages");
        let mut seen = 0;
        for (index, message) in messages.iter().enumerate() {
            let calls = message["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"));
            if !calls {
                continue;
            }
            if seen == assistant_ordinal {
                return &messages[index + 1];
            }
            seen += 1;
        }
        panic!("missing answer {value}");
    }
}
