//! Chat messages.

use serde::{Deserialize, Serialize};

use crate::content::ContentPart;
use crate::extensions::Extensions;

/// Who produced a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Instructions that sit beside the conversation.
    System,
    /// A client or developer instruction role that is not the system role.
    ///
    /// Protocols that only have a system channel fold this role into system on
    /// encode. See the codec notes for that loss.
    Developer,
    /// The end user.
    User,
    /// The model.
    Assistant,
    /// A tool result. Some protocols instead nest the result on a user message.
    Tool,
}

/// One chat message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced the message.
    pub role: Role,
    /// Ordered content parts. Empty when the message has no body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ContentPart>,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl Message {
    /// A message with an empty body and an empty extension bag.
    #[must_use]
    pub fn new(role: Role) -> Self {
        Self {
            role,
            content: Vec::new(),
            extensions: Extensions::new(),
        }
    }

    /// Drops extension bags on the message and its parts.
    pub fn strip_extensions(&mut self) {
        self.extensions.clear();
        for part in &mut self.content {
            part.strip_extensions();
        }
    }
}
