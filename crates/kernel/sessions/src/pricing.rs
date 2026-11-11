//! Pricing for parsed session token counters.

use hofund_plugin_api::{SessionRecord, SessionTokens};
pub use hofund_usage::TokenPrice;

/// Prices every token-bearing model using one caller-supplied settings snapshot.
pub fn price_records(
    records: &mut [SessionRecord],
    mut price: impl FnMut(&str, &str) -> Option<TokenPrice>,
) {
    for record in records {
        record.cost = 0.0;
        record.unpriced = 0;
        for (model, tokens) in &record.models {
            if let Some(price) = price(&record.agent, model) {
                record.cost += token_cost(price, *tokens);
            } else {
                record.unpriced += 1;
            }
        }
    }
}

/// USD cost of one counter set at `price` per million tokens.
#[must_use]
pub fn token_cost(price: TokenPrice, tokens: SessionTokens) -> f64 {
    fn signed(value: u64) -> i64 {
        i64::try_from(value).unwrap_or(i64::MAX)
    }
    price.cost_split(
        signed(tokens.input),
        signed(tokens.output),
        signed(tokens.cache_read),
        signed(tokens.cache_write),
        signed(tokens.cache_write_1h),
    )
}
