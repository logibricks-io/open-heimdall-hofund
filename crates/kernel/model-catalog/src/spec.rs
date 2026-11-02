//! Resolved limits and prices for one model.

use serde::{Deserialize, Serialize};

/// Smallest context window taken from models.dev at face value.
pub const LEAST_WINDOW: u32 = 2048;

/// USD per million tokens, ready for usage cost code (U-1b).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// Input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Cache-read tokens.
    pub cache_read: f64,
    /// Cache-write tokens.
    pub cache_write: f64,
    /// One-hour cache-write tokens; zero bills at the five-minute rate.
    #[serde(default)]
    pub cache_write_1h: f64,
}

/// Context, output, vision, and reasoning metadata for routing and listings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelSpec {
    /// Usable context window in tokens.
    pub context_window: Option<u32>,
    /// Maximum output tokens.
    pub max_output: Option<u32>,
    /// Accepts image input.
    pub vision: bool,
    /// Supports reasoning.
    pub reasoning: bool,
    /// Accepted reasoning levels, in catalog order.
    pub reasoning_levels: Vec<String>,
    /// Normalized input modalities.
    pub input_modalities: Vec<String>,
    /// Normalized output modalities.
    pub output_modalities: Vec<String>,
}

impl ModelSpec {
    /// Millionths of a dollar per million tokens for plugin-api compatibility.
    #[must_use]
    pub fn input_price_micros(&self, price: &Price) -> u64 {
        micros(price.input)
    }

    /// Output price in micro-dollars per million tokens.
    #[must_use]
    pub fn output_price_micros(&self, price: &Price) -> u64 {
        micros(price.output)
    }

    /// Cache-read price in micro-dollars per million tokens.
    #[must_use]
    pub fn cache_read_price_micros(&self, price: &Price) -> u64 {
        micros(price.cache_read)
    }
}

fn micros(value: f64) -> u64 {
    if value <= 0.0 {
        return 0;
    }
    (value * 1_000_000.0).round() as u64
}
