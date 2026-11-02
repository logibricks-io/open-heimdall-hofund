//! Token counts billed on a try that failed before the client saw a reply.

use hofund_ir::Usage;

/// Token counts already reported for a failed upstream try.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BilledUsage {
    /// Prompt tokens billed.
    pub input_tokens: Option<i64>,
    /// Completion tokens billed.
    pub output_tokens: Option<i64>,
    /// Cache-read tokens billed.
    pub cache_read_tokens: Option<i64>,
    /// Five-minute cache-write tokens billed.
    pub cache_write_5m_tokens: Option<i64>,
    /// One-hour cache-write tokens billed.
    pub cache_write_1h_tokens: Option<i64>,
}

impl BilledUsage {
    /// Whether any billed token counter is strictly positive.
    #[must_use]
    pub fn token_bearing(&self) -> bool {
        token_sum(self) > 0
    }

    /// Merges counters from a stream or unary usage block.
    pub fn merge_usage(&mut self, usage: &Usage) {
        merge_i64(&mut self.input_tokens, usage.input_tokens);
        merge_i64(&mut self.output_tokens, usage.output_tokens);
        merge_i64(&mut self.cache_read_tokens, usage.cache_read_tokens);
        merge_i64(
            &mut self.cache_write_5m_tokens,
            usage.cache_write_5m_tokens.or(usage.cache_write_tokens),
        );
        merge_i64(&mut self.cache_write_1h_tokens, usage.cache_write_1h_tokens);
    }
}

fn token_sum(billed: &BilledUsage) -> i64 {
    [
        billed.input_tokens,
        billed.output_tokens,
        billed.cache_read_tokens,
        billed.cache_write_5m_tokens,
        billed.cache_write_1h_tokens,
    ]
    .into_iter()
    .flatten()
    .filter(|value| *value > 0)
    .sum()
}

fn merge_i64(slot: &mut Option<i64>, patch: Option<u64>) {
    let Some(patch) = patch else {
        return;
    };
    let patch = i64::try_from(patch).unwrap_or(i64::MAX);
    *slot = Some(slot.unwrap_or(0).saturating_add(patch));
}
