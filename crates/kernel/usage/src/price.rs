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
        (input as f64 * self.input
            + output as f64 * self.output
            + cache_read as f64 * self.cache_read
            + cache_write_5m as f64 * self.cache_write
            + cache_write_1h as f64 * hour_rate)
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
        let input = details.uncached_input_tokens.or(input_tokens).unwrap_or(0);
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

/// Unix time of 2027-03-01 00:00:00 Pacific Standard Time.
///
/// That civil midnight is 08:00:00 UTC because March 1 is before US daylight
/// time. Ultrafast on GPT-6.2 Astra and Sol is 3x before this instant and 6x
/// from it onward.
pub const GPT62_ULTRAFAST_FULL_RATE_UNIX: i64 = 1_803_888_000;

/// Whether a reply's tier replaces the tier that was sent.
///
/// An empty served tier, and `auto`, fall back to what was sent. `default`
/// and `standard` are the standard rate. A ChatGPT backend that says those
/// words as placeholders drops them before this check.
#[must_use]
pub fn meaningful_served_tier(tier: &str) -> bool {
    !matches!(tier, "" | "auto")
}

/// The tier a row is priced at.
///
/// The served word wins, including `default` and `standard` at 1x. Only an
/// empty served tier or `auto` falls back to the tier that was sent.
#[must_use]
pub fn billed_tier<'a>(served: &'a str, sent: &'a str) -> &'a str {
    if meaningful_served_tier(served) {
        served
    } else {
        sent
    }
}

/// List price scaled by the tier that ran, at `at_unix`.
///
/// Fast and `priority` are 2x. Ultrafast is 6x, except GPT-6.2 Astra and Sol
/// through 2027-02-28 Pacific, which are 3x. Flex is 0.5x. Any other tier,
/// including an empty one, leaves the list price unchanged. A custom price
/// must not be passed here.
#[must_use]
pub fn price_at_tier(price: TokenPrice, model: &str, tier: &str, at_unix: i64) -> TokenPrice {
    let factor = tier_multiplier(model, tier, at_unix);
    if factor == 1.0 {
        return price;
    }
    TokenPrice {
        input: price.input * factor,
        output: price.output * factor,
        cache_read: price.cache_read * factor,
        cache_write: price.cache_write * factor,
        cache_write_1h: price.cache_write_1h * factor,
    }
}

/// Multiplier for `tier` on `model` at `at_unix`.
#[must_use]
pub fn tier_multiplier(model: &str, tier: &str, at_unix: i64) -> f64 {
    match tier {
        "flex" => 0.5,
        "priority" | "fast" => 2.0,
        "ultrafast" => ultrafast_multiplier(model, at_unix),
        _ => 1.0,
    }
}

fn ultrafast_multiplier(model: &str, at_unix: i64) -> f64 {
    if gpt62_astra_or_sol(model) && at_unix < GPT62_ULTRAFAST_FULL_RATE_UNIX {
        3.0
    } else {
        6.0
    }
}

fn gpt62_astra_or_sol(model: &str) -> bool {
    let id = comparable_model(model);
    matches!(id.as_str(), "gpt-6.2-astra" | "gpt-6.2-sol")
}

fn comparable_model(model: &str) -> String {
    let mut id = model.trim().to_ascii_lowercase();
    if let Some((_, tail)) = id.rsplit_once('/') {
        id = tail.to_owned();
    }
    while let Some(rest) = id.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            break;
        };
        id = rest[end + 1..].to_owned();
    }
    if let Some((base, _)) = id.split_once(':') {
        id = base.to_owned();
    }
    strip_dated(&id).to_owned()
}

fn strip_dated(id: &str) -> &str {
    let bytes = id.as_bytes();
    if bytes.len() >= 12 && bytes[bytes.len() - 11] == b'-' {
        let tail = &id[bytes.len() - 10..];
        if calendar_day(tail) {
            return &id[..bytes.len() - 11];
        }
    }
    if bytes.len() >= 9 && bytes[bytes.len() - 9] == b'-' {
        let tail = &id[bytes.len() - 8..];
        if tail.len() == 8 && tail.chars().all(|character| character.is_ascii_digit()) {
            return &id[..bytes.len() - 9];
        }
    }
    id
}

fn calendar_day(tail: &str) -> bool {
    let bytes = tail.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && tail[..4]
            .chars()
            .all(|character| character.is_ascii_digit())
        && tail[5..7]
            .chars()
            .all(|character| character.is_ascii_digit())
        && tail[8..10]
            .chars()
            .all(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{
        GPT62_ULTRAFAST_FULL_RATE_UNIX, TokenPrice, billed_tier, price_at_tier, tier_multiplier,
    };

    fn list() -> TokenPrice {
        TokenPrice {
            input: 1.0,
            output: 4.0,
            cache_read: 0.1,
            cache_write: 1.25,
            cache_write_1h: 2.0,
        }
    }

    #[test]
    fn d224_calls_are_priced_at_their_tier() {
        let day = GPT62_ULTRAFAST_FULL_RATE_UNIX;
        let priority = price_at_tier(list(), "gpt-6-sol", "priority", day);
        assert_eq!(priority.input, 2.0);
        assert_eq!(priority.output, 8.0);
        assert_eq!(priority.cache_write_1h, 4.0);
        assert_eq!(tier_multiplier("gpt-6-sol", "fast", day), 2.0);
        let ultra = price_at_tier(list(), "gpt-6.1-sol", "ultrafast", day);
        assert_eq!(ultra.input, 6.0);
        assert!((ultra.cache_read - 0.6).abs() < 1e-9);
        let flex = price_at_tier(list(), "gpt-6-sol", "flex", day);
        assert_eq!(flex.output, 2.0);
        let plain = price_at_tier(list(), "gpt-6-sol", "", day);
        assert_eq!(plain, list());
        assert_eq!(billed_tier("auto", "priority"), "priority");
        assert_eq!(billed_tier("", "priority"), "priority");
        assert_eq!(billed_tier("flex", "priority"), "flex");
        assert_eq!(billed_tier("priority", "ultrafast"), "priority");
        let served_priority = price_at_tier(
            list(),
            "gpt-6-sol",
            billed_tier("priority", "ultrafast"),
            day,
        );
        assert_eq!(served_priority.input, 2.0);
        assert_eq!(billed_tier("default", "ultrafast"), "default");
        assert_eq!(billed_tier("standard", "ultrafast"), "standard");
        let served_default = price_at_tier(
            list(),
            "gpt-6.2-sol",
            billed_tier("default", "ultrafast"),
            day,
        );
        assert_eq!(served_default, list());
        assert_eq!(tier_multiplier("gpt-6.2-sol", "default", day), 1.0);
        assert_eq!(tier_multiplier("gpt-6.2-sol", "standard", day), 1.0);
    }

    #[test]
    fn d224_dated_ultrafast_uses_the_pacific_day() {
        let before = GPT62_ULTRAFAST_FULL_RATE_UNIX - 1;
        let on = GPT62_ULTRAFAST_FULL_RATE_UNIX;
        for model in [
            "gpt-6.2-astra",
            "gpt-6.2-sol",
            "relay/gpt-6.2-sol",
            "GPT-6.2-Astra-2026-09-14",
        ] {
            assert_eq!(tier_multiplier(model, "ultrafast", before), 3.0, "{model}");
            assert_eq!(tier_multiplier(model, "ultrafast", on), 6.0, "{model}");
        }
        assert_eq!(tier_multiplier("gpt-6.1-sol", "ultrafast", before), 6.0);
        assert_eq!(tier_multiplier("gpt-6-astra", "ultrafast", before), 6.0);
        let early = price_at_tier(list(), "gpt-6.2-sol", "ultrafast", before);
        assert_eq!(early.input, 3.0);
        assert_eq!(early.cache_write, 3.75);
        let later = price_at_tier(list(), "gpt-6.2-sol", "priority", before);
        assert_eq!(later.input, 2.0);
    }
}
