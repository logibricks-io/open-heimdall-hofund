use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// An RFC 3339 timestamp on a quota reading or window.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QuotaTime(pub String);

impl QuotaTime {
    /// Constructs a timestamp already formatted as RFC 3339.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The formatted timestamp.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether a quota belongs to an account, a bought plan, or a key balance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuotaKind {
    /// A signed-in account's renewable allowance.
    #[default]
    Subscription,
    /// A plan bought with a key.
    Plan,
    /// A key's monetary balance.
    Balance,
    /// A newer peer supplied a kind this version does not know.
    #[serde(other)]
    Unknown,
}

/// One account, plan, or key allowance.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    /// Provider or subscription id.
    pub provider: String,
    /// Display name.
    pub name: String,
    /// Allowance kind.
    pub kind: QuotaKind,
    /// Plan label, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plan: String,
    /// Account or key label, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// Reported windows.
    pub windows: Vec<QuotaSpan>,
    /// Balance text, when the provider reports one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub balance: String,
    /// A read failure not covered by a cached reading.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// When a cached reading was originally made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<QuotaTime>,
    /// Whether no upstream request was made for this reading.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_deferred: bool,
    /// When the displayed reading was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<QuotaTime>,
    /// When a paid plan term ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<QuotaTime>,
    /// Renewal mode: auto, off, or empty when unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub renew: String,
    /// When this member last answered through the gateway in the last 30 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_served_at: Option<QuotaTime>,
    /// Whether this is the most recently served card.
    #[serde(default, skip_serializing_if = "is_false")]
    pub last: bool,
    /// User-chosen account alias.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alias: String,
}

/// One window of an allowance.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSpan {
    /// Whether this window has no cap.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unlimited: bool,
    /// Window name.
    pub name: String,
    /// Percent used.
    pub used: f64,
    /// Percent left, never negative.
    pub remaining: f64,
    /// Absolute reset time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<QuotaTime>,
    /// The provider's own count text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display: String,
    /// Whether display is text rather than a share reading.
    #[serde(default, skip_serializing_if = "is_false")]
    pub text: bool,
    /// Amount used in the provider's unit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub amount: f64,
    /// Window cap in the provider's unit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub limit: f64,
    /// Unit for amount and limit.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Shared pool this window draws on.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pool: String,
    /// When the soonest remaining portion ends apart from the reset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_out_at: Option<QuotaTime>,
    /// How much is lost at runsOutAt.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub runs_out: f64,
    /// A prepaid wallet amount, where used and remaining are not shares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<f64>,
}

/// One sampled window reading.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaPoint {
    /// When the reading was made.
    pub at: QuotaTime,
    /// Percent left.
    pub left: f64,
    /// Start of this window cycle, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<QuotaTime>,
    /// End of this window cycle, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<QuotaTime>,
}

/// One window's sampled points, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaLine {
    /// Window name.
    pub name: String,
    /// Sampled points.
    pub points: Vec<QuotaPoint>,
}

/// One account's window history.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaHistory {
    /// Provider or subscription id.
    pub provider: String,
    /// Lowercase account id.
    pub user: String,
    /// Window histories in name order.
    pub lines: Vec<QuotaLine>,
}

/// The list envelope returned by the model quota APIs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListEnvelope<T> {
    /// Always list.
    pub object: String,
    /// Entries.
    pub data: Vec<T>,
}

impl<T> ListEnvelope<T> {
    /// Wraps entries in the reference list envelope.
    #[must_use]
    pub fn new(data: Vec<T>) -> Self {
        Self {
            object: "list".to_owned(),
            data,
        }
    }
}

/// Query for `GET /v1/hofund/quotas`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaQuery {
    /// Answer from the kept readings without asking any vendor.
    pub kept: bool,
}

impl QuotaQuery {
    /// A fresh report, read from the vendors now.
    #[must_use]
    pub fn fresh() -> Self {
        Self { kept: false }
    }

    /// A report of the kept readings; no vendor is asked.
    #[must_use]
    pub fn kept() -> Self {
        Self { kept: true }
    }

    /// Parses a query string. `kept=1` selects the kept report; any other
    /// parameter is ignored.
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
        Self {
            kept: query_pairs(query).any(|(key, value)| key == "kept" && value == "1"),
        }
    }

    /// The query string for this request, without the leading `?`.
    #[must_use]
    pub fn encode(&self) -> String {
        if self.kept { "kept=1".to_owned() } else { String::new() }
    }
}

/// Query for `GET /v1/hofund/quotas/history`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaHistoryQuery {
    /// Days back, as given. Invalid or absent values mean the whole retention.
    pub days: Option<String>,
    /// Provider id to keep.
    pub provider: Option<String>,
    /// Account to keep, compared without case.
    pub user: Option<String>,
}

impl QuotaHistoryQuery {
    /// Parses `days`, `provider`, and `user`; empty values are absent and any
    /// other parameter is ignored.
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
        let mut out = Self::default();
        for (key, value) in query_pairs(query) {
            if value.is_empty() {
                continue;
            }
            match key.as_str() {
                "days" => out.days = Some(value),
                "provider" => out.provider = Some(value),
                "user" => out.user = Some(value),
                _ => {}
            }
        }
        out
    }

    /// Sets the days back.
    #[must_use]
    pub fn with_days(mut self, days: impl Into<String>) -> Self {
        self.days = Some(days.into());
        self
    }

    /// Sets the provider filter.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Sets the account filter.
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    /// The query string for this request, without the leading `?`.
    #[must_use]
    pub fn encode(&self) -> String {
        [
            ("days", &self.days),
            ("provider", &self.provider),
            ("user", &self.user),
        ]
        .into_iter()
        .filter_map(|(key, value)| {
            value
                .as_deref()
                .map(|value| format!("{key}={}", percent_encode(value)))
        })
        .collect::<Vec<_>>()
        .join("&")
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

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Whether one account has allowance now, as `hofund quota wait` reads it.
///
/// This is the daemon's own reckoning over windows that stop the account for
/// every model; windows scoped to one model or set aside never count.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct QuotaStanding {
    /// Provider or subscription id.
    pub provider: String,
    /// Display name.
    pub name: String,
    /// Account label; empty for a provider-wide card.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// A reading made now that names windows: no error and not a cached reading.
    pub readable: bool,
    /// A window that stops the account is used up and has not reset.
    pub used_up: bool,
    /// When the last stopping window starts again; absent when one does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub back_at: Option<QuotaTime>,
    /// Why the reading failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl QuotaStanding {
    /// A standing for one account.
    #[must_use]
    pub fn new(provider: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            user: user.into(),
            ..Self::default()
        }
    }
}

/// Control response containing quota snapshots.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaResponse {
    /// Body wire version.
    pub version: u32,
    /// Quota snapshots.
    pub data: Vec<Quota>,
    /// Each subscription account's standing, for `hofund quota wait`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standings: Vec<QuotaStanding>,
}

impl Default for QuotaResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            data: Vec::new(),
            standings: Vec::new(),
        }
    }
}

/// Control response containing quota histories.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuotaHistoryResponse {
    /// Body wire version.
    pub version: u32,
    /// Window histories.
    pub data: Vec<QuotaHistory>,
}

impl Default for QuotaHistoryResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            data: Vec::new(),
        }
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Quota, QuotaKind, QuotaSpan, QuotaTime};

    #[test]
    fn q1a_quota_json_uses_reference_names_and_omissions() {
        let mut quota = Quota {
            provider: "p".to_owned(),
            name: "Provider".to_owned(),
            kind: QuotaKind::Plan,
            plan: "pro".to_owned(),
            user: "A@example.com".to_owned(),
            as_of: Some(QuotaTime::new("2026-10-01T00:00:00Z")),
            read_deferred: true,
            read_at: Some(QuotaTime::new("2026-10-01T00:00:00Z")),
            until: Some(QuotaTime::new("2026-11-01T00:00:00Z")),
            renew: "auto".to_owned(),
            last_served_at: Some(QuotaTime::new("2026-10-02T00:00:00Z")),
            last: true,
            alias: "work".to_owned(),
            ..Quota::default()
        };
        let value = serde_json::to_value(&quota).expect("quota JSON");
        for key in [
            "provider",
            "name",
            "kind",
            "plan",
            "user",
            "windows",
            "asOf",
            "readDeferred",
            "readAt",
            "until",
            "renew",
            "lastServedAt",
            "last",
            "alias",
        ] {
            assert!(value.get(key).is_some(), "missing {key}: {value}");
        }
        for key in ["balance", "error", "as_of", "read_deferred", "last_served_at"] {
            assert!(value.get(key).is_none(), "unexpected {key}: {value}");
        }
        quota.kind = QuotaKind::Balance;
        quota.balance = "5 credits".to_owned();
        assert_eq!(serde_json::to_value(quota).expect("balance")["kind"], "balance");
        assert_eq!(
            serde_json::to_value(Quota::default()).expect("empty"),
            json!({"provider":"", "name":"", "kind":"subscription", "windows":[]})
        );
    }

    #[test]
    fn q1b_quota_span_json_includes_runs_out_fields() {
        let span = QuotaSpan {
            unlimited: true,
            name: "Credits".to_owned(),
            used: 40.0,
            remaining: 60.0,
            resets_at: Some(QuotaTime::new("2026-10-30T17:00:00Z")),
            display: "1 / 2".to_owned(),
            text: true,
            amount: 1.0,
            limit: 2.0,
            unit: "credits".to_owned(),
            pool: "shared".to_owned(),
            runs_out_at: Some(QuotaTime::new("2026-10-30T17:34:42Z")),
            runs_out: 18.44,
            balance: Some(6291.0),
        };
        let value = serde_json::to_value(span).expect("span JSON");
        assert_eq!(value["runsOutAt"], "2026-10-30T17:34:42Z");
        assert_eq!(value["runsOut"], 18.44);
        assert_eq!(value["balance"], 6291.0);
        for key in [
            "unlimited",
            "name",
            "used",
            "remaining",
            "resetsAt",
            "display",
            "text",
            "amount",
            "limit",
            "unit",
            "pool",
            "runsOutAt",
            "runsOut",
            "balance",
        ] {
            assert!(value.get(key).is_some(), "missing {key}: {value}");
        }
        let empty = serde_json::to_value(QuotaSpan::default()).expect("empty span");
        assert_eq!(empty, json!({"name":"", "used":0.0, "remaining":0.0}));
        assert!(empty.get("runsOutAt").is_none());
        assert!(matches!(empty, Value::Object(_)));
    }
}
