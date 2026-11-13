use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, MAX_ID_BYTES};

/// Subscription sign-in flow exposed to clients.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionFlow {
    /// PKCE browser sign-in.
    #[default]
    Pkce,
    /// Device-code sign-in.
    Device,
    /// Pasted token sign-in.
    TokenFile,
    /// Agent bridge sign-in.
    Bridge,
    /// A newer peer supplied an unknown flow.
    #[serde(other)]
    Unknown,
}

/// One stored account, without credential material.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscriptionAccount {
    /// Account id.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Whether this account can serve calls.
    pub ready: bool,
    /// Why another sign-in is required, or `None` when ready.
    pub needs_sign_in: Option<String>,
}

/// One configured subscription and its accounts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscriptionEntry {
    /// Subscription plugin id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Sign-in flow.
    pub flow: SubscriptionFlow,
    /// Stored accounts ordered by label and id.
    pub accounts: Vec<SubscriptionAccount>,
}

/// Subscription list response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscriptionListResponse {
    /// Body wire version.
    pub version: u32,
    /// Configured subscriptions.
    pub subscriptions: Vec<SubscriptionEntry>,
}

impl Default for SubscriptionListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            subscriptions: Vec::new(),
        }
    }
}

/// Starts one subscription sign-in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct SignInStartRequest {
    /// Body wire version.
    pub version: u32,
    /// Optional vendor host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Optional account label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional sign-in site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

impl SignInStartRequest {
    /// Constructs a start request with default plugin options.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: API_VERSION,
            host: None,
            label: None,
            site: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        for value in [&self.host, &self.label, &self.site].into_iter().flatten() {
            validate_id(value)?;
        }
        Ok(())
    }
}

/// A secret-free instruction for completing sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignInChallenge {
    /// Open this authorization URL.
    OpenUrl {
        /// Authorization URL supplied by the plugin.
        url: String,
    },
    /// Enter this code at the verification URL.
    DeviceCode {
        /// User-facing device code.
        user_code: String,
        /// Verification URL.
        verification_url: String,
    },
    /// Follow these instructions and paste token text when asked.
    Instructions {
        /// Plugin-provided, secret-free instructions.
        text: String,
    },
    /// A newer peer supplied an unknown challenge.
    #[serde(other)]
    Unknown,
}

impl Default for SignInChallenge {
    fn default() -> Self {
        Self::Instructions {
            text: String::new(),
        }
    }
}

/// Sign-in start response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SignInStartResponse {
    /// Body wire version.
    pub version: u32,
    /// Opaque session id used by polling, proof, and cancellation.
    pub session: String,
    /// Sign-in flow.
    pub flow: SubscriptionFlow,
    /// Initial user-facing challenge.
    pub challenge: SignInChallenge,
}

impl Default for SignInStartResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            session: String::new(),
            flow: SubscriptionFlow::Pkce,
            challenge: SignInChallenge::default(),
        }
    }
}

/// Current sign-in session state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum SignInSessionState {
    /// The plugin is working or waiting for an external flow.
    #[default]
    Pending,
    /// A pasted callback URL or token can be supplied.
    NeedsProof,
    /// The account is stored.
    Ready,
    /// The flow failed or expired.
    Failed,
    /// The flow was cancelled.
    Cancelled,
    /// A newer peer supplied an unknown state.
    #[serde(other)]
    Unknown,
}

/// Sign-in polling and proof response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SignInStatusResponse {
    /// Body wire version.
    pub version: u32,
    /// Current session state.
    pub state: SignInSessionState,
    /// Current challenge while the session is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge: Option<SignInChallenge>,
}

impl Default for SignInStatusResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            state: SignInSessionState::Pending,
            challenge: None,
        }
    }
}

/// A pasted callback URL or token. Both values are redacted from `Debug`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct SignInProofRequest {
    /// Body wire version.
    pub version: u32,
    /// Pasted loopback callback URL, mutually exclusive with `token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Pasted token text, mutually exclusive with `url`; never a file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl SignInProofRequest {
    /// Constructs a callback URL proof.
    #[must_use]
    pub fn url(url: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            url: Some(url.into()),
            token: None,
        }
    }

    /// Constructs a token-text proof.
    #[must_use]
    pub fn token(token: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            url: None,
            token: Some(token.into()),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        let count = usize::from(self.url.is_some()) + usize::from(self.token.is_some());
        if count != 1
            || self
                .url
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            || self
                .token
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ControlError::new(
                ControlErrorKind::Unprocessable,
                "exactly one nonempty url or token is required",
                "validate",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for SignInProofRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignInProofRequest")
            .field("version", &self.version)
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Versioned sign-in cancellation body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct SignInCancelRequest {
    /// Body wire version.
    pub version: u32,
}

impl SignInCancelRequest {
    /// Constructs a cancellation request.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: API_VERSION,
        }
    }
}

/// Sign-in cancellation response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SignInCancelResponse {
    /// Body wire version.
    pub version: u32,
    /// Always `cancelled` for a known session.
    pub state: SignInSessionState,
}

impl Default for SignInCancelResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            state: SignInSessionState::Cancelled,
        }
    }
}

/// Refreshes one account or every account of a subscription.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct SubscriptionRefreshRequest {
    /// Body wire version.
    pub version: u32,
    /// Account id, or all accounts when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl SubscriptionRefreshRequest {
    /// Constructs a refresh request.
    #[must_use]
    pub fn new(account: Option<String>) -> Self {
        Self {
            version: API_VERSION,
            account,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if let Some(account) = &self.account {
            validate_id(account)?;
        }
        Ok(())
    }
}

/// Refresh response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubscriptionRefreshResponse {
    /// Body wire version.
    pub version: u32,
    /// Refreshed account ids.
    pub refreshed: Vec<String>,
}

impl Default for SubscriptionRefreshResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            refreshed: Vec::new(),
        }
    }
}

/// Switches an agent to another remembered account.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct AccountSwitchRequest {
    /// Body wire version.
    pub version: u32,
    /// Agent id or alias.
    pub agent: String,
    /// `next`, or an account id, email, or label.
    pub target: String,
}

impl AccountSwitchRequest {
    /// Constructs a switch request.
    #[must_use]
    pub fn new(agent: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            agent: agent.into(),
            target: target.into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        validate_id(&self.agent)?;
        validate_id(&self.target)
    }
}

/// Account switch response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountSwitchResponse {
    /// Body wire version.
    pub version: u32,
    /// Agent id or alias from the request.
    pub agent: String,
    /// Selected account id or label.
    pub account: String,
}

impl Default for AccountSwitchResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            agent: String::new(),
            account: String::new(),
        }
    }
}

/// Imports an account export on the daemon host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct AccountImportRequest {
    /// Body wire version.
    pub version: u32,
    /// Export path on the daemon host.
    pub path: String,
}

impl AccountImportRequest {
    /// Constructs an import request.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            path: path.into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if self.path.trim().is_empty() || self.path.len() > MAX_ID_BYTES * 16 {
            return Err(ControlError::new(
                ControlErrorKind::Unprocessable,
                "path must be nonempty and at most 4096 bytes",
                "validate",
            ));
        }
        Ok(())
    }
}

/// One skipped import source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountImportSkip {
    /// Source path.
    pub path: String,
    /// Sanitized reason it was skipped.
    pub reason: String,
}

/// Account import response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountImportResponse {
    /// Body wire version.
    pub version: u32,
    /// Newly imported account ids.
    pub imported: Vec<String>,
    /// Sources the plugins did not import.
    pub skipped: Vec<AccountImportSkip>,
}

impl Default for AccountImportResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            imported: Vec::new(),
            skipped: Vec::new(),
        }
    }
}

/// One discovered agent sign-in without credential material.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiscoveredAccount {
    /// Configured subscription id.
    pub subscription: String,
    /// Display label.
    pub label: String,
    /// Whether this identity is already stored.
    pub already_stored: bool,
}

/// Account discovery response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountDiscoverResponse {
    /// Body wire version.
    pub version: u32,
    /// Discovered sign-ins.
    pub found: Vec<DiscoveredAccount>,
}

impl Default for AccountDiscoverResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            found: Vec::new(),
        }
    }
}

/// Imports every newly discovered account.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct AccountDiscoverAddRequest {
    /// Body wire version.
    pub version: u32,
    /// Must be `true` to confirm the imports.
    pub add: bool,
}

impl AccountDiscoverAddRequest {
    /// Constructs a confirmed discovery import.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: API_VERSION,
            add: true,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if !self.add {
            return Err(ControlError::new(
                ControlErrorKind::Unprocessable,
                "add must be true",
                "validate",
            ));
        }
        Ok(())
    }
}

/// Discovery import response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountDiscoverImportResponse {
    /// Body wire version.
    pub version: u32,
    /// Newly imported account ids.
    pub imported: Vec<String>,
}

impl Default for AccountDiscoverImportResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            imported: Vec::new(),
        }
    }
}

/// Versioned account-removal body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct AccountRemoveRequest {
    /// Body wire version.
    pub version: u32,
}

impl AccountRemoveRequest {
    /// Constructs an account-removal request.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: API_VERSION,
        }
    }
}

/// Account removal response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountRemoveResponse {
    /// Body wire version.
    pub version: u32,
    /// Whether the account was removed.
    pub removed: bool,
}

impl Default for AccountRemoveResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            removed: false,
        }
    }
}

fn validate_id(value: &str) -> Result<(), ControlError> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err(ControlError::new(
            ControlErrorKind::Unprocessable,
            "an id must be nonempty and at most 256 bytes",
            "validate",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Request, decode_request};

    fn invalid<T: Request + serde::Serialize + std::fmt::Debug>(request: &T) {
        let body = serde_json::to_vec(request).expect("request JSON");
        let error = decode_request::<T>(API_VERSION, &body).expect_err("invalid request");
        assert_eq!(error.kind, ControlErrorKind::Unprocessable);
    }

    #[test]
    fn sign_in_start_validates_every_option() {
        let valid = SignInStartRequest {
            version: API_VERSION,
            host: Some("example.test".to_owned()),
            label: Some("work".to_owned()),
            site: Some("device".to_owned()),
        };
        let body = serde_json::to_vec(&valid).expect("JSON");
        decode_request::<SignInStartRequest>(API_VERSION, &body).expect("valid options");
        let boundary = SignInStartRequest {
            host: Some("x".repeat(MAX_ID_BYTES)),
            ..valid
        };
        let body = serde_json::to_vec(&boundary).expect("JSON");
        decode_request::<SignInStartRequest>(API_VERSION, &body).expect("maximum id length");
        for (host, label, site) in [
            (Some(String::new()), None, None),
            (None, Some(String::new()), None),
            (None, None, Some(String::new())),
            (Some("x".repeat(MAX_ID_BYTES + 1)), None, None),
        ] {
            invalid(&SignInStartRequest {
                version: API_VERSION,
                host,
                label,
                site,
            });
        }
    }

    #[test]
    fn proof_constructors_and_validation_require_one_nonempty_secret_input() {
        let url = SignInProofRequest::url("http://localhost/callback?code=fake&state=right");
        assert!(url.url.is_some());
        let body = serde_json::to_vec(&url).expect("JSON");
        decode_request::<SignInProofRequest>(API_VERSION, &body).expect("URL proof");
        let token = SignInProofRequest::token("fake-token");
        assert!(token.token.is_some());
        let body = serde_json::to_vec(&token).expect("JSON");
        decode_request::<SignInProofRequest>(API_VERSION, &body).expect("token proof");

        let mut neither = SignInProofRequest::url("http://localhost/callback");
        neither.url = None;
        invalid(&neither);
        let mut both = SignInProofRequest::url("http://localhost/callback");
        both.token = Some("fake-token".to_owned());
        invalid(&both);
        invalid(&SignInProofRequest::url("  "));
        invalid(&SignInProofRequest::token("\n"));
    }

    #[test]
    fn refresh_switch_import_and_discovery_validate_their_fields() {
        invalid(&SubscriptionRefreshRequest::new(Some(String::new())));
        invalid(&SubscriptionRefreshRequest::new(Some(
            "x".repeat(MAX_ID_BYTES + 1),
        )));
        let body = serde_json::to_vec(&SubscriptionRefreshRequest::new(None)).expect("JSON");
        decode_request::<SubscriptionRefreshRequest>(API_VERSION, &body).expect("all accounts");
        let body = serde_json::to_vec(&SubscriptionRefreshRequest::new(Some(
            "x".repeat(MAX_ID_BYTES),
        )))
        .expect("JSON");
        decode_request::<SubscriptionRefreshRequest>(API_VERSION, &body)
            .expect("maximum account id length");

        invalid(&AccountSwitchRequest::new("", "next"));
        invalid(&AccountSwitchRequest::new(
            "agent",
            "x".repeat(MAX_ID_BYTES + 1),
        ));
        let body = serde_json::to_vec(&AccountSwitchRequest::new("agent", "next")).expect("JSON");
        decode_request::<AccountSwitchRequest>(API_VERSION, &body).expect("switch");

        invalid(&AccountImportRequest::new("  "));
        invalid(&AccountImportRequest::new(
            "x".repeat(MAX_ID_BYTES * 16 + 1),
        ));
        let body = serde_json::to_vec(&AccountImportRequest::new("export.json")).expect("JSON");
        decode_request::<AccountImportRequest>(API_VERSION, &body).expect("import");
        let body = serde_json::to_vec(&AccountImportRequest::new("x".repeat(MAX_ID_BYTES * 16)))
            .expect("JSON");
        decode_request::<AccountImportRequest>(API_VERSION, &body)
            .expect("maximum import path length");

        let mut add = AccountDiscoverAddRequest::new();
        add.add = false;
        invalid(&add);
        let body = serde_json::to_vec(&AccountDiscoverAddRequest::new()).expect("JSON");
        decode_request::<AccountDiscoverAddRequest>(API_VERSION, &body).expect("confirmed add");

        assert_eq!(SignInCancelRequest::new().version, API_VERSION);
        assert_eq!(AccountRemoveRequest::new().version, API_VERSION);
    }
}
