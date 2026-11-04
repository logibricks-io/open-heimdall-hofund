//! Conversation identity and turn boundaries shared by routing.

use crate::opening::hashed_id;
use hofund_ir::{ContentPart, Request, Role};

/// Client headers that name a conversation, in precedence order.
pub const SESSION_HEADERS: &[&str] = &[
    "x-opencode-session",
    "x-session-affinity",
    "x-session-id",
    "session_id",
    "session-id",
    "x-claude-code-session-id",
    "x-conversation-id",
    "x-deepseek-harness-session-id",
];

/// Header that distinguishes a subagent from the lead in the same session.
pub const SUBAGENT_HEADER: &str = "x-claude-code-agent-id";

/// Returns the scoped conversation key for affinity state.
///
/// A client session id wins. Without one, the first user item is hashed;
/// without any usable item, the whole body is hashed. A subagent receives a
/// separate key even though its client sends the lead's session id.
#[must_use]
pub fn conversation_key(scope: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
    let session = SESSION_HEADERS.iter().find_map(|name| {
        header(headers, name)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    });
    let id = session.map_or_else(|| hashed_id(body), str::to_owned);
    let mut key = format!("{scope}|{id}");
    if let Some(agent) = header(headers, SUBAGENT_HEADER)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        key.push_str("|agent:");
        key.push_str(truncate(agent, 128));
    }
    key
}

fn header<'a>(headers: &[(&str, &'a str)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| *value)
}

fn truncate(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// The user-turn count and whether the latest request continues a turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    /// Number of user turns in the supplied conversation.
    pub number: u64,
    /// Whether tool results or agent-only notes continue the last turn.
    pub within: bool,
}

/// Determines the turn boundary from the decoded conversation.
#[must_use]
pub fn turn_in(request: &Request) -> Turn {
    let mut turn = Turn::default();
    let mut answered = false;
    for message in &request.messages {
        if message.role == Role::Assistant {
            answered = true;
        }
        if message.role == Role::Tool {
            turn.within = true;
            answered = false;
            continue;
        }
        if message.role != Role::User {
            continue;
        }
        let mut said = false;
        let mut result = false;
        for part in &message.content {
            match part {
                ContentPart::Text(text) => said |= !text.text.is_empty(),
                ContentPart::Image(_) => said = true,
                ContentPart::Unknown(part) if is_file(&part.kind) => said = true,
                ContentPart::ToolResult(_) => result = true,
                _ => {}
            }
        }
        if result {
            turn.within = true;
        } else if said {
            turn.number = turn.number.saturating_add(1);
            turn.within = false;
        } else if answered {
            turn.within = turn.number > 0;
        }
        answered = false;
    }
    turn
}

fn is_file(kind: &str) -> bool {
    matches!(kind, "file" | "input_file" | "document")
}

#[cfg(test)]
mod tests {
    use hofund_ir::{ContentPart, Message, Request, Role, TextPart, ToolResultPart};

    use super::{SUBAGENT_HEADER, Turn, conversation_key, turn_in};

    fn message(role: Role, content: Vec<ContentPart>) -> Message {
        let mut message = Message::new(role);
        message.content = content;
        message
    }

    #[test]
    fn c1b_conversation_keys_use_sessions_first_user_and_subagent() {
        let first = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
        let later = br#"{"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"},{"role":"user","content":"again"}]}"#;
        let hashed = conversation_key("demo", &[], first);
        assert!(hashed.starts_with("demo|hofund-"));
        assert_eq!(hashed.len(), "demo|hofund-".len() + 24);
        assert_eq!(hashed, conversation_key("demo", &[], later));
        assert_ne!(hashed, conversation_key("other", &[], first));

        let headers = [("x-session-id", " session-1 ")];
        assert_eq!(conversation_key("demo", &headers, later), "demo|session-1");
        let agent = format!(" {} ", "a".repeat(140));
        let headers = [
            ("x-session-id", "session-1"),
            (SUBAGENT_HEADER, agent.as_str()),
        ];
        assert_eq!(
            conversation_key("demo", &headers, later),
            format!("demo|session-1|agent:{}", "a".repeat(128))
        );
        let responses = br#"{"input":[{"role":"user","content":"hello"}]}"#;
        assert_eq!(
            conversation_key("demo", &[], responses),
            conversation_key("demo", &[], br#"{"input":[{"role":"user","content":"hello"},{"role":"assistant","content":"hi"}],"other":true}"#)
        );
        let gemini = br#"{"contents":[{"parts":[{"text":"hello"}]}]}"#;
        assert_eq!(
            conversation_key("demo", &[], gemini),
            conversation_key("demo", &[], br#"{"contents":[{"parts":[{"text":"hello"}]},{"role":"model","parts":[{"text":"hi"}]}]}"#)
        );
        assert!(conversation_key("demo", &[], b"not json").starts_with("demo|hofund-"));
    }

    #[test]
    fn c1c_turn_detection_stays_within_tool_results_until_user_speaks() {
        let mut request = Request::new("demo");
        request.messages = vec![
            message(
                Role::User,
                vec![ContentPart::Text(TextPart {
                    text: "start".to_owned(),
                    extensions: Default::default(),
                })],
            ),
            Message::new(Role::Assistant),
            Message::new(Role::Tool),
        ];
        assert_eq!(
            turn_in(&request),
            Turn {
                number: 1,
                within: true
            }
        );
        request.messages.push(message(
            Role::User,
            vec![ContentPart::ToolResult(ToolResultPart {
                id: "call-1".to_owned(),
                is_error: false,
                content: Vec::new(),
                extensions: Default::default(),
            })],
        ));
        assert_eq!(
            turn_in(&request),
            Turn {
                number: 1,
                within: true
            }
        );
        request.messages.push(message(
            Role::User,
            vec![ContentPart::Text(TextPart {
                text: "next".to_owned(),
                extensions: Default::default(),
            })],
        ));
        assert_eq!(
            turn_in(&request),
            Turn {
                number: 2,
                within: false
            }
        );
    }
}
