//! Stable thread and prompt ids derived like the reference plugin.

use std::sync::OnceLock;

use hofund_plugin_api::{ContentPart, Message, Request, Role};
use hofund_plugin_sdk::digest::sha256;

static SALT: OnceLock<String> = OnceLock::new();

fn salt() -> &'static str {
    SALT.get_or_init(|| {
        hofund_plugin_sdk::uuid::v4().unwrap_or_else(|_| "zed-thread-salt".to_owned())
    })
}

fn uuid_of(input: &str) -> String {
    let digest = sha256(format!("{}|{input}", salt()).as_bytes());
    let mut bytes: [u8; 16] = digest[..16].try_into().expect("16 bytes");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn user_prompts(request: &Request) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(message_text)
        .filter(|t| !t.trim().is_empty())
        .collect()
}

fn message_text(message: &Message) -> Option<String> {
    let mut parts = String::new();
    for part in &message.content {
        if let ContentPart::Text(part) = part {
            parts.push_str(&part.text);
        }
    }
    if parts.is_empty() { None } else { Some(parts) }
}

/// Thread and prompt ids for the Zed completion envelope.
#[must_use]
pub(crate) fn thread_fields(request: &Request, user_id: &str) -> (Option<String>, Option<String>) {
    let session = request
        .params
        .extensions
        .get("prompt_cache_key")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(128).collect::<String>());
    let prompts = user_prompts(request);
    let key = if let Some(session) = session {
        format!("session|{session}")
    } else if let Some(first) = prompts.first() {
        format!("first|{first}")
    } else {
        return (None, None);
    };
    let thread = uuid_of(&format!("zed-thread|{user_id}|{key}"));
    let prompt = uuid_of(&format!("zed-prompt|{thread}|{}", prompts.len()));
    (Some(thread), Some(prompt))
}
