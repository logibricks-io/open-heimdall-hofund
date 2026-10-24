//! Sampling and length parameters shared by chat protocols.

use serde::{Deserialize, Serialize};

use crate::extensions::Extensions;

/// Generation controls.
///
/// Absent fields were not set. Codecs omit them on encode instead of writing null.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Upper bound on tokens the model may produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling cutoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Hard cutoff on the number of candidate tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u64>,
    /// Sequences that end generation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Deterministic sampling seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Penalty for tokens that have already appeared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Penalty that grows with how often a token has appeared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Parameter fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}
