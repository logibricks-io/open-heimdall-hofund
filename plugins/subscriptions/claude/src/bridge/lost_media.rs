//! Image-only conversation rewrites (`lostMedia`, #1382).
//!
//! A client may send the conversation back with earlier images taken out
//! (or replaced with a line of text). The run that already holds the
//! images should still be treated as following that history.

use hofund_plugin_sdk::digest::sha256;

use hofund_plugin_api::{ContentPart, Message, Role};

/// Whether `now` is `was` with only images (or files) taken out, plus
/// optional trailing messages.
#[must_use]
pub(crate) fn lost_media(was: &[Message], now: &[Message]) -> bool {
    lost_runs(&heard_runs(was), &heard_runs(now))
}

fn lost_runs(was: &[HeardRun], now: &[HeardRun]) -> bool {
    if now.len() < was.len() {
        return false;
    }
    let mut lost = 0;
    for (i, w) in was.iter().enumerate() {
        if now[i].role != w.role {
            return false;
        }
        let Some(n) = lost_from_run(&w.parts, &now[i].parts) else {
            return false;
        };
        lost += n;
    }
    lost > 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    ToolCall,
    ToolResult,
    Image,
}

struct WordPart {
    kind: Kind,
    words: String,
    media: usize,
}

struct HeardPart {
    kind: Kind,
    n: usize,
    sum: [u8; 32],
    media: usize,
    words: String,
}

struct HeardRun {
    role: String,
    parts: Vec<HeardPart>,
}

fn role_label(role: Role) -> String {
    match role {
        Role::Assistant => "assistant".into(),
        Role::System => "system".into(),
        Role::Developer => "developer".into(),
        Role::User | Role::Tool => "user".into(),
    }
}

fn message_parts(msgs: &[Message]) -> Vec<Vec<WordPart>> {
    let mut out: Vec<Vec<WordPart>> = (0..msgs.len()).map(|_| Vec::new()).collect();
    let mut n = 0_usize;
    let mut calls = std::collections::HashMap::<String, usize>::new();
    for (i, message) in msgs.iter().enumerate() {
        for part in &message.content {
            let (kind, s, media) = match part {
                ContentPart::Text(text) => (Kind::Text, text.text.clone(), 0),
                ContentPart::ToolCall(call) => {
                    n += 1;
                    calls.insert(call.id.clone(), n);
                    (Kind::ToolCall, format!("\x01call {} #{n}", call.name), 0)
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
                    let media = result
                        .content
                        .iter()
                        .filter(|part| matches!(part, ContentPart::Image(_)))
                        .count();
                    (Kind::ToolResult, format!("\x01result #{k} {text}"), media)
                }
                ContentPart::Image(image) => {
                    let s = match &image.source {
                        hofund_plugin_api::ImageSource::Url { url } => {
                            format!("\x01image 0 {url}")
                        }
                        hofund_plugin_api::ImageSource::Base64 { data, .. } => {
                            format!("\x01image {} ", data.len())
                        }
                    };
                    (Kind::Image, s, 0)
                }
                ContentPart::Thinking(thinking) => (Kind::Text, thinking.text.clone(), 0),
                ContentPart::Unknown(_) => continue,
            };
            let words = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if !words.is_empty() {
                out[i].push(WordPart { kind, words, media });
            }
        }
    }
    out
}

fn heard_runs(msgs: &[Message]) -> Vec<HeardRun> {
    let mut out = Vec::new();
    for (i, parts) in message_parts(msgs).into_iter().enumerate() {
        if parts.is_empty() {
            continue;
        }
        let role = role_label(msgs[i].role);
        if out.last().is_none_or(|run: &HeardRun| run.role != role) {
            out.push(HeardRun {
                role,
                parts: Vec::new(),
            });
        }
        let run = out.last_mut().expect("run just pushed");
        for p in parts {
            let hash = sha256(p.words.as_bytes());
            run.parts.push(HeardPart {
                kind: p.kind,
                n: p.words.len(),
                sum: hash,
                media: p.media,
                words: p.words,
            });
        }
    }
    out
}

fn is_media(kind: Kind) -> bool {
    kind == Kind::Image
}

fn same(was: &HeardPart, now: &HeardPart) -> bool {
    now.kind == was.kind && was.n == now.words.len() && was.sum == sha256(now.words.as_bytes())
}

fn grown(was: &HeardPart, words: &str) -> bool {
    words.len() > was.n && sha256(&words.as_bytes()[..was.n]) == was.sum
}

fn media_after(was: &[HeardPart], i: usize) -> bool {
    let mut k = i + 1;
    while k < was.len() && was[k].kind == Kind::ToolResult {
        k += 1;
    }
    if k < was.len() && was[k].kind == Kind::Text {
        k += 1;
    }
    k < was.len() && is_media(was[k].kind)
}

fn lost_from_run(was: &[HeardPart], now: &[HeardPart]) -> Option<usize> {
    let mut lost = 0;
    let mut j = 0;
    let mut grew = false;
    let mut i = 0;
    while i < was.len() {
        let w = &was[i];
        if j < now.len() && same(w, &now[j]) {
            if w.kind != Kind::ToolResult {
                grew = false;
            }
            j += 1;
        } else if is_media(w.kind) {
            lost += 1;
            let mut next = i + 1;
            while next < was.len() && is_media(was[next].kind) {
                next += 1;
            }
            while j < now.len()
                && now[j].kind == Kind::Text
                && (next == was.len() || !same(&was[next], &now[j]))
            {
                j += 1;
            }
        } else if w.kind == Kind::ToolResult
            && j < now.len()
            && now[j].kind == Kind::ToolResult
            && w.media > now[j].media
            && grown(w, &now[j].words)
        {
            lost += w.media - now[j].media;
            grew = true;
            j += 1;
        } else if w.kind == Kind::ToolResult
            && j < now.len()
            && now[j].kind == Kind::ToolResult
            && w.media == now[j].media
            && media_after(was, i)
            && grown(w, &now[j].words)
        {
            grew = true;
            j += 1;
        } else if w.kind == Kind::Text
            && grew
            && i > 0
            && was[i - 1].kind == Kind::ToolResult
            && i + 1 < was.len()
            && is_media(was[i + 1].kind)
        {
            // the line before the results' images, gone with them
        } else {
            return None;
        }
        i += 1;
    }
    (j == now.len()).then_some(lost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_api::{ImagePart, ImageSource};

    fn user_text(text: &str) -> Message {
        let mut message = Message::new(Role::User);
        message.content = vec![ContentPart::text(text)];
        message
    }

    fn user_with_image(text: &str, url: &str) -> Message {
        let mut message = Message::new(Role::User);
        message.content = vec![
            ContentPart::text(text),
            ContentPart::Image(ImagePart {
                source: ImageSource::Url { url: url.into() },
                detail: None,
                extensions: Default::default(),
            }),
        ];
        message
    }

    fn assistant(text: &str) -> Message {
        let mut message = Message::new(Role::Assistant);
        message.content = vec![ContentPart::text(text)];
        message
    }

    #[test]
    fn dropping_only_an_earlier_image_is_lost_media() {
        let was = vec![
            user_with_image("see this", "https://ex/a.png"),
            assistant("nice"),
        ];
        let now = vec![user_text("see this"), assistant("nice"), user_text("again")];
        assert!(lost_media(&was, &now));
    }

    #[test]
    fn a_changed_text_is_not_lost_media() {
        let was = vec![user_with_image("see this", "https://ex/a.png")];
        let now = vec![user_text("see that")];
        assert!(!lost_media(&was, &now));
    }

    #[test]
    fn identical_history_is_not_lost_media() {
        let msgs = vec![user_text("hi"), assistant("hello")];
        assert!(!lost_media(&msgs, &msgs));
    }
}
