//! Stored list prices and cost from token counts.

use crate::UsageDetails;

/// USD per million tokens recorded on a usage row.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TokenPrice {
    /// Uncached and cache-miss input.
    pub input: f64,
    /// Output tokens, reasoning included in the protocol output count.
    pub output: f64,
    /// Cache-read tokens.
    pub cache_read: f64,
    /// Five-minute cache-write tokens.
    pub cache_write: f64,
    /// One-hour cache-write tokens; zero means bill at the five-minute rate.
    pub cache_write_1h: f64,
}

/// Outcome of pricing one request at record time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowPricing {
    /// Whether a list price was known (including an explicit zero).
    pub priced: bool,
    /// Effective cost in USD when [`Self::priced`] is true.
    pub cost_usd: f64,
    /// The list price used for [`Self::cost_usd`].
    pub price: TokenPrice,
}

impl RowPricing {
    /// A deliberate zero-cost row that still counts as priced.
    #[must_use]
    pub const fn free() -> Self {
        Self {
            priced: true,
            cost_usd: 0.0,
            price: TokenPrice {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
                cache_write_1h: 0.0,
            },
        }
    }
}

impl TokenPrice {
    /// Applies the reference `CostSplit` formula without context tiers.
    ///
    /// Reasoning tokens are billed as output and are already inside the output
    /// count.
    #[must_use]
    pub fn cost_split(
        self,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write_total: i64,
        cache_write_1h: i64,
    ) -> f64 {
        let input = i64::max(input, 0);
        let output = i64::max(output, 0);
        let cache_read = i64::max(cache_read, 0);
        let cache_write_total = i64::max(cache_write_total, 0);
        let cache_write_1h = i64::min(i64::max(cache_write_1h, 0), cache_write_total);
        let cache_write_5m = cache_write_total - cache_write_1h;
        let hour_rate = if self.cache_write_1h > 0.0 {
            self.cache_write_1h
        } else {
            self.cache_write
        };
        (f64::from(input) * self.input
            + f64::from(output) * self.output
            + f64::from(cache_read) * self.cache_read
            + f64::from(cache_write_5m) * self.cache_write
            + f64::from(cache_write_1h) * hour_rate)
            / 1_000_000.0
    }

    /// Costs one gateway row from its token columns.
    #[must_use]
    pub fn cost_for_row(
        self,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        details: UsageDetails,
    ) -> f64 {
        let input = details
            .uncached_input_tokens
            .or(input_tokens)
            .unwrap_or(0);
        let output = output_tokens.unwrap_or(0);
        let cache_read = details.cache_read_tokens.unwrap_or(0);
        let write_5m = details.cache_write_5m_tokens.unwrap_or(0);
        let write_1h = details.cache_write_1h_tokens.unwrap_or(0);
        let write_total = details
            .cache_write_tokens
            .unwrap_or_else(|| write_5m.saturating_add(write_1h));
        self.cost_split(input, output, cache_read, write_total, write_1h)
    }
}

/// Whether token counts are large enough to attempt a list price.
#[must_use]
pub fn token_bearing(input: Option<i64>, output: Option<i64>) -> bool {
    input.unwrap_or(0).saturating_add(output.unwrap_or(0)) > 0
}
