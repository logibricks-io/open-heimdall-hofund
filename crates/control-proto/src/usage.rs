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

/// Query-only usage request. It uses the path version and has no JSON body
/// version field. Query-string parsing belongs to the HTTP layer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    /// Requested reporting period.
    pub period: UsagePeriod,
}

impl UsageQuery {
    /// Constructs a usage query.
    #[must_use]
    pub fn new(period: UsagePeriod) -> Self {
        Self { period }
    }

    /// Rejects a period this version does not understand. The HTTP layer must
    /// call this after parsing the query string, so a typo cannot silently
    /// widen or narrow the report.
    ///
    /// # Errors
    ///
    /// Returns a `BadRequest` error with an `unknown_variant` detail for
    /// [`UsagePeriod::Unknown`].
    pub fn validate(&self) -> Result<(), ControlError> {
        if self.period != UsagePeriod::Unknown {
            return Ok(());
        }
        let message = "unknown usage period; expected day, week, month, or all";
        let mut error = ControlError::new(ControlErrorKind::BadRequest, message, "validate");
        error
            .details
            .push(FieldIssue::new("period", "unknown_variant", message));
        Err(error)
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
        }
    }
}

fn default_currency() -> String {
    "USD".to_owned()
}
