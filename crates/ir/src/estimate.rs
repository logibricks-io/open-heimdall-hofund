//! A character estimate of input size.
//!
//! This is not a tokenizer. It exists so a count endpoint can answer when no
//! upstream publishes a count. Four characters count as one token, rounded up.
//! A family-specific tokenizer waits for the model catalog.

use crate::content::ContentPart;
use crate::request::Request;

/// Estimates input tokens for `request`.
///
/// Images and unknown parts add nothing. The result is a ceiling, not a bill.
#[must_use]
pub fn estimate_input_tokens(request: &Request) -> u64 {
    let mut chars = 0usize;
    for message in &request.messages {
        for part in &message.content {
            chars += part_chars(part);
        }
    }
    for tool in &request.tools {
        chars += tool.name.chars().count();
        if let Some(description) = &tool.description {
            chars += description.chars().count();
        }
    }
    u64::try_from(chars.div_ceil(4)).unwrap_or(u64::MAX)
}

fn part_chars(part: &ContentPart) -> usize {
    match part {
        ContentPart::Text(part) => part.text.chars().count(),
        ContentPart::Thinking(part) => part.text.chars().count(),
        ContentPart::ToolCall(part) => part.name.chars().count(),
        ContentPart::ToolResult(part) => part.content.iter().map(part_chars).sum(),
        ContentPart::Image(_) | ContentPart::Unknown(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::estimate_input_tokens;
    use crate::content::ContentPart;
    use crate::message::{Message, Role};
    use crate::request::Request;

    #[test]
    fn four_characters_are_one_token() {
        let mut request = Request::new("demo");
        let mut message = Message::new(Role::User);
        message.content.push(ContentPart::text("abcd"));
        request.messages.push(message);
        assert_eq!(estimate_input_tokens(&request), 1);
        let mut extra = Message::new(Role::User);
        extra.content.push(ContentPart::text("e"));
        request.messages.push(extra);
        assert_eq!(estimate_input_tokens(&request), 2);
    }
}
