//! Typed events for one streamed assistant turn.

use serde::{Deserialize, Serialize};

use crate::extensions::Extensions;
use crate::response::Stop;
use crate::usage::Usage;

/// One event in a streamed assistant turn.
///
/// Indexes identify a content block inside the turn. A codec chooses the index
/// space and documents it. Deltas for the same index append in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// The turn started.
    MessageStart(MessageStart),
    /// A content block opened.
    ContentBlockStart(ContentBlockStart),
    /// Text appended to a block.
    TextDelta(TextDelta),
    /// Reasoning text appended to a block.
    ThinkingDelta(ThinkingDelta),
    /// A reasoning signature arrived for a block.
    SignatureDelta(SignatureDelta),
    /// A fragment of tool-call arguments arrived.
    ToolArgumentsDelta(ToolArgumentsDelta),
    /// A content block closed.
    ContentBlockStop(ContentBlockStop),
    /// Stop reason or usage changed.
    MessageDelta(MessageDelta),
    /// The turn finished.
    MessageStop(MessageStop),
    /// A keep-alive with no content.
    Ping(Ping),
    /// An event name this version does not model.
    Unknown(UnknownEvent),
}

impl StreamEvent {
    /// Sets the model on a start event. Other events stay as they are.
    #[must_use]
    pub fn with_model(self, model: &str) -> Self {
        match self {
            Self::MessageStart(mut event) => {
                event.model = Some(model.to_owned());
                Self::MessageStart(event)
            }
            other => other,
        }
    }
}

/// The turn started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageStart {
    /// Response identifier, if known yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Model name, if known yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Counters known at the start. Often only input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// A content block opened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockStart {
    /// Block index.
    pub index: u32,
    /// What kind of block opened.
    pub block: BlockStart,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// The opening shape of a content block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BlockStart {
    /// A text block.
    Text,
    /// A reasoning block.
    Thinking {
        /// Whether the block is an opaque redacted blob.
        #[serde(default, skip_serializing_if = "is_false")]
        redacted: bool,
        /// Redacted payload, when `redacted` is set and the blob arrived with
        /// the opening event.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<String>,
    },
    /// A tool call.
    ToolCall {
        /// Call identifier.
        id: String,
        /// Tool name.
        name: String,
    },
    /// A block type this version does not model.
    Unknown {
        /// Original type name.
        kind: String,
    },
}

/// Text appended to a block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextDelta {
    /// Block index.
    pub index: u32,
    /// Text fragment.
    pub text: String,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Reasoning text appended to a block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThinkingDelta {
    /// Block index.
    pub index: u32,
    /// Reasoning fragment.
    pub text: String,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// A reasoning signature arrived.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignatureDelta {
    /// Block index.
    pub index: u32,
    /// Signature text.
    pub signature: String,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// A fragment of tool-call arguments.
///
/// Fragments are concatenated in order. They are not parsed until the block
/// is assembled, so a fragment may be split in the middle of a JSON token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolArgumentsDelta {
    /// Block index.
    pub index: u32,
    /// Argument text fragment, not a complete JSON value.
    pub partial_json: String,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// A content block closed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContentBlockStop {
    /// Block index.
    pub index: u32,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Stop reason or usage changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageDelta {
    /// Stop information, when this event carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,
    /// Counters carried by this event. Unset counters are left unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// The turn finished.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageStop {
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// A keep-alive.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Ping {
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// An event this version does not model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnknownEvent {
    /// Event name.
    pub name: String,
    /// Raw JSON payload.
    pub payload: serde_json::Value,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl StreamEvent {
    /// Borrows the extension bag, when this event has one.
    #[must_use]
    pub fn extensions(&self) -> Option<&Extensions> {
        match self {
            Self::MessageStart(event) => Some(&event.extensions),
            Self::ContentBlockStart(event) => Some(&event.extensions),
            Self::TextDelta(event) => Some(&event.extensions),
            Self::ThinkingDelta(event) => Some(&event.extensions),
            Self::SignatureDelta(event) => Some(&event.extensions),
            Self::ToolArgumentsDelta(event) => Some(&event.extensions),
            Self::ContentBlockStop(event) => Some(&event.extensions),
            Self::MessageDelta(event) => Some(&event.extensions),
            Self::MessageStop(event) => Some(&event.extensions),
            Self::Ping(event) => Some(&event.extensions),
            Self::Unknown(_) => None,
        }
    }

    /// Drops the extension bag on this event.
    pub fn strip_extensions(&mut self) {
        match self {
            Self::MessageStart(event) => {
                event.extensions.clear();
                if let Some(usage) = &mut event.usage {
                    usage.extensions.clear();
                }
            }
            Self::ContentBlockStart(event) => event.extensions.clear(),
            Self::TextDelta(event) => event.extensions.clear(),
            Self::ThinkingDelta(event) => event.extensions.clear(),
            Self::SignatureDelta(event) => event.extensions.clear(),
            Self::ToolArgumentsDelta(event) => event.extensions.clear(),
            Self::ContentBlockStop(event) => event.extensions.clear(),
            Self::MessageDelta(event) => {
                event.extensions.clear();
                if let Some(usage) = &mut event.usage {
                    usage.extensions.clear();
                }
            }
            Self::MessageStop(event) => event.extensions.clear(),
            Self::Ping(event) => event.extensions.clear(),
            Self::Unknown(event) => {
                event.payload = serde_json::Value::Null;
            }
        }
    }
}
