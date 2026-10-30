//! Reasoning effort, independent of any one protocol's spelling.
//!
//! Codecs translate this enum with one table each. Levels a protocol does not
//! publish are folded to the nearest published level. There is no catalog yet,
//! so a level is not clamped to a model: an upstream that rejects it returns
//! that error.

use serde::{Deserialize, Serialize};

/// How much reasoning the caller asked for.
///
/// `None` on [`crate::GenerationParams::effort`] means the caller did not ask.
/// [`Effort::None`] means the caller asked for no reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// No reasoning.
    None,
    /// The smallest reasoning step.
    Minimal,
    /// A short reasoning pass.
    Low,
    /// A balanced reasoning pass.
    Medium,
    /// A deep reasoning pass.
    High,
    /// Deeper than [`Effort::High`].
    Xhigh,
    /// The deepest published level.
    Max,
}

impl Effort {
    /// Parses a level label.
    ///
    /// Matching is case-insensitive. `ultra` is accepted as [`Effort::Max`]:
    /// some clients spell the top level that way. It is not written back out.
    #[must_use]
    pub fn from_label(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" | "ultra" => Some(Self::Max),
            _ => None,
        }
    }

    /// Chat Completions `reasoning_effort` and Responses `reasoning.effort`.
    ///
    /// Those APIs publish `none`, `minimal`, `low`, `medium`, `high`,
    /// `xhigh`, and `max`.
    #[must_use]
    pub fn openai_label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Messages `output_config.effort`.
    ///
    /// That API publishes `low`, `medium`, `high`, `xhigh`, and `max`.
    /// [`Effort::Minimal`] is sent as `low`. [`Effort::None`] is not a level
    /// there; the Messages encoder writes `thinking.type = disabled` instead.
    #[must_use]
    pub fn messages_label(self) -> &'static str {
        match self {
            Self::None | Self::Minimal | Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Generative Language `thinkingConfig.thinkingLevel`.
    ///
    /// That API publishes `minimal`, `low`, `medium`, and `high`.
    /// [`Effort::Xhigh`] and [`Effort::Max`] are sent as `high`.
    /// [`Effort::None`] has no level; the encoder writes `thinkingBudget: 0`.
    #[must_use]
    pub fn generative_level(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High | Self::Xhigh | Self::Max => Some("high"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Effort;

    #[test]
    fn labels_round_trip_and_ultra_is_max() {
        for effort in [
            Effort::None,
            Effort::Minimal,
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ] {
            assert_eq!(Effort::from_label(effort.openai_label()), Some(effort));
        }
        assert_eq!(Effort::from_label("ULTRA"), Some(Effort::Max));
        assert_eq!(Effort::from_label("adaptive"), None);
        assert_eq!(Effort::Minimal.messages_label(), "low");
        assert_eq!(Effort::Max.generative_level(), Some("high"));
        assert_eq!(Effort::None.generative_level(), None);
    }
}
