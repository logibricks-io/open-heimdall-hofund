//! Claude Code's billing line is system text, not a modeled field.
//!
//! A Messages request may start with `x-anthropic-billing-header:` and a list
//! of `key=value;` fields. That line is kept when the upstream speaks the same
//! protocol. Crossing to another protocol removes a system text part that is
//! only the line, and cuts the same prefix off the first remaining system text.
//! User text is left alone. Checked against `internal/gateway/billing_line.go`
//! and `withoutBillingHeader` on 2026-10-25.

use hofund_ir::{ContentPart, Request, Role};

pub(crate) fn strip_billing_header(request: &mut Request) {
    let mut saw_text = false;
    for message in &mut request.messages {
        if !matches!(message.role, Role::System | Role::Developer) {
            continue;
        }
        message.content.retain(|part| match part {
            ContentPart::Text(text) => !billing_line_only(&text.text),
            _ => true,
        });
        if saw_text {
            continue;
        }
        for part in &mut message.content {
            if let ContentPart::Text(text) = part {
                let cut = strip_billing_prefix(&text.text);
                if cut != text.text {
                    text.text = cut;
                }
                saw_text = true;
                break;
            }
        }
    }
    request.messages.retain(|message| {
        !(matches!(message.role, Role::System | Role::Developer)
            && message.content.is_empty()
            && message.extensions.is_empty())
    });
}

fn billing_line_only(text: &str) -> bool {
    text.starts_with("x-anthropic-billing-header:") && strip_billing_prefix(text).is_empty()
}

fn strip_billing_prefix(text: &str) -> String {
    let Some(rest) = text.strip_prefix("x-anthropic-billing-header:") else {
        return text.to_owned();
    };
    let rest = consume_fields(rest);
    rest.trim_start_matches(char::is_whitespace).to_owned()
}

/// Consumes `(\s*[A-Za-z_]+=[^;\s]*;)*` and leaves the unmatched tail.
fn consume_fields(mut rest: &str) -> &str {
    loop {
        let trimmed = rest.trim_start();
        let bytes = trimmed.as_bytes();
        if bytes.first().is_none_or(|byte| !is_key_byte(*byte)) {
            return rest;
        }
        let mut index = 1;
        while index < bytes.len() && is_key_byte(bytes[index]) {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            return rest;
        }
        index += 1;
        while index < bytes.len() && bytes[index] != b';' && !bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b';' {
            return rest;
        }
        rest = &trimmed[index + 1..];
    }
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}
