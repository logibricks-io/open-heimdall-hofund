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
    /// Group by client-facing model id.
    Model,
    /// Group by answering provider member id.
    Provider,
    /// Group by subscription account on the upstream id.
    Account,
    /// Group by client session id.
    Session,
    /// Group by local calendar day.
    Day,
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
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Account => "account",
            Self::Session => "session",
            Self::Day => "day",
            Self::Unknown => "unknown",
        }
    }

    /// Parses a breakdown query token.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "agent" => Some(Self::Agent),
            "provider-key" => Some(Self::ProviderKey),
            "gateway-key" => Some(Self::GatewayKey),
            "model" => Some(Self::Model),
            "provider" => Some(Self::Provider),
            "account" => Some(Self::Account),
            "session" => Some(Self::Session),
            "day" => Some(Self::Day),
            _ => None,
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
    /// Mean time to first content in milliseconds when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_ttft_ms: Option<f64>,
    /// Weighted mean output tokens per second after first content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_tokens_per_second: Option<f64>,
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
    /// Rolling window length in days (`--days`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<String>,
    /// Rolling window duration (`--last`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// Inclusive window start (`--since`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Subscription account filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Client-facing model filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// When true, return every row in the window instead of the row cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unlimited: bool,
}

impl UsageQuery {
    /// Constructs a usage query.
    #[must_use]
    pub fn new(period: UsagePeriod) -> Self {
        Self {
            period,
            ..Self::default()
        }
    }

    /// Constructs a usage query with a breakdown dimension.
    #[must_use]
    pub fn with_breakdown(period: UsagePeriod, by: UsageBreakdownBy) -> Self {
        Self {
            period,
            by: Some(by),
            ..Self::default()
        }
    }

    /// Parses usage query parameters from a query string.
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
        let mut out = Self::default();
        for (key, value) in query_pairs(query) {
            if value.is_empty() {
                continue;
            }
            match key.as_str() {
                "period" => out.period = parse_period(&value),
                "by" | "group-by" | "group_by" => {
                    out.by = UsageBreakdownBy::parse(&value).or(Some(UsageBreakdownBy::Unknown));
                }
                "days" => out.days = Some(value),
                "last" => out.last = Some(value),
                "since" => out.since = Some(value),
                "account" => out.account = Some(value),
                "model" => out.model = Some(value),
                "unlimited" if value == "1" || value.eq_ignore_ascii_case("true") => {
                    out.unlimited = true;
                }
                _ => {}
            }
        }
        out
    }

    /// Encodes this query for `GET /v1/hofund/usage`.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut parts = vec![format!("period={}", period_token(self.period))];
        if let Some(by) = self.by {
            parts.push(format!("by={}", by.as_query_value()));
        }
        for (key, value) in [
            ("days", &self.days),
            ("last", &self.last),
            ("since", &self.since),
            ("account", &self.account),
            ("model", &self.model),
        ] {
            if let Some(value) = value {
                parts.push(format!("{key}={}", percent_encode(value)));
            }
        }
        if self.unlimited {
            parts.push("unlimited=1".to_owned());
        }
        parts.join("&")
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
            let message = "unknown usage breakdown; expected agent, provider-key, gateway-key, model, provider, account, session, or day";
            let mut error = ControlError::new(ControlErrorKind::BadRequest, message, "validate");
            error
                .details
                .push(FieldIssue::new("by", "unknown_variant", message));
            return Err(error);
        }
        let rolling = [
            self.days.as_deref().filter(|value| !value.is_empty()),
            self.last.as_deref().filter(|value| !value.is_empty()),
            self.since.as_deref().filter(|value| !value.is_empty()),
        ];
        let rolling_count = rolling.iter().filter(|value| value.is_some()).count();
        if rolling_count > 1 {
            let message = "take one of --days, --last, or --since, not several";
            return Err(ControlError::new(
                ControlErrorKind::BadRequest,
                message,
                "validate",
            ));
        }
        if rolling_count > 0 && self.period != UsagePeriod::All {
            let message = "take a rolling window or a calendar period, not both";
            return Err(ControlError::new(
                ControlErrorKind::BadRequest,
                message,
                "validate",
            ));
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
    /// Time to first content in milliseconds when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Output tokens per second after first content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_second: Option<f64>,
    /// Subscription account parsed from the upstream id when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account: String,
    /// Client session id when recorded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
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
    /// Mean time to first content in milliseconds when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_ttft_ms: Option<f64>,
    /// Weighted mean output tokens per second after first content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_tokens_per_second: Option<f64>,
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
    /// Human-readable window label for rolling or custom ranges.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub window_label: String,
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
            window_label: String::new(),
        }
    }
}

fn period_token(period: UsagePeriod) -> &'static str {
    match period {
        UsagePeriod::Day => "day",
        UsagePeriod::Week => "week",
        UsagePeriod::Month => "month",
        UsagePeriod::All => "all",
        UsagePeriod::Unknown => "unknown",
    }
}

fn parse_period(value: &str) -> UsagePeriod {
    match value {
        "day" | "today" => UsagePeriod::Day,
        "week" | "7d" => UsagePeriod::Week,
        "month" | "30d" => UsagePeriod::Month,
        "all" => UsagePeriod::All,
        _ => UsagePeriod::Unknown,
    }
}

fn query_pairs(query: Option<&str>) -> impl Iterator<Item = (String, String)> + '_ {
    query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high << 4 | low);
                        index += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn default_currency() -> String {
    "USD".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{UsageBreakdownBy, UsagePeriod, UsageQuery};

    #[test]
    fn u1c_15_usage_query_validate_rejects_unknown_breakdown() {
        let query = UsageQuery {
            period: UsagePeriod::All,
            by: Some(UsageBreakdownBy::Unknown),
            ..UsageQuery::default()
        };
        let error = query.validate().expect_err("unknown by");
        assert!(error.message.contains("provider-key"));
    }

    #[test]
    fn r10_usage_query_parse_group_by_alias_and_conflicts() {
        let parsed = UsageQuery::parse(Some("period=week&group-by=model&model=demo"));
        assert_eq!(parsed.period, UsagePeriod::Week);
        assert_eq!(parsed.by, Some(UsageBreakdownBy::Model));
        assert_eq!(parsed.model.as_deref(), Some("demo"));
        let conflict = UsageQuery {
            days: Some("1".to_owned()),
            last: Some("1h".to_owned()),
            ..UsageQuery::default()
        };
        assert!(conflict.validate().is_err());
    }
}
