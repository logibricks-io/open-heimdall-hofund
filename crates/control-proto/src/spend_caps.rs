use std::fmt;

use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Spend-cap reporting window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderSpendCapPeriod {
    /// Local calendar day.
    Day,
    /// Local calendar month.
    Month,
    /// Since the cap was set.
    Total,
}

/// One provider key spend cap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderSpendCap {
    /// Configured provider id.
    pub provider: String,
    /// Ten-hex key fingerprint.
    pub key_id: String,
    /// USD ceiling.
    pub usd: f64,
    /// Window preset.
    pub period: ProviderSpendCapPeriod,
    /// Unix seconds when a total cap started.
    pub set_at: i64,
    /// USD spent in the active window at list prices.
    pub spent_usd: f64,
}

/// `GET` spend-cap list response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderSpendCapsResponse {
    /// Body wire version.
    pub version: u32,
    /// Caps for the requested provider.
    pub caps: Vec<ProviderSpendCap>,
}

impl Default for ProviderSpendCapsResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            caps: Vec::new(),
        }
    }
}

/// Set or clear one key's cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProviderSpendCapRequest {
    /// Body wire version.
    pub version: u32,
    /// Ten-hex key fingerprint.
    pub key_id: String,
    /// `50/day`, `200/month`, `1000/total`, or empty to clear.
    pub cap: String,
}

impl ProviderSpendCapRequest {
    /// Names the key the change applies to.
    #[must_use]
    pub fn for_id(key_id: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            key_id: key_id.into(),
            cap: String::new(),
        }
    }
}

impl fmt::Display for ProviderSpendCapPeriod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Day => f.write_str("day"),
            Self::Month => f.write_str("month"),
            Self::Total => f.write_str("total"),
        }
    }
}
