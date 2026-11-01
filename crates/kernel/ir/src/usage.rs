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
    /// Prompt tokens written into a cache (total when the protocol reports one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Prompt cache writes billed at the five-minute TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_tokens: Option<u64>,
    /// Prompt cache writes billed at the one-hour TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_tokens: Option<u64>,
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
        merge_counter(&mut self.cache_write_5m_tokens, patch.cache_write_5m_tokens);
        merge_counter(&mut self.cache_write_1h_tokens, patch.cache_write_1h_tokens);
        merge_counter(&mut self.reasoning_tokens, patch.reasoning_tokens);
        self.extensions.absorb(&patch.extensions);
    }

    /// Merges `patch` into `slot`. A missing patch leaves `slot` unchanged.
    pub fn merge_patch(slot: &mut Option<Self>, patch: Option<&Self>) {
        let Some(patch) = patch else {
            return;
        };
        match slot {
            Some(current) => current.merge_from(patch),
            None => *slot = Some(patch.clone()),
        }
    }

    /// Input, output, and total, with a missing total filled by the sum.
    ///
    /// A missing input or output is zero so a client that requires the field
    /// still receives a number.
    #[must_use]
    pub fn filled_counts(&self) -> (u64, u64, u64) {
        let input = self.input_tokens.unwrap_or(0);
        let output = self.output_tokens.unwrap_or(0);
        let total = self.total_tokens.unwrap_or(input.saturating_add(output));
        (input, output, total)
    }
}

fn merge_counter(slot: &mut Option<u64>, patch: Option<u64>) {
    if let Some(value) = patch {
        *slot = Some(value);
    }
}

/// Fills cache-write totals and TTL splits the way the reference ledger does.
///
/// When the protocol sends `cache_creation` counts, five-minute and one-hour
/// tokens are taken from there. When only a total is given, it counts as
/// five-minute write and one-hour is zero.
#[must_use]
pub fn normalize_cache_write(
    reported_total: Option<u64>,
    five_minute: Option<u64>,
    one_hour: Option<u64>,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    if five_minute.is_some() || one_hour.is_some() {
        let five = five_minute.unwrap_or(0);
        let hour = one_hour.unwrap_or(0);
        let total = reported_total
            .map(|total| total.max(five.saturating_add(hour)))
            .or_else(|| Some(five.saturating_add(hour)))
            .filter(|total| *total > 0 || five > 0 || hour > 0);
        let five = (five > 0)
            .then_some(five)
            .or_else(|| total.map(|total| total.saturating_sub(hour)));
        let hour = (hour > 0).then_some(hour);
        return (total, five, hour);
    }
    let total = reported_total.filter(|total| *total > 0);
    (total, total, Some(0))
}

#[cfg(test)]
mod tests {
    use super::Usage;

    #[test]
    fn merge_patch_keeps_counters_the_later_event_omits() {
        let mut slot = None;
        Usage::merge_patch(
            &mut slot,
            Some(&Usage {
                input_tokens: Some(21),
                output_tokens: Some(0),
                cache_read_tokens: Some(4),
                cache_write_tokens: Some(2),
                ..Usage::default()
            }),
        );
        Usage::merge_patch(
            &mut slot,
            Some(&Usage {
                output_tokens: Some(5),
                ..Usage::default()
            }),
        );
        let usage = slot.expect("merged");
        assert_eq!(usage.filled_counts(), (21, 5, 26));
        assert_eq!(usage.cache_read_tokens, Some(4));
        assert_eq!(usage.cache_write_tokens, Some(2));
    }
}
