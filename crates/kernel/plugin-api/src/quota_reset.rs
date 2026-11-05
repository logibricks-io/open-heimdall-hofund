//! Rate-limit reset credits a subscription account may spend by hand.

use std::time::SystemTime;

/// One reset credit still available on the account.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaResetCard {
    id: String,
    expires_at: Option<SystemTime>,
}

impl QuotaResetCard {
    /// A reset named by vendor `id`.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            expires_at: None,
        }
    }

    /// When this reset credit expires, when the vendor says so.
    #[must_use]
    pub const fn with_expires_at(mut self, at: Option<SystemTime>) -> Self {
        self.expires_at = at;
        self
    }

    /// Vendor credit id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Expiration time, when known.
    #[must_use]
    pub const fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at
    }
}

/// Reset credits an account holds.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaResetCredits {
    count: u32,
    until: Option<SystemTime>,
    until_unread: bool,
    each: Vec<QuotaResetCard>,
}

impl QuotaResetCredits {
    /// `count` resets from the usage page.
    #[must_use]
    pub const fn new(count: u32) -> Self {
        Self {
            count,
            until: None,
            until_unread: false,
            each: Vec::new(),
        }
    }

    /// When the soonest listed credit expires.
    #[must_use]
    pub const fn with_until(mut self, at: Option<SystemTime>) -> Self {
        self.until = at;
        self
    }

    /// The details endpoint failed; a missing [`Self::until`] is unknown, not never.
    #[must_use]
    pub const fn with_until_unread(mut self, unread: bool) -> Self {
        self.until_unread = unread;
        self
    }

    /// Every credit still available, soonest first.
    #[must_use]
    pub fn with_each(mut self, each: Vec<QuotaResetCard>) -> Self {
        self.each = each;
        self
    }

    /// How many resets the usage page says are available.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// When the soonest credit expires, when known.
    #[must_use]
    pub const fn until(&self) -> Option<SystemTime> {
        self.until
    }

    /// Whether [`Self::until`] could not be read.
    #[must_use]
    pub const fn until_unread(&self) -> bool {
        self.until_unread
    }

    /// Listed credits, when the vendor returned them.
    #[must_use]
    pub fn each(&self) -> &[QuotaResetCard] {
        &self.each
    }
}

/// What spending one reset credit did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaResetOutcome {
    code: String,
    windows: u32,
}

impl QuotaResetOutcome {
    /// Vendor outcome code (`reset`, `no_credit`, `nothing_to_reset`, …).
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            windows: 0,
        }
    }

    /// How many windows started again when `code` is `reset`.
    #[must_use]
    pub const fn with_windows(mut self, windows: u32) -> Self {
        self.windows = windows;
        self
    }

    /// Vendor code word.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Windows restarted on success.
    #[must_use]
    pub const fn windows(&self) -> u32 {
        self.windows
    }
}
