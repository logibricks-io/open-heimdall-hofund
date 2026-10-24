//! Token counters.

use serde::{Deserialize, Serialize};

use crate::extensions::Extensions;

/// Token counters for one request or one point in a stream.
///
/// Every counter is optional because a stream event often carries only the
/// counters it knows. [`Usage::merge_from`] fills gaps and lets a later event
/// overwrite a counter it sets again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens read from the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Tokens written by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Sum reported by the protocol, when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Prompt tokens served from a cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written into a cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Output tokens spent on reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Counter fields this version does not model.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl Usage {
    /// Fills unset counters from `patch` and overwrites counters `patch` sets.
    pub fn merge_from(&mut self, patch: &Usage) {
        merge_counter(&mut self.input_tokens, patch.input_tokens);
        merge_counter(&mut self.output_tokens, patch.output_tokens);
        merge_counter(&mut self.total_tokens, patch.total_tokens);
        merge_counter(&mut self.cache_read_tokens, patch.cache_read_tokens);
        merge_counter(&mut self.cache_write_tokens, patch.cache_write_tokens);
        merge_counter(&mut self.reasoning_tokens, patch.reasoning_tokens);
        self.extensions.absorb(&patch.extensions);
    }
}

fn merge_counter(slot: &mut Option<u64>, patch: Option<u64>) {
    if let Some(value) = patch {
        *slot = Some(value);
    }
}
