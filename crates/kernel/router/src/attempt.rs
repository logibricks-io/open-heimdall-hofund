//! Per-request hooks across routing and encoded upstream attempts.

/// Static facts about one member that a route may attempt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemberAttempt {
    /// Configured provider id used on routes and usage rows.
    pub provider_id: String,
    /// Outbound protocol id.
    pub protocol: String,
    /// Configured upstream base URL, when available.
    pub base_url: String,
    /// Provider id and a one-way credential fingerprint, when available.
    pub route: String,
    /// Whether this member can retain Responses and continue them by id.
    pub keeps_responses: bool,
    /// Whether this member is a subscription account.
    pub subscription: bool,
    /// Whether a plugin owns this member.
    pub plugin: bool,
}

/// One concrete attempt in a dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// Member being called.
    pub member: MemberAttempt,
    /// Client protocol id.
    pub inbound_protocol: String,
}

impl Attempt {
    /// Whether this attempt relays Responses rather than translating them.
    #[must_use]
    pub fn relays_responses(&self) -> bool {
        let responses = hofund_plugin_api::ProtocolId::OPENAI_RESPONSES.as_str();
        self.inbound_protocol == responses && self.member.protocol == responses
    }
}

/// Why an attempt was rejected before calling its member.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AttemptError {
    /// The request names a previous response this member cannot resolve.
    #[error("Previous response with id '{id}' not found.")]
    PreviousResponseNotFound {
        /// Client-supplied response id.
        id: String,
    },
}

/// Per-request callbacks. Defaults preserve ordinary dispatch behavior.
pub trait AttemptHook: Send + Sync {
    /// Checks an attempt before any upstream call.
    fn before(&self, attempt: &Attempt) -> Result<(), AttemptError> {
        let _ = attempt;
        Ok(())
    }

    /// Returns a shortened encoded body, or `None` to send it unchanged.
    fn prepare(&self, attempt: &Attempt, body: &[u8]) -> Option<Vec<u8>> {
        let _ = (attempt, body);
        None
    }

    /// Whether a shortened continuation should be retried as a whole request.
    fn refused(&self, attempt: &Attempt, status: u16, body: &[u8]) -> bool {
        let _ = (attempt, status, body);
        false
    }

    /// Returns a replacement body for one same-member retry after a refusal.
    fn retry(&self, attempt: &Attempt, status: u16, body: &[u8]) -> Option<Vec<u8>> {
        let _ = (attempt, status, body);
        None
    }

    /// Notes the attempt that produced the returned completion.
    fn answered(&self, attempt: &Attempt) {
        let _ = attempt;
    }
}
