use serde::{Deserialize, Serialize};

use crate::{
    API_VERSION, ControlError, ControlErrorKind, FieldIssue, MAX_ID_BYTES, validate_revision,
};

/// Revision-checked partial update of terminal UI settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct SettingsPatchRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 disk-content revision.
    pub expected_revision: String,
    /// New model listen address, applied after restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// New usage display currency: `usd` or `cny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Whether quota bars show remaining rather than used percentages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_left: Option<bool>,
    /// Whether agent sign-in discovery is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discover_accounts: Option<bool>,
    /// Stored LAN allow-list. Enforcement is a separate feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_allow: Option<Vec<String>>,
}

impl SettingsPatchRequest {
    /// Constructs an empty settings patch against `expected_revision`.
    #[must_use]
    pub fn new(expected_revision: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            expected_revision: expected_revision.into(),
            listen: None,
            currency: None,
            quota_left: None,
            discover_accounts: None,
            lan_allow: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)?;
        if let Some(currency) = &self.currency
            && !matches!(currency.as_str(), "usd" | "cny")
        {
            return Err(issue("currency", "currency must be usd or cny"));
        }
        if let Some(listen) = &self.listen
            && listen.parse::<std::net::SocketAddr>().is_err()
        {
            return Err(issue("listen", "listen must be an ip:port address"));
        }
        if self
            .lan_allow
            .iter()
            .flatten()
            .any(|entry| entry.len() > MAX_ID_BYTES)
        {
            return Err(issue(
                "lan_allow",
                "a LAN allow-list entry exceeds the 256-byte limit",
            ));
        }
        Ok(())
    }
}

fn issue(field: &str, message: &str) -> ControlError {
    let mut error = ControlError::new(ControlErrorKind::Unprocessable, message, "validate");
    error
        .details
        .push(FieldIssue::new(field, "invalid", message));
    error
}

/// Current terminal UI settings and the config revision they came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SettingsResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// Configured model listen address.
    pub listen: String,
    /// Usage display currency: `usd` or `cny`.
    pub currency: String,
    /// Whether quota bars show remaining rather than used percentages.
    pub quota_left: bool,
    /// Whether agent sign-in discovery is enabled.
    pub discover_accounts: bool,
    /// Stored LAN allow-list. It is not enforced by this API.
    pub lan_allow: Vec<String>,
    /// Whether the configured listen address differs from the one in use.
    pub pending_restart: bool,
}

impl Default for SettingsResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            disk_revision: String::new(),
            listen: String::new(),
            currency: "usd".to_owned(),
            quota_left: false,
            discover_accounts: false,
            lan_allow: Vec::new(),
            pending_restart: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode_request;

    fn valid() -> SettingsPatchRequest {
        let mut request = SettingsPatchRequest::new("r".repeat(64));
        request.listen = Some("127.0.0.1:3425".to_owned());
        request.currency = Some("usd".to_owned());
        request.lan_allow = Some(vec!["192.168.1.0/24".to_owned(), String::new()]);
        request
    }

    #[test]
    fn settings_patch_validates_revision_currency_listen_and_lan_entries() {
        let body = serde_json::to_vec(&valid()).expect("JSON");
        decode_request::<SettingsPatchRequest>(API_VERSION, &body).expect("valid patch");
        let mut cny = valid();
        cny.currency = Some("cny".to_owned());
        let body = serde_json::to_vec(&cny).expect("JSON");
        decode_request::<SettingsPatchRequest>(API_VERSION, &body).expect("cny");
        let mut boundary = valid();
        boundary.expected_revision = "r".repeat(MAX_ID_BYTES);
        boundary.lan_allow = Some(vec!["x".repeat(MAX_ID_BYTES)]);
        let body = serde_json::to_vec(&boundary).expect("JSON");
        decode_request::<SettingsPatchRequest>(API_VERSION, &body).expect("maximum lengths");

        for (field, value) in [
            ("revision", "x".repeat(MAX_ID_BYTES + 1)),
            ("currency", "eur".to_owned()),
            ("listen", "localhost:3425".to_owned()),
            ("lan", "x".repeat(MAX_ID_BYTES + 1)),
        ] {
            let mut request = valid();
            match field {
                "revision" => request.expected_revision = value,
                "currency" => request.currency = Some(value),
                "listen" => request.listen = Some(value),
                _ => request.lan_allow = Some(vec![value]),
            }
            let body = serde_json::to_vec(&request).expect("JSON");
            let error = decode_request::<SettingsPatchRequest>(API_VERSION, &body)
                .expect_err("invalid settings patch");
            assert_eq!(
                error.kind,
                ControlErrorKind::Unprocessable,
                "{field}: {}",
                error.message
            );
        }
    }
}
