//! Daily warm-up status and the subscription hook that runs one.

use std::time::SystemTime;

use crate::id::AccountId;

/// Whether a warm-up ran, was skipped, or failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WarmupOutcome {
    /// The vendor request completed.
    Ok,
    /// The scheduler chose not to send (window running, already started, and so on).
    Skipped,
    /// The vendor request failed or the plugin refused.
    Failed,
}

/// One finished warm-up attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WarmupRecord {
    at: SystemTime,
    outcome: WarmupOutcome,
    detail: String,
}

impl WarmupRecord {
    /// Builds a record. `detail` is a short English reason, never secret material.
    #[must_use]
    pub fn new(at: SystemTime, outcome: WarmupOutcome, detail: impl Into<String>) -> Self {
        Self {
            at,
            outcome,
            detail: detail.into(),
        }
    }

    /// When the attempt finished.
    #[must_use]
    pub const fn at(&self) -> SystemTime {
        self.at
    }

    /// How the attempt ended.
    #[must_use]
    pub const fn outcome(&self) -> WarmupOutcome {
        self.outcome
    }

    /// Why it was skipped or failed, or empty on success.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Last warm-up state for one subscription account.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WarmupAccountStatus {
    subscription: String,
    account: AccountId,
    last: Option<WarmupRecord>,
}

impl WarmupAccountStatus {
    /// One row for the control API and CLI.
    #[must_use]
    pub fn new(subscription: impl Into<String>, account: AccountId) -> Self {
        Self {
            subscription: subscription.into(),
            account,
            last: None,
        }
    }

    /// The configured subscription id.
    #[must_use]
    pub fn subscription(&self) -> &str {
        &self.subscription
    }

    /// The account id.
    #[must_use]
    pub fn account(&self) -> &AccountId {
        &self.account
    }

    /// The most recent attempt, if any.
    #[must_use]
    pub fn last(&self) -> Option<&WarmupRecord> {
        self.last.as_ref()
    }

    /// Replaces the last attempt.
    pub fn set_last(&mut self, record: WarmupRecord) {
        self.last = Some(record);
    }
}
