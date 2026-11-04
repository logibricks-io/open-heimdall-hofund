//! Usage reporting period and breakdown dimensions.
use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, FieldIssue};

/// Usage reporting period.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum UsagePeriod {
    /// Current day.
    Day,
    /// Current week.
    Week,
    /// Current month.
    Month,
    /// All retained rows.
    #[default]
    All,
    /// A newer peer supplied an unknown period.
    #[serde(other)]
    Unknown,
}

/// Usage breakdown dimension for grouped reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "kebab-case")]
pub enum UsageBreakdownBy {
    /// Group by detected client agent.
    Agent,
    /// Group by provider API key fingerprint.
    ProviderKey,
    /// Group by authenticated gateway key.
    GatewayKey,
    /// A newer peer supplied an unknown breakdown.
    #[serde(other)]
    Unknown,
}

impl UsageBreakdownBy {
    /// Query-string token for this dimension.
    #[must_use]
    pub const fn as_query_value(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::ProviderKey => "provider-key",
            Self::GatewayKey => "gateway-key",
            Self::Unknown => "unknown",
        }
    }
}

/// One group's totals in a usage breakdown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(default)]
pub struct UsageBreakdownGroup {
    /// Stable group id.
    pub id: String,
    /// Display label (masked for provider keys).
    pub label: String,
    /// Request count in the group.
    pub requests: u64,
    /// Sum of input tokens.
    pub input_tokens: u64,
    /// Sum of output tokens.
    pub output_tokens: u64,
    /// Sum of stored costs in the response currency.
    pub cost: f64,
    /// Token-bearing rows without a stored list price.
    pub unpriced: u64,
}

/// Query-only usage request. It uses the path version and has no JSON body
/// version field. Query-string parsing belongs to the HTTP layer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    /// Requested reporting period.
    pub period: UsagePeriod,
    /// When set, the response includes a grouped breakdown instead of only rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<UsageBreakdownBy>,
}

impl UsageQuery {
    /// Constructs a usage query.
    #[must_use]
    pub fn new(period: UsagePeriod) -> Self {
        Self { period, by: None }
    }

    /// Constructs a usage query with a breakdown dimension.
    #[must_use]
    pub fn with_breakdown(period: UsagePeriod, by: UsageBreakdownBy) -> Self {
        Self {
            period,
            by: Some(by),
        }
    }

    /// Rejects unknown query values. The HTTP layer must call this after
    /// parsing the query string.
    ///
    /// # Errors
    ///
    /// Returns a `BadRequest` error with an `unknown_variant` detail for
    /// unrecognized period or breakdown values.
    pub fn validate(&self) -> Result<(), ControlError> {
        if self.period == UsagePeriod::Unknown {
            let message = "unknown usage period; expected day, week, month, or all";
            let mut error = ControlError::new(ControlErrorKind::BadRequest, message, "validate");
            error
                .details
                .push(FieldIssue::new("period", "unknown_variant", message));
            return Err(error);
        }
        if self.by == Some(UsageBreakdownBy::Unknown) {
            let message = "unknown usage breakdown; expected agent, provider-key, or gateway-key";
            let mut error = ControlError::new(ControlErrorKind::BadRequest, message, "validate");
            error
                .details
                .push(FieldIssue::new("by", "unknown_variant", message));
            return Err(error);
        }
        Ok(())
    }
}

/// One usage row with unsigned wire quantities.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(default)]
pub struct UsageRow {
    /// Unix milliseconds when the row was recorded.
    pub created_at_ms: u64,
    /// Client-facing model id.
    pub model: String,
    /// Provider id that answered, or empty when none did.
    pub provider: String,
    /// Input tokens when reported.
    pub input_tokens: Option<u64>,
    /// Output tokens when reported.
    pub output_tokens: Option<u64>,
    /// Unsigned elapsed milliseconds.
    pub duration_ms: u64,
    /// HTTP status label.
    pub status: String,
    /// Authenticated gateway key id, absent in older peers.
    #[serde(default)]
    pub caller_key_id: String,
    /// Authenticated gateway key name, absent in older peers.
    #[serde(default)]
    pub caller_key_name: String,
    /// Issuer of the authenticated key, absent in older peers.
    #[serde(default)]
    pub caller_issuer_id: String,
    /// Cache-read tokens when stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Five-minute cache-write tokens when stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m_tokens: Option<u64>,
    /// One-hour cache-write tokens when stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_tokens: Option<u64>,
    /// Reasoning tokens when stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Reasoning streamed without a separate usage count.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning_hidden: bool,
    /// Stored cost in the response currency when the row was priced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Whether a list price was stored for this row.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub priced: bool,
    /// Row kind for handed-off, refusal, or turned-away tries; empty for ordinary rows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub outcome: String,
    /// Detected client agent id, absent in older peers.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_agent: String,
}

/// Aggregated usage totals.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(default)]
pub struct UsageTotals {
    /// Number of requests.
    pub requests: u64,
    /// Sum of known input token counts.
    pub input_tokens: u64,
    /// Sum of known output token counts.
    pub output_tokens: u64,
    /// Sum of cache-read tokens.
    pub cache_read_tokens: u64,
    /// Sum of five-minute cache-write tokens.
    pub cache_write_5m_tokens: u64,
    /// Sum of one-hour cache-write tokens.
    pub cache_write_1h_tokens: u64,
    /// Sum of reasoning tokens.
    pub reasoning_tokens: u64,
    /// Sum of stored USD costs for priced rows.
    pub cost: f64,
    /// Token-bearing rows with no stored list price.
    pub unpriced: u64,
}

/// Usage response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageResponse {
    /// Body wire version.
    pub version: u32,
    /// Reporting period used by the server.
    pub period: UsagePeriod,
    /// Usage rows.
    pub rows: Vec<UsageRow>,
    /// Aggregate totals over every row in the period, not only `rows`.
    pub totals: UsageTotals,
    /// Whether the server dropped older rows from `rows` to stay within its
    /// row cap. Absent from older peers, which never truncate.
    #[serde(default)]
    pub rows_truncated: bool,
    /// Currency code for `totals.cost` (`USD` or `CNY`).
    #[serde(default = "default_currency")]
    pub currency: String,
    /// Yuan per US dollar when `currency` is `CNY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cny_per_usd: Option<f64>,
    /// Breakdown dimension when the client asked for grouped totals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakdown_by: Option<UsageBreakdownBy>,
    /// Grouped totals when `breakdown_by` is set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breakdown: Vec<UsageBreakdownGroup>,
}

impl Default for UsageResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            period: UsagePeriod::All,
            rows: Vec::new(),
            totals: UsageTotals::default(),
            rows_truncated: false,
            currency: default_currency(),
            cny_per_usd: None,
            breakdown_by: None,
            breakdown: Vec::new(),
        }
    }
}

fn default_currency() -> String {
    "USD".to_owned()
}
