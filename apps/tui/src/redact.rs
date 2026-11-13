//! Removes credential material from text the terminal UI can show.

/// Replaces `secret` and common token shapes with `[redacted]`.
#[must_use]
pub fn redact(text: &str, secret: Option<&str>) -> String {
    let mut text = text.to_owned();
    if let Some(secret) = secret.filter(|value| value.len() >= 4) {
        text = text.replace(secret, "[redacted]");
    }
    redact_marked(&text)
}

fn redact_marked(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(skip) = marked_token(&text[index..]) {
            out.push_str("[redacted]");
            index += skip;
            continue;
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

fn marked_token(rest: &str) -> Option<usize> {
    const MARKS: &[&str] = &[
        "bearer ", "Bearer ", "BEARER ", "sk-", "sk_", "key-", "api_key=",
    ];
    for mark in MARKS {
        if rest.starts_with(mark) {
            let mut end = mark.len();
            let bytes = rest.as_bytes();
            while end < bytes.len() && is_token_byte(bytes[end]) {
                end += 1;
            }
            if end > mark.len() {
                return Some(end);
            }
        }
    }
    None
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'+' | b'=')
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn submitted_secret_and_token_shapes_are_removed() {
        let text = "rejected sk-live-secret and Bearer abcdefghijk for key sk-live-secret";
        let cleaned = redact(text, Some("sk-live-secret"));
        assert!(!cleaned.contains("sk-live-secret"));
        assert!(!cleaned.contains("abcdefghijk"));
        assert!(cleaned.contains("[redacted]"));
    }
}
