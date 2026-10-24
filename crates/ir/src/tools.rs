//! Tool definitions and the model's tool-choice policy.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extensions::Extensions;

/// A tool the model may call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the arguments object.
    pub input_schema: Value,
    /// Fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// How the model may use tools.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolChoice {
    /// The selection policy.
    pub mode: ToolChoiceMode,
    /// Fields this version does not model, such as a parallel-call switch.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// Tool selection policy, without protocol-specific spellings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolChoiceMode {
    /// The model may call a tool or not.
    Auto,
    /// The model must not call a tool.
    None,
    /// The model must call at least one tool.
    Required,
    /// The model must call this tool.
    Named {
        /// Tool name.
        name: String,
    },
}

impl ToolChoice {
    /// Auto mode with an empty extension bag.
    #[must_use]
    pub fn auto() -> Self {
        Self {
            mode: ToolChoiceMode::Auto,
            extensions: Extensions::new(),
        }
    }

    /// Required mode with an empty extension bag.
    #[must_use]
    pub fn required() -> Self {
        Self {
            mode: ToolChoiceMode::Required,
            extensions: Extensions::new(),
        }
    }

    /// A named tool, with an empty extension bag.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            mode: ToolChoiceMode::Named { name: name.into() },
            extensions: Extensions::new(),
        }
    }

    /// None mode with an empty extension bag.
    #[must_use]
    pub fn none() -> Self {
        Self {
            mode: ToolChoiceMode::None,
            extensions: Extensions::new(),
        }
    }
}
