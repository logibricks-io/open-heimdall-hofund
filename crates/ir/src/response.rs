//! A canonical non-streaming chat response.

use serde::{Deserialize, Serialize};

use crate::extensions::Extensions;
use crate::message::Message;
use crate::usage::Usage;

/// Why generation stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished the turn.
    EndTurn,
    /// The output token cap was hit.
    MaxTokens,
    /// A stop sequence was hit.
    StopSequence,
    /// The model is waiting on a tool call.
    ToolUse,
    /// A content filter stopped generation.
    ContentFilter,
    /// The model refused the request.
    Refusal,
    /// A protocol-specific reason this version does not name.
    ///
    /// The string is the protocol's own token, so a same-protocol round trip
    /// can write it back.
    Other(String),
}

/// Stop reason plus the stop sequence text, when the protocol reports one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    /// Why generation stopped.
    pub reason: StopReason,
    /// The stop sequence that matched, when the protocol reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<String>,
}

/// One completed assistant turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Response identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Model that produced the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The assistant message.
    pub message: Message,
    /// Why generation stopped. `None` when the protocol has not said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Stop>,
    /// Token counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl Response {
    /// Drops extension bags on the response, its usage, and its message.
    pub fn strip_extensions(&mut self) {
        self.extensions.clear();
        self.message.strip_extensions();
        if let Some(usage) = &mut self.usage {
            usage.extensions.clear();
        }
    }
}
