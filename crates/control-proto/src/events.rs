use serde::{Deserialize, Serialize};

use crate::{
    API_VERSION, AgentAction, ControlError, ControlErrorKind, EventCursor, FieldIssue,
    ListenSource, ShutdownState, validate_boot_id,
};

/// Independent event sequence spaces carried by one SSE stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// Lifecycle, configuration, model, agent, and membership events.
    #[default]
    Control,
    /// Opt-in request summary events.
    Requests,
    /// A newer peer supplied an unknown topic.
    #[serde(other)]
    Unknown,
}

impl Topic {
    /// Canonical topics in composite-id order.
    pub const CANONICAL: [Self; 2] = [Self::Control, Self::Requests];

    /// Stable wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Requests => "requests",
            Self::Unknown => "unknown",
        }
    }
}

/// One versioned event envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EventEnvelope {
    /// Body wire version.
    pub version: u32,
    /// Daemon boot identity.
    pub boot_id: String,
    /// Topic containing this event.
    pub topic: Topic,
    /// Sequence within `topic` and `boot_id`.
    pub seq: u64,
    /// Unix milliseconds when the event was emitted.
    pub time_ms: u64,
    /// Typed event payload.
    pub payload: EventPayload,
}

impl Default for EventEnvelope {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            boot_id: String::new(),
            topic: Topic::Control,
            seq: 0,
            time_ms: 0,
            payload: EventPayload::Snapshot(SnapshotEvent::default()),
        }
    }
}

impl EventEnvelope {
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if validate_boot_id(&self.boot_id).is_err() {
            return Err(event_validation_error(
                "boot_id",
                "event boot id violates the composite event id grammar",
            ));
        }
        if self
            .payload
            .topic()
            .is_some_and(|topic| topic != self.topic)
        {
            return Err(event_validation_error(
                "topic",
                "event topic does not match its payload topic",
            ));
        }
        Ok(())
    }
}

fn event_validation_error(field: &str, message: &'static str) -> ControlError {
    let mut error = ControlError::new(ControlErrorKind::BadRequest, message, "validate");
    error
        .details
        .push(FieldIssue::new(field, "invalid", message));
    error
}

/// Event payload variants and their topic membership. Request events belong to
/// `requests`; every known non-request variant is emitted on `control`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum EventPayload {
    /// Initial per-connection state snapshot.
    Snapshot(SnapshotEvent),
    /// A candidate configuration was published.
    ConfigReloaded(ConfigReloadedEvent),
    /// A candidate configuration was rejected.
    ConfigRejected(ConfigRejectedEvent),
    /// The model listener could not move to its effective address.
    ListenerRelistenFailed(ListenerRelistenFailedEvent),
    /// A member entered cooldown.
    MemberCooled(MemberCooledEvent),
    /// A member left cooldown.
    MemberRecovered(MemberRecoveredEvent),
    /// A new route snapshot was published.
    SnapshotPublished(SnapshotPublishedEvent),
    /// A request started.
    RequestStarted(RequestStartedEvent),
    /// A request finished.
    RequestFinished(RequestFinishedEvent),
    /// A provider model list refresh completed.
    ModelsRefreshed(ModelsRefreshedEvent),
    /// Agent files changed.
    AgentChanged(AgentChangedEvent),
    /// Reserved sign-in progress event.
    SignInProgress(SignInProgressEvent),
    /// The bearer token rotated; no token material is included.
    TokenRotated(TokenRotatedEvent),
    /// Graceful shutdown started.
    ShutdownStarted(ShutdownStartedEvent),
    /// The client must replace one topic's state from a fresh snapshot.
    ResyncRequired(ResyncRequiredEvent),
    /// A newer peer supplied an unknown event tag.
    #[serde(other)]
    Unknown,
}

impl<'de> Deserialize<'de> for EventPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let event = value
            .get("event")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("event tag must be a string"))?;
        let data = value
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        macro_rules! decode {
            ($variant:ident, $type:ident) => {
                Self::$variant(
                    serde_json::from_value::<$type>(data).map_err(serde::de::Error::custom)?,
                )
            };
        }
        Ok(match event {
            "snapshot" => decode!(Snapshot, SnapshotEvent),
            "config_reloaded" => decode!(ConfigReloaded, ConfigReloadedEvent),
            "config_rejected" => decode!(ConfigRejected, ConfigRejectedEvent),
            "listener_relisten_failed" => {
                decode!(ListenerRelistenFailed, ListenerRelistenFailedEvent)
            }
            "member_cooled" => decode!(MemberCooled, MemberCooledEvent),
            "member_recovered" => decode!(MemberRecovered, MemberRecoveredEvent),
            "snapshot_published" => decode!(SnapshotPublished, SnapshotPublishedEvent),
            "request_started" => decode!(RequestStarted, RequestStartedEvent),
            "request_finished" => decode!(RequestFinished, RequestFinishedEvent),
            "models_refreshed" => decode!(ModelsRefreshed, ModelsRefreshedEvent),
            "agent_changed" => decode!(AgentChanged, AgentChangedEvent),
            "sign_in_progress" => decode!(SignInProgress, SignInProgressEvent),
            "token_rotated" => decode!(TokenRotated, TokenRotatedEvent),
            "shutdown_started" => decode!(ShutdownStarted, ShutdownStartedEvent),
            "resync_required" => decode!(ResyncRequired, ResyncRequiredEvent),
            _ => Self::Unknown,
        })
    }
}

impl EventPayload {
    /// Topic on which this variant is emitted, or `None` for an unknown event.
    #[must_use]
    pub const fn topic(&self) -> Option<Topic> {
        match self {
            Self::RequestStarted(_) | Self::RequestFinished(_) => Some(Topic::Requests),
            Self::Unknown => None,
            _ => Some(Topic::Control),
        }
    }

    /// Wire tag of this variant, as used for the SSE `event:` field.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Snapshot(_) => "snapshot",
            Self::ConfigReloaded(_) => "config_reloaded",
            Self::ConfigRejected(_) => "config_rejected",
            Self::ListenerRelistenFailed(_) => "listener_relisten_failed",
            Self::MemberCooled(_) => "member_cooled",
            Self::MemberRecovered(_) => "member_recovered",
            Self::SnapshotPublished(_) => "snapshot_published",
            Self::RequestStarted(_) => "request_started",
            Self::RequestFinished(_) => "request_finished",
            Self::ModelsRefreshed(_) => "models_refreshed",
            Self::AgentChanged(_) => "agent_changed",
            Self::SignInProgress(_) => "sign_in_progress",
            Self::TokenRotated(_) => "token_rotated",
            Self::ShutdownStarted(_) => "shutdown_started",
            Self::ResyncRequired(_) => "resync_required",
            Self::Unknown => "unknown",
        }
    }
}

/// Minimal secret-free route member summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemberSummary {
    /// Stable member id string.
    pub id: String,
    /// Whether the member is ready for calls.
    pub ready: bool,
}

/// Initial snapshot payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SnapshotEvent {
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// SHA-256 published runtime revision.
    pub runtime_revision: String,
    /// Publication generation.
    pub generation: u64,
    /// Restart-only fields not yet active.
    pub pending_restart: Vec<String>,
    /// Live fields whose update was not applied.
    pub not_applied: Vec<FieldIssue>,
    /// Current route members.
    pub members: Vec<MemberSummary>,
    /// Latest cursor for every requested topic.
    pub cursors: Vec<EventCursor>,
    /// Effective model listener address, mirroring `StatusResponse`.
    #[serde(default)]
    pub effective_listen: String,
    /// Source of the effective model listener address.
    #[serde(default)]
    pub listen_source: ListenSource,
    /// Daemon shutdown state.
    #[serde(default)]
    pub shutdown: ShutdownState,
    /// Number of requests in flight when the snapshot was taken.
    #[serde(default)]
    pub in_flight: u64,
}

/// Successful configuration publication payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigReloadedEvent {
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// SHA-256 published runtime revision.
    pub runtime_revision: String,
    /// Publication generation.
    pub generation: u64,
    /// Stable ids of changed members.
    pub changed_members: Vec<String>,
    /// Restart-only fields not yet active.
    pub pending_restart: Vec<String>,
    /// Live fields whose update was not applied.
    pub not_applied: Vec<FieldIssue>,
}

/// Rejected configuration payload. The previous runtime remains active.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigRejectedEvent {
    /// Rejected disk-content revision.
    pub revision: String,
    /// Sanitized validation issues.
    pub issues: Vec<FieldIssue>,
}

/// Listener relisten failure payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ListenerRelistenFailedEvent {
    /// `listen` issue with a stable reason and sanitized message.
    pub issue: FieldIssue,
    /// Retained running listener address.
    pub running: String,
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// SHA-256 published runtime revision.
    pub runtime_revision: String,
    /// Publication generation.
    pub generation: u64,
}

/// Stable wire reason for member cooldown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum CooldownReason {
    /// Generic consecutive-failure ladder.
    #[default]
    Failure,
    /// Rate-limit response.
    RateLimit,
    /// Exhausted quota.
    Quota,
    /// Insufficient credit.
    Credit,
    /// Passive quota observation.
    Observed,
    /// Contained member panic.
    Panic,
    /// A newer peer supplied an unknown reason.
    #[serde(other)]
    Unknown,
}

impl CooldownReason {
    /// Stable wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failure => "failure",
            Self::RateLimit => "rate_limit",
            Self::Quota => "quota",
            Self::Credit => "credit",
            Self::Observed => "observed",
            Self::Panic => "panic",
            Self::Unknown => "unknown",
        }
    }
}

/// Member cooldown payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemberCooledEvent {
    /// Stable member id string.
    pub member_id: String,
    /// Stable cooldown reason.
    pub cause: CooldownReason,
    /// Optional upstream retry hint in unsigned milliseconds.
    pub retry_after_ms: Option<u64>,
    /// Optional remaining cooldown in unsigned milliseconds.
    pub remaining_ms: Option<u64>,
    /// Consecutive failure count.
    pub failures: u32,
    /// Upstream status that caused the cooldown, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Upstream words, capped at 300 characters.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Default for MemberCooledEvent {
    fn default() -> Self {
        Self {
            member_id: String::new(),
            cause: CooldownReason::Failure,
            retry_after_ms: None,
            remaining_ms: None,
            failures: 0,
            status: None,
            error: String::new(),
        }
    }
}

/// Member recovery payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MemberRecoveredEvent {
    /// Stable member id string.
    pub member_id: String,
    /// New readiness.
    pub ready: bool,
}

/// Route snapshot publication payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SnapshotPublishedEvent {
    /// Publication generation.
    pub generation: u64,
    /// Number of routes.
    pub routes: u32,
    /// Number of members.
    pub members: u32,
}

/// Request start summary without bodies, headers, queries, or credentials.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestStartedEvent {
    /// Bounded request correlation id.
    pub request_id: String,
    /// Client-facing route id.
    pub route: String,
    /// Inbound protocol label.
    pub inbound_protocol: String,
    /// Unix milliseconds when the request started.
    pub started_at_ms: u64,
}

/// Coarse response status class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum StatusClass {
    /// HTTP 1xx.
    Informational,
    /// HTTP 2xx.
    #[default]
    Success,
    /// HTTP 3xx.
    Redirection,
    /// HTTP 4xx.
    ClientError,
    /// HTTP 5xx.
    ServerError,
    /// A newer peer supplied an unknown class.
    #[serde(other)]
    Unknown,
}

/// Request finish summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestFinishedEvent {
    /// Request correlation id.
    pub request_id: String,
    /// Selected stable member id when known.
    pub member: Option<String>,
    /// Coarse response status class.
    pub status_class: StatusClass,
    /// Unsigned elapsed milliseconds.
    pub duration_ms: u64,
    /// Input tokens when reported.
    pub input_tokens: Option<u64>,
    /// Output tokens when reported.
    pub output_tokens: Option<u64>,
}

impl Default for RequestFinishedEvent {
    fn default() -> Self {
        Self {
            request_id: String::new(),
            member: None,
            status_class: StatusClass::Success,
            duration_ms: 0,
            input_tokens: None,
            output_tokens: None,
        }
    }
}

/// Provider model refresh payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelsRefreshedEvent {
    /// Configured provider id.
    pub provider: String,
    /// Model list source kind label.
    pub list_kind: String,
    /// Number of listed models.
    pub model_count: u64,
    /// Whether refresh succeeded.
    pub ok: bool,
    /// Sanitized failure message.
    pub error: Option<String>,
}

/// Applied agent change payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentChangedEvent {
    /// Agent id.
    pub agent: String,
    /// Applied lifecycle action.
    pub action: AgentAction,
    /// Changed paths.
    pub changed_paths: Vec<String>,
    /// Nonfatal sanitized warnings without token material.
    pub warnings: Vec<String>,
}

impl Default for AgentChangedEvent {
    fn default() -> Self {
        Self {
            agent: String::new(),
            action: AgentAction::Connect,
            changed_paths: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

/// Reserved minimal sign-in progress payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SignInProgressEvent {
    /// Provider or account-owner id.
    pub provider: String,
    /// Opaque sign-in state label.
    pub state: String,
}

/// Token rotation notification without token material.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TokenRotatedEvent {}

/// Shutdown start payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ShutdownStartedEvent {
    /// Whether in-flight work is draining.
    pub draining: bool,
}

/// Reason a topic must be resynchronized.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    /// The client fell behind the broadcast channel.
    #[default]
    Lagged,
    /// The last event id belongs to another daemon boot.
    UnknownBoot,
    /// The requested cursor is no longer retained.
    Unreplayable,
    /// A requested topic is absent from an otherwise valid id.
    MissingTopic,
    /// The composite id is malformed.
    MalformedId,
    /// A newer peer supplied an unknown reason.
    #[serde(other)]
    Unknown,
}

/// Per-topic resynchronization request and current snapshot cursors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ResyncRequiredEvent {
    /// Affected topic.
    pub topic: Topic,
    /// Why replay cannot continue.
    pub reason: ResyncReason,
    /// Current per-topic snapshot cursors.
    pub cursors: Vec<EventCursor>,
}

impl Default for ResyncRequiredEvent {
    fn default() -> Self {
        Self {
            topic: Topic::Control,
            reason: ResyncReason::Lagged,
            cursors: Vec::new(),
        }
    }
}
