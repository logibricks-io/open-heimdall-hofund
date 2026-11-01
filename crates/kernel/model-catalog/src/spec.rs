//! Resolved limits and prices for one model.

use serde::{Deserialize, Serialize};

/// Smallest context window taken from models.dev at face value.
pub const LEAST_WINDOW: u32 = 2048;

/// USD per million tokens, ready for usage cost code (U-1b).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// Context, output, vision, and reasoning metadata for routing and listings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelSpec {
    pub context_window: Option<u32>,
    pub max_output: Option<u32>,
    pub vision: bool,
    pub reasoning: bool,
    pub reasoning_levels: Vec<String>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
}

impl ModelSpec {
    /// Millionths of a dollar per million tokens for plugin-api compatibility.
    #[must_use]
    pub fn input_price_micros(&self, price: &Price) -> u64 {
        micros(price.input)
    }

    #[must_use]
    pub fn output_price_micros(&self, price: &Price) -> u64 {
        micros(price.output)
    }

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
