//! Daily check-in results and status rows for subscription accounts.

use std::time::SystemTime;

use crate::id::AccountId;

/// How a vendor check-in ended, matching the plugin hook vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckinOutcome {
    /// Credits were claimed during this attempt.
    Claimed,
    /// Already checked in or nothing left to claim.
    Done,
    /// The account or device cannot participate.
    Ineligible,
    /// Check-in is off or not running for this account.
    Inactive,
    /// The vendor wants verification the gateway cannot complete.
    Captcha,
    /// The attempt failed and may be retried on a later day.
    Failed,
}

impl CheckinOutcome {
    /// Stable wire and log name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Done => "done",
            Self::Ineligible => "ineligible",
            Self::Inactive => "inactive",
            Self::Captcha => "captcha",
            Self::Failed => "failed",
        }
    }

    /// Whether a successful-enough attempt counts for the local calendar day.
    #[must_use]
    pub const fn settles_day(self) -> bool {
        !matches!(self, Self::Failed)
    }
}

/// One finished check-in attempt from a subscription plugin.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct CheckinResult {
    outcome: CheckinOutcome,
    credit: f64,
    streak: i32,
    message: String,
    recheck_at: Option<SystemTime>,
}

impl CheckinResult {
    /// Builds a result. `message` is a short English reason, never secret material.
    #[must_use]
    pub fn new(outcome: CheckinOutcome, message: impl Into<String>) -> Self {
        Self {
            outcome,
            credit: 0.0,
            streak: 0,
            message: message.into(),
            recheck_at: None,
        }
    }

    /// Adds credit and streak fields when the vendor reports them.
    #[must_use]
    pub fn with_rewards(mut self, credit: f64, streak: i32) -> Self {
        self.credit = credit;
        self.streak = streak;
        self
    }

    /// Asks the scheduler to run this account again at `recheck_at` within the same day.
    #[must_use]
    pub const fn with_recheck_at(mut self, recheck_at: SystemTime) -> Self {
        self.recheck_at = Some(recheck_at);
        self
    }

    /// How the vendor answered.
    #[must_use]
    pub const fn outcome(&self) -> CheckinOutcome {
        self.outcome
    }

    /// Credits reported for the day, if any.
    #[must_use]
    pub const fn credit(&self) -> f64 {
        self.credit
    }

    /// Streak day count when the vendor reports one.
    #[must_use]
    pub const fn streak(&self) -> i32 {
        self.streak
    }

    /// Detail for operators, or empty on a quiet success.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// When this account should be checked again, if the vendor requested it.
    #[must_use]
    pub const fn recheck_at(&self) -> Option<SystemTime> {
        self.recheck_at
    }
}

/// One stored check-in attempt for the control API and CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckinRecord {
    at: SystemTime,
    outcome: CheckinOutcome,
    detail: String,
}

impl CheckinRecord {
    /// Builds a record from a plugin result.
    #[must_use]
    pub fn from_result(at: SystemTime, result: &CheckinResult) -> Self {
        let detail = if result.message().is_empty() {
            String::new()
        } else {
            result.message().to_owned()
        };
        Self {
            at,
            outcome: result.outcome(),
            detail,
        }
    }

    /// Builds a record with an explicit detail string.
    #[must_use]
    pub fn new(at: SystemTime, outcome: CheckinOutcome, detail: impl Into<String>) -> Self {
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
    pub const fn outcome(&self) -> CheckinOutcome {
        self.outcome
    }

    /// Why it failed or was ineligible, or empty on a quiet success.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Last check-in state for one subscription account.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CheckinAccountStatus {
    subscription: String,
    account: AccountId,
    last: Option<CheckinRecord>,
}

impl CheckinAccountStatus {
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
    pub fn last(&self) -> Option<&CheckinRecord> {
        self.last.as_ref()
    }

    /// Replaces the last attempt.
    pub fn set_last(&mut self, record: CheckinRecord) {
        self.last = Some(record);
    }
}
