use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Why a request was served on another model than the one it named.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelMove {
    /// Model id routing chose instead of the asked name.
    pub to: String,
    /// Short reason shown in the routing view.
    pub why: String,
}

impl ModelMove {
    /// Constructs a stand-in move record.
    #[must_use]
    pub fn new(to: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            to: to.into(),
            why: why.into(),
        }
    }
}

/// Why a candidate was not tried, or was moved behind others.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkipReason {
    /// Member-wide or model-scoped cooldown.
    Cooldown {
        /// Cooldown cause label when known.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        cause: String,
    },
    /// Quota or credit exhaustion before the attempt.
    Quota,
    /// Resting after an earlier failure.
    Rest,
    /// Strategy ordering placed another member ahead.
    Strategy,
    /// Gateway key scope disabled this member.
    Disabled,
    /// Prompt longer than the member's context window.
    Context {
        /// Known window size when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
    },
    /// Provider refused the same content on every account.
    ProviderRefused,
    /// An attempt failed and failover moved on.
    Failure {
        /// HTTP status when present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        /// Sanitized failure text.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
    },
    /// A newer peer supplied a reason this version does not know.
    #[serde(other)]
    Unknown,
}

/// One ordered routing candidate before or during dispatch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouteCandidate {
    /// Stable member id.
    pub member: String,
    /// Configured provider id.
    pub provider: String,
    /// Model name that would be sent upstream.
    pub model: String,
    /// Position in the final try order, zero-based.
    pub order: u32,
    /// Whether this candidate was not attempted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub skipped: bool,
    /// Why the candidate was skipped or deferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<SkipReason>,
}

impl RouteCandidate {
    /// Constructs one candidate row for the routing trace.
    #[must_use]
    pub fn new(
        member: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        order: u32,
        skipped: bool,
        skip: Option<SkipReason>,
    ) -> Self {
        Self {
            member: member.into(),
            provider: provider.into(),
            model: model.into(),
            order,
            skipped,
            skip,
        }
    }
}

/// One upstream attempt within a routing decision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouteTry {
    /// Stable member id.
    pub member: String,
    /// Configured provider id.
    pub provider: String,
    /// Model sent on the wire.
    pub model: String,
    /// Whether the attempt finished.
    #[serde(default, skip_serializing_if = "is_false")]
    pub done: bool,
    /// HTTP status when the attempt ended with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Why the attempt failed, when it did.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fail: String,
}

impl RouteTry {
    /// Constructs one attempt row for the routing trace.
    #[must_use]
    pub fn new(
        member: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        done: bool,
        status: Option<u16>,
        fail: impl Into<String>,
    ) -> Self {
        Self {
            member: member.into(),
            provider: provider.into(),
            model: model.into(),
            done,
            status,
            fail: fail.into(),
        }
    }
}

/// One completed or in-flight routing decision.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoutingDecision {
    /// Monotonic id within this process.
    pub id: u64,
    /// Sequence number that advances on every trace change.
    pub seq: u64,
    /// Unix milliseconds when the decision was opened.
    pub time_ms: u64,
    /// Client session id when the request carried one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    /// Agent label when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// Gateway key id when the request used a named key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gateway_key: String,
    /// Correlation id from request events when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    /// Model name the client asked for.
    pub asked: String,
    /// Registered routing strategy for the pool.
    #[serde(default = "default_routing")]
    pub routing: String,
    /// Stand-in that changed the model before routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved: Option<ModelMove>,
    /// Ordered candidates as routing weighed them.
    #[serde(default)]
    pub candidates: Vec<RouteCandidate>,
    /// Attempts in order.
    #[serde(default)]
    pub tries: Vec<RouteTry>,
    /// Member id that served the reply, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub served_member: String,
    /// Provider id that served the reply, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub served_provider: String,
    /// Conversation affinity summary when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub affinity: String,
    /// Whether routing finished.
    #[serde(default, skip_serializing_if = "is_false")]
    pub done: bool,
    /// Final HTTP status when the client received an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Error text when routing or the upstream failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// True when no member could be selected before dispatch.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unrouted: bool,
}

fn default_routing() -> String {
    "smart".to_owned()
}

const fn is_false(value: &bool) -> bool {
    !*value
}

/// Query parameters for `GET /v1/hofund/route`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteQuery {
    /// Optional session filter.
    pub session: Option<String>,
    /// Maximum rows to return, default 60, capped at 256.
    pub limit: usize,
    /// Wait for a sequence past this value when set with `wait`.
    pub after: Option<u64>,
    /// Seconds to wait for a change, capped at 60.
    pub wait_secs: Option<u64>,
    /// When set, only decisions for this gateway key id are returned.
    pub gateway_key: Option<String>,
}

impl Default for RouteQuery {
    fn default() -> Self {
        Self {
            session: None,
            limit: 60,
            after: None,
            wait_secs: None,
            gateway_key: None,
        }
    }
}

impl RouteQuery {
    /// Parses query string values.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        let mut out = Self {
            limit: 60,
            ..Self::default()
        };
        let Some(raw) = raw else {
            return out;
        };
        for pair in raw.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let value = percent_decode(value.trim());
            match key {
                "session" if !value.is_empty() => out.session = Some(truncate(&value, 128)),
                "limit" => {
                    if let Ok(limit) = value.parse::<usize>() {
                        out.limit = limit.clamp(1, 256);
                    }
                }
                "after" => out.after = value.parse().ok(),
                "wait" => out.wait_secs = value.parse().ok().map(|secs: u64| secs.min(60)),
                "gateway_key" if !value.is_empty() => {
                    out.gateway_key = Some(truncate(&value, 128));
                }
                _ => {}
            }
        }
        out
    }

    /// Encodes the query for a control GET request.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut parts = Vec::new();
        if let Some(session) = &self.session {
            parts.push(format!("session={}", encode_component(session)));
        }
        if self.limit != 60 {
            parts.push(format!("limit={}", self.limit));
        }
        if let Some(after) = self.after {
            parts.push(format!("after={after}"));
        }
        if let Some(wait) = self.wait_secs {
            parts.push(format!("wait={wait}"));
        }
        if let Some(key) = &self.gateway_key {
            parts.push(format!("gateway_key={}", encode_component(key)));
        }
        parts.join("&")
    }
}

fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            out.push(byte as char);
            index += 3;
            continue;
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    value.chars().take(max).collect()
}

/// Latest routing decisions for the management API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouteListResponse {
    /// Body wire version.
    pub version: u32,
    /// Current trace sequence.
    pub seq: u64,
    /// Decisions newest-first unless a session filter fixed order.
    pub decisions: Vec<RoutingDecision>,
}

impl Default for RouteListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            seq: 0,
            decisions: Vec::new(),
        }
    }
}

/// Latest routing decision for one session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionRouteResponse {
    /// Body wire version.
    pub version: u32,
    /// Session id echoed from the query.
    pub session: String,
    /// Sequence of the returned decision when present.
    pub seq: u64,
    /// Latest decision for the session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<RoutingDecision>,
}

impl SessionRouteResponse {
    /// Constructs an empty session response.
    #[must_use]
    pub fn empty(session: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            session: session.into(),
            seq: 0,
            decision: None,
        }
    }
}

/// A request waiting before any route was chosen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnroutedRequest {
    /// Monotonic hold id.
    pub id: u64,
    /// Unix milliseconds when the hold started.
    pub since_ms: u64,
    /// Client session when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session: String,
    /// Model name from the request when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub asked: String,
    /// Inbound protocol label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub inbound_protocol: String,
    /// Why routing has not started yet.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
}

/// Unrouted holds exposed on the management API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnroutedListResponse {
    /// Body wire version.
    pub version: u32,
    /// Active holds oldest-first.
    pub holds: Vec<UnroutedRequest>,
}

impl Default for UnroutedListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            holds: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RouteQuery, SkipReason};

    #[test]
    fn r1_1_route_query_parses_session_limit_and_after() {
        let query = RouteQuery::parse(Some("session=s1&limit=10&after=5&wait=120"));
        assert_eq!(query.session.as_deref(), Some("s1"));
        assert_eq!(query.limit, 10);
        assert_eq!(query.after, Some(5));
        assert_eq!(query.wait_secs, Some(60));
    }

    #[test]
    fn r1_2_skip_reason_serializes_with_kind_tag() {
        let reason = SkipReason::Failure {
            status: Some(429),
            message: "rate limited".to_owned(),
        };
        let json = serde_json::to_value(reason).expect("json");
        assert_eq!(json["kind"], "failure");
        assert_eq!(json["status"], 429);
    }
}
