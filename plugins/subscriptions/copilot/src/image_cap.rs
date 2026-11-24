//! Image caps learned from a Copilot refusal.
//!
//! A Gemini model counts every image in the conversation. Past its cap it
//! answers 400 with
//! `too many images: maximum allowed for model <id> is <cap>, got <n>`.
//! The cap is remembered per account and model, from that
//! refusal. The model list's image limit is not used.
//!
//! Earlier copies of an image outside the latest user message are replaced
//! with one line of text, keeping the last copy. If the count is still over
//! the cap, the oldest images by their last position are replaced with a
//! different line, including every earlier copy, and that turn is sent once
//! more. Later turns to the same model are fitted before they are sent. A turn
//! already within the cap is left as it arrived. When the latest user message
//! alone is over the cap, nothing is replaced and the refusal is returned as
//! it came.

use std::collections::{HashMap, HashSet};

use hofund_plugin_api::{AccountId, ContentPart, Message, PluginError, Request, Role};
use hofund_plugin_sdk::{digest::sha256, json};

use crate::models::Book;

/// Text written where an image was left out. It is sent upstream, so it
/// names neither this gateway nor the vendor.
pub(crate) const OMITTED: &str =
    "[An earlier image was omitted here to keep this request within the model's image limit.]";

/// Text written where an earlier copy of an image was left out.
pub(crate) const SHOWN_LATER: &str =
    "[This image is omitted here; the same image appears again later in this conversation.]";

const MARKER: &str = "too many images: maximum allowed for model ";

/// How deep tool-result content is walked. A hostile nest stops here.
const MAX_DEPTH: usize = 6;

struct Spot {
    message: usize,
    path: Vec<usize>,
    protected: bool,
    digest: [u8; 32],
}

struct Omission {
    spot: Spot,
    text: &'static str,
}

enum Plan {
    Under,
    Cannot,
    Omit(Vec<Omission>),
}

/// The cap named by a 400 "too many images" refusal, when the words are present.
pub(crate) fn stated_cap(error: &PluginError) -> Option<usize> {
    if error.status() != Some(400) {
        return None;
    }
    let lower = error.message().to_ascii_lowercase();
    let rest = lower.split_once(MARKER)?.1;
    let model_end = rest.find(" is ")?;
    let model = &rest[..model_end];
    if model.is_empty() || model.chars().any(char::is_whitespace) {
        return None;
    }
    let digits: String = rest[model_end + 4..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let cap = digits.parse::<usize>().ok()?;
    (cap > 0).then_some(cap)
}

/// Remembers `cap` for `account` and `model`, keeping the smaller one already known.
pub(crate) fn learn(book: &Book, account: &AccountId, model: &str, cap: usize) -> usize {
    if cap == 0 {
        return 0;
    }
    let mut caps = book
        .image_caps
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let models = caps.entry(account.clone()).or_default();
    if let Some(&old) = models.get(model)
        && old > 0
        && old <= cap
    {
        return old;
    }
    models.insert(model.to_owned(), cap);
    cap
}

fn known(book: &Book, account: &AccountId, model: &str) -> Option<usize> {
    let caps = book
        .image_caps
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    caps.get(account)
        .and_then(|models| models.get(model).copied())
        .filter(|cap| *cap > 0)
}

/// `request` fitted to the cap already learned for its model, when one is known.
pub(crate) fn within(book: &Book, account: &AccountId, request: Request) -> Request {
    match known(book, account, &request.model) {
        Some(cap) => apply(request, cap),
        None => request,
    }
}

/// Whether fitting `request` to `cap` would replace at least one image.
pub(crate) fn omits_any(request: &Request, cap: usize) -> bool {
    matches!(plan(request, cap), Plan::Omit(_))
}

fn apply(request: Request, cap: usize) -> Request {
    match plan(&request, cap) {
        Plan::Omit(spots) => rewrite(request, &spots),
        Plan::Under | Plan::Cannot => request,
    }
}

fn plan(request: &Request, cap: usize) -> Plan {
    let found = locate(&request.messages);
    if found.len() <= cap {
        return Plan::Under;
    }

    let mut copies_of: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
    let mut in_latest = HashSet::new();
    let mut latest = 0;
    for (index, spot) in found.iter().enumerate() {
        if spot.protected {
            latest += 1;
            in_latest.insert(spot.digest);
        } else {
            copies_of.entry(spot.digest).or_default().push(index);
        }
    }

    let mut history = Vec::new();
    for (index, spot) in found.iter().enumerate() {
        if spot.protected || in_latest.contains(&spot.digest) {
            continue;
        }
        let copies = copies_of
            .get(&spot.digest)
            .expect("every history image has a copy group");
        if copies.last() == Some(&index) {
            history.push(spot.digest);
        }
    }

    let over = (latest + history.len()).saturating_sub(cap);
    if over > history.len() {
        return Plan::Cannot;
    }
    let order: HashMap<[u8; 32], usize> = history
        .into_iter()
        .enumerate()
        .map(|(index, digest)| (digest, index))
        .collect();

    let mut omissions = Vec::new();
    for (index, spot) in found.into_iter().enumerate() {
        if spot.protected {
            continue;
        }
        let text = if in_latest.contains(&spot.digest) {
            Some(SHOWN_LATER)
        } else {
            let copies = copies_of
                .get(&spot.digest)
                .expect("every history image has a copy group");
            let position = order
                .get(&spot.digest)
                .expect("every grouped image has a history position");
            if *position < over {
                Some(OMITTED)
            } else if copies.last() != Some(&index) {
                Some(SHOWN_LATER)
            } else {
                None
            }
        };
        if let Some(text) = text {
            omissions.push(Omission { spot, text });
        }
    }
    Plan::Omit(omissions)
}

fn locate(messages: &[Message]) -> Vec<Spot> {
    let protected = latest_own_user(messages);
    let mut found = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let mut path = Vec::new();
        walk(
            &message.content,
            index,
            protected == Some(index),
            &mut path,
            0,
            &mut found,
        );
    }
    found
}

/// The latest user message that is the user's own words, not only tool results.
fn latest_own_user(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            (message.role == Role::User && owns_words(message)).then_some(index)
        })
}

fn owns_words(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|part| !matches!(part, ContentPart::ToolResult(_)))
}

fn walk(
    parts: &[ContentPart],
    message: usize,
    protected: bool,
    path: &mut Vec<usize>,
    depth: usize,
    found: &mut Vec<Spot>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    for (index, part) in parts.iter().enumerate() {
        path.push(index);
        match part {
            ContentPart::Image(_) => {
                let encoded = json::to_vec(part).expect("an image content part serializes");
                found.push(Spot {
                    message,
                    path: path.clone(),
                    protected,
                    digest: sha256(&encoded),
                });
            }
            ContentPart::ToolResult(result) => {
                walk(&result.content, message, protected, path, depth + 1, found);
            }
            _ => {}
        }
        path.pop();
    }
}

fn rewrite(mut request: Request, omissions: &[Omission]) -> Request {
    for omission in omissions {
        let Some(message) = request.messages.get_mut(omission.spot.message) else {
            continue;
        };
        replace(&mut message.content, &omission.spot.path, omission.text);
    }
    request
}

fn replace(parts: &mut [ContentPart], path: &[usize], text: &'static str) {
    let Some((&index, rest)) = path.split_first() else {
        return;
    };
    let Some(part) = parts.get_mut(index) else {
        return;
    };
    if rest.is_empty() {
        *part = ContentPart::text(text);
        return;
    }
    if let ContentPart::ToolResult(result) = part {
        replace(&mut result.content, rest, text);
    }
}
