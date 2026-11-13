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

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{SessionRecord, SessionTokens};
    use hofund_usage::TokenPrice;

    use super::{price_records, token_cost};

    fn unit_price() -> TokenPrice {
        TokenPrice {
            input: 1_000_000.0,
            output: 2_000_000.0,
            cache_read: 0.0,
            cache_write: 0.0,
            cache_write_1h: 0.0,
        }
    }

    #[test]
    fn token_cost_and_price_records_accumulate() {
        let tokens = SessionTokens::new(2, 3, 0, 0);
        let cost = token_cost(unit_price(), tokens);
        assert_eq!(cost, 8.0);

        let mut records = [
            SessionRecord::new("fake", "fake:a", "/a.jsonl"),
            SessionRecord::new("fake", "fake:b", "/b.jsonl"),
        ];
        records[0].add_usage("m", tokens);
        records[1].add_usage("n", SessionTokens::new(1, 0, 0, 0));
        price_records(&mut records, |_, model| {
            (model == "m").then_some(unit_price())
        });
        assert_eq!(records[0].cost, 8.0);
        assert_eq!(records[0].unpriced, 0);
        assert_eq!(records[1].cost, 0.0);
        assert_eq!(records[1].unpriced, 1);
    }
}
