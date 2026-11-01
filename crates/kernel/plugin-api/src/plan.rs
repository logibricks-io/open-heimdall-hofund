//! Plan quota readers implemented by provider plugins.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::{HostContext, PluginError};

/// A provider key and the endpoints whose plan it uses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlanQuery {
    bases: Vec<String>,
    api_key: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    organization: String,
    project: String,
    plan_url: String,
    now: SystemTime,
}

impl PlanQuery {
    /// A query over the candidate base URLs and the key that signs key-readable plans.
    #[must_use]
    pub fn new(bases: Vec<String>, api_key: impl Into<String>) -> Self {
        Self {
            bases,
            api_key: api_key.into(),
            access_key_id: String::new(),
            secret_access_key: String::new(),
            session_token: String::new(),
            organization: String::new(),
            project: String::new(),
            plan_url: String::new(),
            now: SystemTime::now(),
        }
    }

    /// Supplies the account access key used by control-plane plan readers.
    #[must_use]
    pub fn with_access_key(mut self, id: impl Into<String>, secret: impl Into<String>) -> Self {
        self.access_key_id = id.into();
        self.secret_access_key = secret.into();
        self
    }

    /// Supplies a platform session for a plan that is not readable by its inference key.
    #[must_use]
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = token.into();
        self
    }

    /// Supplies optional team identifiers for a key's team-plan fallback.
    #[must_use]
    pub fn with_team(
        mut self,
        organization: impl Into<String>,
        project: impl Into<String>,
    ) -> Self {
        self.organization = organization.into();
        self.project = project.into();
        self
    }

    /// Overrides the plan-reading endpoint without changing inference endpoints.
    #[must_use]
    pub fn with_plan_url(mut self, url: impl Into<String>) -> Self {
        self.plan_url = url.into();
        self
    }

    /// Sets the reading and signing clock.
    #[must_use]
    pub const fn with_now(mut self, now: SystemTime) -> Self {
        self.now = now;
        self
    }

    /// Candidate Chat, Messages, and Responses base URLs.
    #[must_use]
    pub fn bases(&self) -> &[String] {
        &self.bases
    }

    /// Inference API key. Callers must never log it.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Account access-key id, when configured.
    #[must_use]
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// Account access-key secret, when configured. Callers must never log it.
    #[must_use]
    pub fn secret_access_key(&self) -> &str {
        &self.secret_access_key
    }

    /// Platform session token, when configured. Callers must never log it.
    #[must_use]
    pub fn session_token(&self) -> &str {
        &self.session_token
    }

    /// Optional team organization id.
    #[must_use]
    pub fn organization(&self) -> &str {
        &self.organization
    }

    /// Optional team project id.
    #[must_use]
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Plan-reading endpoint override, or empty for the vendor default.
    #[must_use]
    pub fn plan_url(&self) -> &str {
        &self.plan_url
    }

    /// Reading and signing clock.
    #[must_use]
    pub const fn now(&self) -> SystemTime {
        self.now
    }
}

/// Whether a provider is known to sell a plan whose windows can be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlanSupport {
    /// This provider has no plan reader.
    Unsupported,
    /// A failed read still deserves an error card.
    Certain,
    /// A failed read deserves a card only after a successful read established the plan.
    Possible,
}

/// One allowance window reported by a plan.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PlanWindow {
    name: String,
    used: f64,
    resets_at: Option<SystemTime>,
    span: Option<Duration>,
    amount: f64,
    limit: f64,
    unit: String,
    blocking: bool,
    display: String,
}

impl PlanWindow {
    /// A window with `used` percent consumed.
    #[must_use]
    pub fn new(name: impl Into<String>, used: f64) -> Self {
        Self {
            name: name.into(),
            used,
            resets_at: None,
            span: None,
            amount: 0.0,
            limit: 0.0,
            unit: String::new(),
            blocking: true,
            display: String::new(),
        }
    }

    /// Sets the absolute reset time.
    #[must_use]
    pub const fn with_resets_at(mut self, at: SystemTime) -> Self {
        self.resets_at = Some(at);
        self
    }

    /// Sets an optional absolute reset time.
    #[must_use]
    pub const fn with_optional_reset(mut self, at: Option<SystemTime>) -> Self {
        self.resets_at = at;
        self
    }

    /// Sets the window length used by routing and history.
    #[must_use]
    pub const fn with_span(mut self, span: Duration) -> Self {
        self.span = Some(span);
        self
    }

    /// Sets the used amount, cap, and unit reported by the vendor.
    #[must_use]
    pub fn with_count(mut self, amount: f64, limit: f64, unit: impl Into<String>) -> Self {
        self.amount = amount;
        self.limit = limit;
        self.unit = unit.into();
        self
    }

    /// Sets whether exhaustion blocks routing.
    #[must_use]
    pub const fn with_blocking(mut self, blocking: bool) -> Self {
        self.blocking = blocking;
        self
    }

    /// Sets the vendor's own count text.
    #[must_use]
    pub fn with_display(mut self, display: impl Into<String>) -> Self {
        self.display = display.into();
        self
    }

    /// Window name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Percent used.
    #[must_use]
    pub const fn used(&self) -> f64 {
        self.used
    }

    /// Absolute reset time.
    #[must_use]
    pub const fn resets_at(&self) -> Option<SystemTime> {
        self.resets_at
    }

    /// Window length.
    #[must_use]
    pub const fn span(&self) -> Option<Duration> {
        self.span
    }

    /// Amount used in the vendor's unit.
    #[must_use]
    pub const fn amount(&self) -> f64 {
        self.amount
    }

    /// Cap in the vendor's unit.
    #[must_use]
    pub const fn limit(&self) -> f64 {
        self.limit
    }

    /// Vendor's unit.
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// Whether exhaustion blocks routing.
    #[must_use]
    pub const fn blocking(&self) -> bool {
        self.blocking
    }

    /// Vendor's own count text.
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }
}

/// One plan card. A provider account with several team plans can return several cards.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PlanCard {
    plan: String,
    user: String,
    windows: Vec<PlanWindow>,
    until: Option<SystemTime>,
    renew: String,
    error: String,
}

impl PlanCard {
    /// A card named `plan`.
    #[must_use]
    pub fn new(plan: impl Into<String>) -> Self {
        Self {
            plan: plan.into(),
            user: String::new(),
            windows: Vec::new(),
            until: None,
            renew: String::new(),
            error: String::new(),
        }
    }

    /// Sets a label that distinguishes plans or keys.
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    /// Sets the plan's windows.
    #[must_use]
    pub fn with_windows(mut self, windows: Vec<PlanWindow>) -> Self {
        self.windows = windows;
        self
    }

    /// Sets when the paid plan ends and its renewal mode (`auto`, `off`, or empty).
    #[must_use]
    pub fn with_term(mut self, until: SystemTime, renew: impl Into<String>) -> Self {
        self.until = Some(until);
        self.renew = renew.into();
        self
    }

    /// Sets a card-specific read failure.
    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = error.into();
        self
    }

    /// Plan label.
    #[must_use]
    pub fn plan(&self) -> &str {
        &self.plan
    }

    /// Plan or key label.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Reported windows.
    #[must_use]
    pub fn windows(&self) -> &[PlanWindow] {
        &self.windows
    }

    /// Paid plan end.
    #[must_use]
    pub const fn until(&self) -> Option<SystemTime> {
        self.until
    }

    /// Renewal mode.
    #[must_use]
    pub fn renew(&self) -> &str {
        &self.renew
    }

    /// Card-specific read failure.
    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }
}

/// A vendor plan reader. Vendor response shapes and signing stay in provider plugins.
#[async_trait]
pub trait PlanQuota: Send + Sync + 'static {
    /// Whether `query` selects a plan source and how certain its plan is.
    fn support(&self, query: &PlanQuery) -> PlanSupport;

    /// Reads the plan cards for `query`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the vendor cannot be reached, refuses the
    /// credential, or answers a shape the plugin cannot read.
    async fn read(
        &self,
        query: &PlanQuery,
        host: &HostContext,
    ) -> Result<Vec<PlanCard>, PluginError>;
}
