use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, MAX_ID_BYTES};

/// Manual check-in trigger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct CheckinRunRequest {
    /// Body wire version.
    pub version: u32,
    /// Configured subscription id.
    pub subscription: String,
    /// Account id on that subscription.
    pub account: String,
}

impl CheckinRunRequest {
    /// Constructs a version-current run request.
    #[must_use]
    pub fn new(subscription: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            subscription: subscription.into(),
            account: account.into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        for (field, value) in [
            ("subscription", &self.subscription),
            ("account", &self.account),
        ] {
            if value.is_empty() || value.len() > MAX_ID_BYTES {
                return Err(ControlError::new(
                    ControlErrorKind::Unprocessable,
                    format!("{field} is missing or too long"),
                    "validate",
                ));
            }
        }
        Ok(())
    }
}

/// One account's last check-in row on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub struct CheckinAccountRow {
    /// Configured subscription id.
    pub subscription: String,
    /// Account id.
    pub account: String,
    /// Vendor outcome name, or empty when never run.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub outcome: String,
    /// Detail when failed or ineligible.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Unix milliseconds when the last attempt finished, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
}

/// Listed check-in status for every account the daemon has seen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CheckinListResponse {
    /// Body wire version.
    pub version: u32,
    /// Rows in subscription then account order.
    pub accounts: Vec<CheckinAccountRow>,
}

impl Default for CheckinListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            accounts: Vec::new(),
        }
    }
}

/// Result of one manual check-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "camelCase")]
pub struct CheckinRunResponse {
    /// Body wire version.
    pub version: u32,
    /// Vendor outcome name.
    pub outcome: String,
    /// Detail when failed or ineligible.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Unix milliseconds when the attempt finished.
    pub at_ms: u64,
}

impl Default for CheckinRunResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            outcome: String::new(),
            detail: String::new(),
            at_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CheckinRunRequest;

    #[test]
    fn a3b_11_checkin_run_request_validates_ids() {
        assert!(CheckinRunRequest::new("", "a@x.io").validate().is_err());
        assert!(CheckinRunRequest::new("acct", "").validate().is_err());
        assert!(CheckinRunRequest::new("acct", "a@x.io").validate().is_ok());
    }
}
