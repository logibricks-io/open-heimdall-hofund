//! Key balance readers implemented by provider plugins.

use std::time::SystemTime;

use async_trait::async_trait;

use crate::{HostContext, PlanWindow, PluginError};

/// A provider key, endpoints, and optional balance settings.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BalanceQuery {
    bases: Vec<String>,
    api_key: String,
    balance_url: String,
    balance_path: String,
    balance_token: String,
    balance_method: String,
    headers: Vec<(String, String)>,
    now: SystemTime,
}

impl BalanceQuery {
    /// A query over candidate base URLs and the key that signs balance reads.
    #[must_use]
    pub fn new(bases: Vec<String>, api_key: impl Into<String>) -> Self {
        Self {
            bases,
            api_key: api_key.into(),
            balance_url: String::new(),
            balance_path: String::new(),
            balance_token: String::new(),
            balance_method: String::new(),
            headers: Vec::new(),
            now: SystemTime::now(),
        }
    }

    /// Overrides balance endpoint settings from provider config.
    #[must_use]
    pub fn with_balance_settings(
        mut self,
        url: impl Into<String>,
        path: impl Into<String>,
        token: impl Into<String>,
        method: impl Into<String>,
    ) -> Self {
        self.balance_url = url.into();
        self.balance_path = path.into();
        self.balance_token = token.into();
        self.balance_method = method.into();
        self
    }

    /// Extra headers copied onto balance requests.
    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Reading clock.
    #[must_use]
    pub const fn with_now(mut self, now: SystemTime) -> Self {
        self.now = now;
        self
    }

    /// Candidate inference base URLs.
    #[must_use]
    pub fn bases(&self) -> &[String] {
        &self.bases
    }

    /// Inference API key. Callers must never log it.
    #[must_use]
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Custom balance URL, or empty for a host-known endpoint.
    #[must_use]
    pub fn balance_url(&self) -> &str {
        &self.balance_url
    }

    /// Field or expression to read from a custom reply.
    #[must_use]
    pub fn balance_path(&self) -> &str {
        &self.balance_path
    }

    /// Account token for a custom balance URL. Callers must never log it.
    #[must_use]
    pub fn balance_token(&self) -> &str {
        &self.balance_token
    }

    /// GET or POST for a custom balance URL.
    #[must_use]
    pub fn balance_method(&self) -> &str {
        &self.balance_method
    }

    /// Provider headers with key placeholders applied by the reader.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Reading clock.
    #[must_use]
    pub const fn now(&self) -> SystemTime {
        self.now
    }
}

/// Whether a provider is known to publish a key balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BalanceSupport {
    /// This provider has no balance reader.
    Unsupported,
    /// A failed read still deserves an error card.
    Certain,
    /// A failed read deserves a card only after a successful read established the balance.
    Possible,
}

/// One formatted amount on a balance card.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct BalancePart {
    label: String,
    text: String,
    percent: Option<f64>,
    error: String,
}

impl BalancePart {
    /// A part with display text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// Sets the part label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Sets a percent when the part is a share reading.
    #[must_use]
    pub const fn with_percent(mut self, percent: f64) -> Self {
        self.percent = Some(percent);
        self
    }

    /// Sets a read failure for this part alone.
    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = error.into();
        self
    }

    /// Part label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Formatted amount.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Percent represented, when present.
    #[must_use]
    pub const fn percent(&self) -> Option<f64> {
        self.percent
    }

    /// Part-specific failure text.
    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }
}

/// A numeric wallet amount for routing.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct BalanceWallet {
    name: String,
    amount: f64,
    unit: String,
}

impl BalanceWallet {
    /// A wallet with amount and unit.
    #[must_use]
    pub fn new(amount: f64, unit: impl Into<String>) -> Self {
        let unit = unit.into();
        Self {
            name: unit.clone(),
            amount,
            unit,
        }
    }

    /// Sets the wallet label.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Wallet label.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Amount left.
    #[must_use]
    pub const fn amount(&self) -> f64 {
        self.amount
    }

    /// Currency or provider unit.
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }
}

/// One balance card for a key.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct BalanceCard {
    text: String,
    parts: Vec<BalancePart>,
    split: Vec<BalancePart>,
    wallets: Vec<BalanceWallet>,
    windows: Vec<PlanWindow>,
    plan: String,
    plan_windows: Vec<PlanWindow>,
    until: Option<SystemTime>,
    renew: String,
    account_wide: bool,
    error: String,
}

impl BalanceCard {
    /// An empty card.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets display text.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    /// Sets custom expression parts.
    #[must_use]
    pub fn with_parts(mut self, parts: Vec<BalancePart>) -> Self {
        self.parts = parts;
        self
    }

    /// Sets granted versus topped-up parts.
    #[must_use]
    pub fn with_split(mut self, split: Vec<BalancePart>) -> Self {
        self.split = split;
        self
    }

    /// Sets routing wallets.
    #[must_use]
    pub fn with_wallets(mut self, wallets: Vec<BalanceWallet>) -> Self {
        self.wallets = wallets;
        self
    }

    /// Sets key-specific windows on the balance card.
    #[must_use]
    pub fn with_windows(mut self, windows: Vec<PlanWindow>) -> Self {
        self.windows = windows;
        self
    }

    /// Sets a bought plan carried on the same endpoint.
    #[must_use]
    pub fn with_plan(
        mut self,
        plan: impl Into<String>,
        windows: Vec<PlanWindow>,
        until: Option<SystemTime>,
        renew: impl Into<String>,
    ) -> Self {
        self.plan = plan.into();
        self.plan_windows = windows;
        self.until = until;
        self.renew = renew.into();
        self
    }

    /// Sets whether every key of one account sees this balance.
    #[must_use]
    pub const fn with_account_wide(mut self, account_wide: bool) -> Self {
        self.account_wide = account_wide;
        self
    }

    /// Sets a card-specific read failure.
    #[must_use]
    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = error.into();
        self
    }

    /// Display text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Custom expression parts.
    #[must_use]
    pub fn parts(&self) -> &[BalancePart] {
        &self.parts
    }

    /// Granted and topped-up components.
    #[must_use]
    pub fn split(&self) -> &[BalancePart] {
        &self.split
    }

    /// Routing wallets.
    #[must_use]
    pub fn wallets(&self) -> &[BalanceWallet] {
        &self.wallets
    }

    /// Key-specific windows.
    #[must_use]
    pub fn windows(&self) -> &[PlanWindow] {
        &self.windows
    }

    /// Plan name when the endpoint carries one.
    #[must_use]
    pub fn plan(&self) -> &str {
        &self.plan
    }

    /// Plan windows when the endpoint carries a plan.
    #[must_use]
    pub fn plan_windows(&self) -> &[PlanWindow] {
        &self.plan_windows
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

    /// Whether the balance is account-wide.
    #[must_use]
    pub const fn account_wide(&self) -> bool {
        self.account_wide
    }

    /// Card-specific failure.
    #[must_use]
    pub fn error(&self) -> &str {
        &self.error
    }

    /// Builds a card from plugin reader parts.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn pack(
        text: impl Into<String>,
        parts: Vec<BalancePart>,
        split: Vec<BalancePart>,
        wallets: Vec<BalanceWallet>,
        windows: Vec<PlanWindow>,
        plan: impl Into<String>,
        plan_windows: Vec<PlanWindow>,
        until: Option<SystemTime>,
        renew: impl Into<String>,
        account_wide: bool,
    ) -> Self {
        Self {
            text: text.into(),
            parts,
            split,
            wallets,
            windows,
            plan: plan.into(),
            plan_windows,
            until,
            renew: renew.into(),
            account_wide,
            error: String::new(),
        }
    }
}

/// A vendor balance reader. Vendor response shapes stay in provider plugins.
#[async_trait]
pub trait BalanceQuota: Send + Sync + 'static {
    /// Whether `query` selects a balance source and how certain it is.
    fn support(&self, query: &BalanceQuery) -> BalanceSupport;

    /// Reads the balance card for `query`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the vendor cannot be reached, refuses the
    /// credential, or answers a shape the plugin cannot read.
    async fn read(
        &self,
        query: &BalanceQuery,
        host: &HostContext,
    ) -> Result<Option<BalanceCard>, PluginError>;
}
