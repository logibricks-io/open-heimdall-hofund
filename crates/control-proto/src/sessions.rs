//! Read-only session listing and reporting contracts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, validate_revision};

/// Default maximum number of rows returned by the session list.
pub const DEFAULT_SESSION_LIST_LIMIT: usize = 5000;

/// Session list filters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionListQuery {
    /// Number of local calendar days to include, or zero for all.
    pub days: Option<u32>,
    /// Case-insensitive model substring.
    pub model: String,
    /// Case-insensitive working-folder substring.
    pub folder: String,
    /// Maximum rows, clamped to [`DEFAULT_SESSION_LIST_LIMIT`]; absent uses that default.
    pub limit: Option<usize>,
}

impl SessionListQuery {
    /// Parses and validates the list query string.
    ///
    /// # Errors
    ///
    /// Returns `BadRequest` for an unknown, duplicate, or malformed parameter.
    pub fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut out = Self::default();
        for (key, value) in query_fields(query, &["days", "model", "folder", "limit"])? {
            match key.as_str() {
                "days" => out.days = Some(parse_days(&value)?),
                "model" => out.model = value,
                "folder" => out.folder = value,
                "limit" => {
                    out.limit = Some(
                        value
                            .parse::<usize>()
                            .ok()
                            .filter(|limit| *limit > 0)
                            .ok_or_else(|| bad_query("limit must be a positive integer"))?,
                    );
                }
                _ => unreachable!("allowed query key"),
            }
        }
        Ok(out)
    }

    /// Effective row cap.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.limit
            .unwrap_or(DEFAULT_SESSION_LIST_LIMIT)
            .min(DEFAULT_SESSION_LIST_LIMIT)
    }
}

/// Session daily-stat filters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionStatsQuery {
    /// Number of local calendar days to include, or zero for all.
    pub days: Option<u32>,
    /// Case-insensitive model substring.
    pub model: String,
    /// Case-insensitive working-folder substring.
    pub folder: String,
}

impl SessionStatsQuery {
    /// Parses and validates the stats query string.
    ///
    /// # Errors
    ///
    /// Returns `BadRequest` for an unknown, duplicate, or malformed parameter.
    pub fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut out = Self::default();
        for (key, value) in query_fields(query, &["days", "model", "folder"])? {
            match key.as_str() {
                "days" => out.days = Some(parse_days(&value)?),
                "model" => out.model = value,
                "folder" => out.folder = value,
                _ => unreachable!("allowed query key"),
            }
        }
        Ok(out)
    }
}

/// Session overview filters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionOverviewQuery {
    /// Number of local calendar days to include, or zero for all.
    pub days: Option<u32>,
    /// Exact agent id.
    pub agent: String,
    /// Exact model id.
    pub model: String,
    /// Exact working folder.
    pub cwd: String,
}

impl SessionOverviewQuery {
    /// Parses and validates the overview query string.
    ///
    /// # Errors
    ///
    /// Returns `BadRequest` for an unknown, duplicate, or malformed parameter.
    pub fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut out = Self::default();
        for (key, value) in query_fields(query, &["days", "agent", "model", "cwd"])? {
            match key.as_str() {
                "days" => out.days = Some(parse_days(&value)?),
                "agent" => out.agent = value,
                "model" => out.model = value,
                "cwd" => out.cwd = value,
                _ => unreachable!("allowed query key"),
            }
        }
        Ok(out)
    }
}

/// Token counters on a session or aggregate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionTokenTotals {
    /// Uncached and cache-miss input tokens.
    pub input: u64,
    /// Output tokens, including reasoning.
    pub output: u64,
    /// Cached input tokens read.
    pub cache_read: u64,
    /// Cached tokens written.
    pub cache_write: u64,
}

/// Resume command as argv plus cwd for `std::process::Command`, never a shell line.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionResume {
    /// Executable and arguments, without shell joining.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: String,
}

/// One listed session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionSummary {
    /// Public session id used in detail route paths.
    pub id: String,
    /// Agent plugin id.
    pub agent: String,
    /// Display title.
    pub title: String,
    /// Working folder.
    pub cwd: String,
    /// Last message time in Unix milliseconds.
    pub last_ms: i64,
    /// Token-bearing model ids.
    pub models: Vec<String>,
    /// Listed subagent ids accepted by the transcript `sub` selector.
    pub subagents: Vec<String>,
    /// Total token counters.
    pub tokens: SessionTokenTotals,
    /// USD cost.
    pub cost: f64,
    /// Number of token-bearing models without a price.
    pub unpriced: u64,
    /// Resume command, or null when the session cannot be resumed.
    pub resume: Option<SessionResume>,
    /// Whether the source cannot be mutated.
    pub read_only: bool,
    /// Whether this row comes only from gateway records.
    pub gateway: bool,
}

/// Current session-read consent shown beside a list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionConsent {
    /// Whether the user has answered the consent prompt.
    pub asked: bool,
    /// Default for agents without an explicit answer.
    pub all: bool,
    /// Explicitly enabled agent ids.
    pub enabled: Vec<String>,
}

/// Session list response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionListResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 resource revision.
    pub revision: String,
    /// Current read consent.
    pub consent: SessionConsent,
    /// Newest-first sessions.
    pub sessions: Vec<SessionSummary>,
}

impl Default for SessionListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            revision: String::new(),
            consent: SessionConsent::default(),
            sessions: Vec::new(),
        }
    }
}

/// One date's aggregate tokens and cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionDayStats {
    /// Local date in `YYYY-MM-DD` form.
    pub date: String,
    /// Tokens used on this date.
    pub tokens: SessionTokenTotals,
    /// USD cost on this date.
    pub cost: f64,
}

/// Tokens grouped by model or folder name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionStatShare {
    /// Model id or working folder.
    pub name: String,
    /// Tokens attributed to this name.
    pub tokens: SessionTokenTotals,
}

/// Session daily-stat response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionStatsResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 resource revision.
    pub revision: String,
    /// Aggregate token counters.
    pub tokens: SessionTokenTotals,
    /// Aggregate USD cost.
    pub cost: f64,
    /// Number of session/model pairs without a price.
    pub unpriced: u64,
    /// Daily totals in date order.
    pub days: Vec<SessionDayStats>,
    /// Model totals in name order.
    pub models: Vec<SessionStatShare>,
    /// Folder totals in name order.
    pub folders: Vec<SessionStatShare>,
}

impl Default for SessionStatsResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            revision: String::new(),
            tokens: SessionTokenTotals::default(),
            cost: 0.0,
            unpriced: 0,
            days: Vec::new(),
            models: Vec::new(),
            folders: Vec::new(),
        }
    }
}

/// Session count and cost on one local date.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionOverviewDay {
    /// Local date in `YYYY-MM-DD` form.
    pub date: String,
    /// Sessions active on this date.
    pub count: u64,
    /// USD cost on this date.
    pub cost: f64,
}

/// One high-cost session in an overview.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionOverviewTop {
    /// Public session id.
    pub id: String,
    /// Display title.
    pub title: String,
    /// USD cost.
    pub cost: f64,
}

/// Session overview response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionOverviewResponse {
    /// Body wire version.
    pub version: u32,
    /// Number of matching sessions.
    pub count: u64,
    /// Nearest-rank median session cost.
    pub median_cost: f64,
    /// Daily counts and costs in date order.
    pub by_day: Vec<SessionOverviewDay>,
    /// Up to eight highest-cost sessions.
    pub top: Vec<SessionOverviewTop>,
}

impl Default for SessionOverviewResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            count: 0,
            median_cost: 0.0,
            by_day: Vec::new(),
            top: Vec::new(),
        }
    }
}

/// Session index progress response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionProgressResponse {
    /// Body wire version.
    pub version: u32,
    /// Files discovered in the current or latest refresh.
    pub files_total: u64,
    /// Files completed in the current or latest refresh.
    pub files_done: u64,
    /// Whether a refresh is currently reading files.
    pub running: bool,
}

impl Default for SessionProgressResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            files_total: 0,
            files_done: 0,
            running: false,
        }
    }
}

/// One session and whether its transcript can be read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionDetailResponse {
    /// Body wire version.
    pub version: u32,
    /// The same session object returned by the list route.
    #[serde(flatten)]
    pub session: SessionSummary,
    /// Whether the plugin supports reading this session's own transcript.
    pub transcript_available: bool,
}

impl Default for SessionDetailResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            session: SessionSummary::default(),
            transcript_available: false,
        }
    }
}

/// One transcript content part on the control wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionTranscriptPart {
    /// Speaker: user, assistant, or tool.
    pub role: String,
    /// Content kind.
    pub kind: String,
    /// Tool name when present.
    pub name: String,
    /// Text content.
    pub text: String,
}

/// Transcript response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionTranscriptResponse {
    /// Body wire version.
    pub version: u32,
    /// Content parts in file order.
    pub parts: Vec<SessionTranscriptPart>,
    /// Whether input bounds omitted content.
    pub cut: bool,
    /// Optional source label.
    pub source: String,
}

impl Default for SessionTranscriptResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            parts: Vec::new(),
            cut: false,
            source: String::new(),
        }
    }
}

/// Optional subagent selector for a transcript read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionTranscriptQuery {
    /// An id listed on the parent session's `subagents`.
    pub sub: Option<String>,
}

impl SessionTranscriptQuery {
    /// Parses and validates the transcript query string.
    ///
    /// # Errors
    ///
    /// Returns `BadRequest` for an unknown, duplicate, or malformed parameter.
    pub fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut out = Self::default();
        for (key, value) in query_fields(query, &["sub"])? {
            if key == "sub" && !value.is_empty() {
                out.sub = Some(value);
            }
        }
        Ok(out)
    }
}

/// Session search filters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionSearchQuery {
    /// Exact agent id, or empty for every consented agent.
    pub agent: String,
    /// Case-folded substring to find.
    pub q: String,
}

impl SessionSearchQuery {
    /// Parses and validates the search query string.
    ///
    /// # Errors
    ///
    /// Returns `BadRequest` for an empty query, unknown key, duplicate key, or
    /// malformed encoding.
    pub fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut out = Self::default();
        for (key, value) in query_fields(query, &["agent", "q"])? {
            if key == "agent" {
                out.agent = value;
            } else {
                out.q = value;
            }
        }
        if out.q.is_empty() {
            return Err(bad_query("q must not be empty"));
        }
        Ok(out)
    }
}

/// One search hit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionSearchHit {
    /// Public session id.
    pub id: String,
    /// One-line excerpt around the first match.
    pub excerpt: String,
}

/// Session search response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionSearchResponse {
    /// Body wire version.
    pub version: u32,
    /// Matching sessions in newest-first order.
    pub hits: Vec<SessionSearchHit>,
    /// Whether the hit cap or time budget ended the search.
    pub cut: bool,
}

impl Default for SessionSearchResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            hits: Vec::new(),
            cut: false,
        }
    }
}

/// Manage query filters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionManageQuery {
    /// Exact agent id, or empty for every agent.
    pub agent: String,
}

impl SessionManageQuery {
    /// Parses and validates the manage query string.
    ///
    /// # Errors
    ///
    /// Returns `BadRequest` for an unknown, duplicate, or malformed parameter.
    pub fn parse(query: Option<&str>) -> Result<Self, ControlError> {
        let mut out = Self::default();
        for (_key, value) in query_fields(query, &["agent"])? {
            out.agent = value;
        }
        Ok(out)
    }
}

/// One agent's session count and deletion capability.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionManageAgent {
    /// Agent plugin id.
    pub agent: String,
    /// Listed session count.
    pub count: u64,
    /// Whether session deletion is available. It is false in the read-only row.
    pub deletable: bool,
}

/// One trash listing row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionTrashRow {
    /// Trash entry id.
    pub id: String,
    /// Agent plugin id.
    pub agent: String,
    /// Display title.
    pub title: String,
}

/// Read-only session management listing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionManageResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 resource revision.
    pub revision: String,
    /// Per-agent listing counts.
    pub agents: Vec<SessionManageAgent>,
    /// Active agent filter.
    pub agent: String,
    /// Listed sessions, capped at [`DEFAULT_SESSION_LIST_LIMIT`].
    pub sessions: Vec<SessionSummary>,
    /// Trash rows; empty until session mutation ships.
    pub trash: Vec<SessionTrashRow>,
    /// Session trash directory.
    pub trash_dir: String,
    /// Whether gateway recording is enabled; false until that row ships.
    pub recording: bool,
    /// Whether gateway recordings exist; false until that row ships.
    pub recorded: bool,
}

impl Default for SessionManageResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            revision: String::new(),
            agents: Vec::new(),
            agent: String::new(),
            sessions: Vec::new(),
            trash: Vec::new(),
            trash_dir: String::new(),
            recording: false,
            recorded: false,
        }
    }
}

/// One found or explicitly configured agent's read state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionReadAgent {
    /// Agent instance id.
    pub agent: String,
    /// Whether current consent permits this agent's files.
    pub on: bool,
}

/// Session read-consent response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionReadsResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 resource revision.
    pub revision: String,
    /// Whether the user has answered the consent prompt.
    pub asked: bool,
    /// Default for agents without an explicit answer.
    pub all: bool,
    /// Explicitly enabled agent ids.
    pub enabled: Vec<String>,
    /// Found and explicitly configured agents.
    pub found: Vec<SessionReadAgent>,
}

impl Default for SessionReadsResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            revision: String::new(),
            asked: false,
            all: false,
            enabled: Vec::new(),
            found: Vec::new(),
        }
    }
}

/// Session read-consent update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct SessionReadsPatchRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected resource revision, when tracked by the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
    /// New default for agents without an explicit answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<bool>,
    /// Per-agent answers to merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<BTreeMap<String, bool>>,
}

impl Default for SessionReadsPatchRequest {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            expected_revision: None,
            all: None,
            agents: None,
        }
    }
}

impl SessionReadsPatchRequest {
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if let Some(revision) = &self.expected_revision {
            validate_revision("expected_revision", revision)?;
        }
        Ok(())
    }
}

/// Carry request for continuing a session in another agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct SessionCarryRequest {
    /// Body wire version.
    pub version: u32,
    /// Public id of the session to carry.
    pub id: String,
    /// Destination agent id (`omp` for Pi carry).
    pub to: String,
}

impl Default for SessionCarryRequest {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            id: String::new(),
            to: String::new(),
        }
    }
}

impl SessionCarryRequest {
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if self.id.is_empty() {
            return Err(bad_query("id is required"));
        }
        if self.id.len() > 256 {
            return Err(bad_query("id is too long"));
        }
        if self.to.is_empty() {
            return Err(bad_query("to is required"));
        }
        if self.to.len() > 64 {
            return Err(bad_query("to is too long"));
        }
        Ok(())
    }
}

/// Carry response with spawn argv instead of a shell line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionCarryResponse {
    /// Body wire version.
    pub version: u32,
    /// Destination agent id.
    pub agent: String,
    /// Session id the carry applies to.
    pub id: String,
    /// Command to run in the session folder.
    pub command: SessionResume,
}

impl Default for SessionCarryResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            agent: String::new(),
            id: String::new(),
            command: SessionResume::default(),
        }
    }
}

fn parse_days(value: &str) -> Result<u32, ControlError> {
    match value {
        "0" | "1" | "7" | "30" | "90" => value
            .parse::<u32>()
            .map_err(|_| bad_query("days must be 0, 1, 7, 30, or 90")),
        _ => Err(bad_query("days must be 0, 1, 7, 30, or 90")),
    }
}

fn query_fields(
    query: Option<&str>,
    allowed: &[&str],
) -> Result<Vec<(String, String)>, ControlError> {
    let mut fields = Vec::new();
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key).ok_or_else(|| bad_query("malformed query encoding"))?;
        let value = percent_decode(value).ok_or_else(|| bad_query("malformed query encoding"))?;
        if !allowed.contains(&key.as_str()) {
            return Err(bad_query("unknown sessions query parameter"));
        }
        if fields.iter().any(|(seen, _)| seen == &key) {
            return Err(bad_query("sessions query parameters may appear only once"));
        }
        fields.push((key, value));
    }
    Ok(fields)
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' => {
                let high = hex(*bytes.get(index + 1)?)?;
                let low = hex(*bytes.get(index + 2)?)?;
                out.push(high << 4 | low);
                index += 2;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn bad_query(message: &str) -> ControlError {
    ControlError::new(ControlErrorKind::BadRequest, message, "validate")
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SESSION_LIST_LIMIT, SessionCarryRequest, SessionListQuery, SessionManageQuery,
        SessionOverviewQuery, SessionReadsPatchRequest, SessionSearchQuery, SessionStatsQuery,
        SessionTranscriptQuery,
    };
    use crate::{ControlErrorKind, MAX_ID_BYTES};

    fn carry(id: impl Into<String>, to: impl Into<String>) -> SessionCarryRequest {
        SessionCarryRequest {
            version: crate::API_VERSION,
            id: id.into(),
            to: to.into(),
        }
    }

    #[test]
    fn list_query_accepts_only_the_contract_days() {
        assert_eq!(SessionListQuery::parse(None).expect("empty").days, None);
        for days in ["0", "1", "7", "30", "90"] {
            assert!(SessionListQuery::parse(Some(&format!("days={days}"))).is_ok());
        }
        for query in [
            "days=2",
            "days=",
            "days=all",
            "limit=0",
            "unknown=x",
            "days=1&days=7",
        ] {
            assert!(SessionListQuery::parse(Some(query)).is_err(), "{query}");
        }
    }

    #[test]
    fn stats_and_overview_have_distinct_filters() {
        assert!(SessionStatsQuery::parse(Some("days=7&model=a&folder=b")).is_ok());
        assert!(SessionStatsQuery::parse(Some("limit=1")).is_err());
        let overview = SessionOverviewQuery::parse(Some("agent=a&model=m&cwd=%2Fwork%2Frepo"))
            .expect("overview");
        assert_eq!(overview.cwd, "/work/repo");
        assert!(SessionOverviewQuery::parse(Some("folder=repo")).is_err());
    }

    #[test]
    fn carry_request_validate_rejects_empty_and_overlong_fields() {
        assert!(carry("", "omp").validate().is_err(), "empty id");
        assert!(carry("s", "").validate().is_err(), "empty to");
        assert!(carry("s", "omp").validate().is_ok());
        assert!(
            carry("a".repeat(256), "omp").validate().is_ok(),
            "id of 256 is accepted"
        );
        assert!(
            carry("a".repeat(257), "omp").validate().is_err(),
            "id of 257 is rejected"
        );
        assert!(
            carry("s", "b".repeat(64)).validate().is_ok(),
            "to of 64 is accepted"
        );
        assert!(
            carry("s", "b".repeat(65)).validate().is_err(),
            "to of 65 is rejected"
        );
    }

    #[test]
    fn list_query_keeps_model_folder_and_positive_limit() {
        let query =
            SessionListQuery::parse(Some("model=gpt-4&folder=%2Ftmp&limit=10")).expect("list");
        assert_eq!(query.model, "gpt-4");
        assert_eq!(query.folder, "/tmp");
        assert_eq!(query.limit(), 10);
        assert_eq!(
            SessionListQuery::default().limit(),
            DEFAULT_SESSION_LIST_LIMIT
        );
        assert_eq!(
            SessionListQuery::parse(Some("limit=1"))
                .expect("one")
                .limit(),
            1
        );
        assert_eq!(
            SessionListQuery::parse(Some("limit=5001"))
                .expect("excessive")
                .limit(),
            DEFAULT_SESSION_LIST_LIMIT
        );
        let error = SessionListQuery::parse(Some("limit=0")).expect_err("zero");
        assert_eq!(error.kind, ControlErrorKind::BadRequest);
        assert!(error.message.contains("positive integer"));
    }

    #[test]
    fn overview_days_and_lowercase_percent_decoding() {
        let query = SessionOverviewQuery::parse(Some("days=7&cwd=%2fwork")).expect("overview");
        assert_eq!(query.days, Some(7));
        assert_eq!(query.cwd, "/work");
        let a = SessionOverviewQuery::parse(Some("cwd=%61")).expect("lowercase a");
        assert_eq!(a.cwd, "a");
        let zero = SessionOverviewQuery::parse(Some("cwd=%30")).expect("digit zero");
        assert_eq!(zero.cwd, "0");
    }

    #[test]
    fn transcript_search_and_manage_queries_keep_fields() {
        let transcript = SessionTranscriptQuery::parse(Some("sub=child-1")).expect("sub");
        assert_eq!(transcript.sub.as_deref(), Some("child-1"));
        assert_eq!(
            SessionTranscriptQuery::parse(Some("sub="))
                .expect("empty sub")
                .sub,
            None
        );
        assert_eq!(SessionTranscriptQuery::parse(None).expect("none").sub, None);

        let search = SessionSearchQuery::parse(Some("agent=one&q=hello")).expect("search");
        assert_eq!(search.agent, "one");
        assert_eq!(search.q, "hello");
        let empty = SessionSearchQuery::parse(Some("q=")).expect_err("empty q");
        assert_eq!(empty.kind, ControlErrorKind::BadRequest);
        assert!(empty.message.contains("q must not be empty"));

        let manage = SessionManageQuery::parse(Some("agent=pi")).expect("manage");
        assert_eq!(manage.agent, "pi");
    }

    #[test]
    fn reads_patch_rejects_an_overlong_revision_and_unknown_params_are_bad_requests() {
        let ok = SessionReadsPatchRequest::default();
        ok.validate().expect("default");
        let bad = SessionReadsPatchRequest {
            expected_revision: Some("x".repeat(MAX_ID_BYTES + 1)),
            ..SessionReadsPatchRequest::default()
        };
        let error = bad.validate().expect_err("too long");
        assert_eq!(error.kind, ControlErrorKind::Unprocessable);
        let unknown = SessionListQuery::parse(Some("nope=1")).expect_err("unknown");
        assert_eq!(unknown.kind, ControlErrorKind::BadRequest);
        assert!(unknown.message.contains("unknown sessions query parameter"));
    }
}
