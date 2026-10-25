//! Canonical in-memory types for an LLM request, a response, and a stream event.
//!
//! This crate is pure data: it performs no I/O and names no vendor. Every protocol
//! codec translates into and out of these types so the rest of hofund speaks one dialect.
//!
//! Unknown protocol fields live in [`Extensions`]. Codecs copy that bag back onto
//! the protocol object on encode, so a same-protocol passthrough keeps fields this
//! version does not model yet. See the codec crate for which canonical fields
//! survive a translation between protocols.
//!
//! [`assemble`] folds a [`StreamEvent`] sequence into one [`Response`].

mod assemble;
mod content;
mod effort;
mod estimate;
mod extensions;
mod message;
mod params;
mod request;
mod response;
mod stream;
mod tools;
mod usage;

pub use assemble::assemble;
pub use content::{
    ContentPart, ImageDetail, ImagePart, ImageSource, TextPart, ThinkingPart, ToolArguments,
    ToolCallPart, ToolResultPart, UnknownPart,
};
pub use effort::Effort;
pub use estimate::estimate_input_tokens;
pub use extensions::Extensions;
pub use message::{Message, Role};
pub use params::GenerationParams;
pub use request::Request;
pub use response::{Response, Stop, StopReason};
pub use stream::{
    BlockStart, ContentBlockStart, ContentBlockStop, MessageDelta, MessageStart, MessageStop, Ping,
    SignatureDelta, StreamEvent, TextDelta, ThinkingDelta, ToolArgumentsDelta, UnknownEvent,
};
pub use tools::{Tool, ToolChoice, ToolChoiceMode};
pub use usage::Usage;

#[cfg(test)]
mod tests {
    use super::{
        BlockStart, ContentBlockStart, Extensions, MessageStart, Role, StreamEvent, TextDelta,
        ThinkingDelta, ToolArguments, ToolArgumentsDelta, assemble,
    };
    use serde_json::json;

    #[test]
    fn extension_bag_round_trips_beside_known_fields() {
        let mut message = super::Message::new(Role::User);
        message.content.push(super::ContentPart::text("hello"));
        message
            .extensions
            .insert("cache_control", json!({"type": "ephemeral"}));
        let bytes = serde_json::to_vec(&message).expect("serialize");
        let decoded: super::Message = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, message);
        assert_eq!(
            decoded.extensions.get("cache_control"),
            Some(&json!({"type": "ephemeral"}))
        );
    }

    #[test]
    fn assemble_concatenates_text_thinking_and_tool_arguments() {
        let events = vec![
            StreamEvent::MessageStart(MessageStart {
                id: Some("msg_1".to_owned()),
                model: Some("demo".to_owned()),
                usage: None,
                extensions: Extensions::new(),
            }),
            StreamEvent::ContentBlockStart(ContentBlockStart {
                index: 0,
                block: BlockStart::Thinking {
                    redacted: false,
                    data: None,
                },
                extensions: Extensions::new(),
            }),
            StreamEvent::ThinkingDelta(ThinkingDelta {
                index: 0,
                text: "Let ".to_owned(),
                extensions: Extensions::new(),
            }),
            StreamEvent::ThinkingDelta(ThinkingDelta {
                index: 0,
                text: "me".to_owned(),
                extensions: Extensions::new(),
            }),
            StreamEvent::ContentBlockStart(ContentBlockStart {
                index: 1,
                block: BlockStart::Text,
                extensions: Extensions::new(),
            }),
            StreamEvent::TextDelta(TextDelta {
                index: 1,
                text: "Hi".to_owned(),
                extensions: Extensions::new(),
            }),
            StreamEvent::ContentBlockStart(ContentBlockStart {
                index: 2,
                block: BlockStart::ToolCall {
                    id: "call_1".to_owned(),
                    name: "get_weather".to_owned(),
                },
                extensions: Extensions::new(),
            }),
            StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: 2,
                partial_json: "{\"city\":".to_owned(),
                extensions: Extensions::new(),
            }),
            StreamEvent::ToolArgumentsDelta(ToolArgumentsDelta {
                index: 2,
                partial_json: "\"Paris\"}".to_owned(),
                extensions: Extensions::new(),
            }),
        ];
        let response = assemble(&events);
        assert_eq!(response.id.as_deref(), Some("msg_1"));
        assert_eq!(response.message.content.len(), 3);
        match &response.message.content[2] {
            super::ContentPart::ToolCall(call) => {
                assert_eq!(call.name, "get_weather");
                assert_eq!(
                    call.arguments,
                    ToolArguments::Json {
                        value: json!({"city": "Paris"})
                    }
                );
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }
}
