use std::fmt;

use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Named-key management action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum GatewayKeyAction {
    /// Add a key with a generated or supplied secret.
    Add,
    /// Rotate a key's secret.
    Rotate,
    /// Rename a key.
    Rename,
    /// Enable a key.
    On,
    /// Disable a key.
    Off,
    /// Remove a key.
    Remove,
    /// Move a key to a one-based list position.
    Move,
    /// Replace model patterns.
    Models,
    /// Replace account patterns.
    Accounts,
    /// Set scoped quota visibility.
    Quotas,
    /// Set full or read-only access.
    Access,
    /// Set whether the key may issue short-lived keys.
    Issuer,
    /// Return the key's secret again, for copying it.
    Copy,
}

/// One stored limit description, reserved for the limits phase.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GatewayKeyLimit {
    /// Window name.
    pub period: String,
    /// Repeating day count.
    pub days: u32,
    /// Repeating hour count.
    pub hours: u32,
    /// Token cap.
    pub tokens: u64,
    /// Cost cap in US dollars.
    pub cost: f64,
    /// Whether cache reads count.
    pub cache_reads: bool,
    /// Unix-second cycle start.
    pub since: i64,
}

/// A named-key record. `secret` is populated only in backup responses.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GatewayKey {
    /// Stable key id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Disabled flag.
    pub off: bool,
    /// Default sharing-key marker.
    #[serde(rename = "default")]
    pub is_default: bool,
    /// Masked secret display form.
    pub masked: String,
    /// The key's secret, present only in a backup.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret: String,
    /// Model patterns.
    pub models: Vec<String>,
    /// Account patterns.
    pub accounts: Vec<String>,
    /// Scoped quota visibility.
    pub quotas: bool,
    /// Stored read-only flag.
    pub read_only: bool,
    /// Optional Unix-second expiry.
    pub expires: Option<i64>,
    /// Issuer key id.
    pub issuer: String,
    /// Whether the key may issue short-lived keys.
    pub issues: bool,
    /// Stored limits.
    pub limits: Vec<GatewayKeyLimit>,
    /// Removed-default tombstone, present only in a backup.
    #[serde(default, skip_serializing_if = "is_false")]
    pub removed: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl fmt::Debug for GatewayKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayKey")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("off", &self.off)
            .field("is_default", &self.is_default)
            .field("masked", &self.masked)
            .field(
                "secret",
                &if self.secret.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("models", &self.models)
            .field("accounts", &self.accounts)
            .field("quotas", &self.quotas)
            .field("read_only", &self.read_only)
            .field("expires", &self.expires)
            .field("issuer", &self.issuer)
            .field("issues", &self.issues)
            .field("limits", &self.limits)
            .field("removed", &self.removed)
            .finish()
    }
}

/// A key-management mutation.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct GatewayKeyRequest {
    /// Body wire version.
    pub version: u32,
    /// Requested action.
    pub action: GatewayKeyAction,
    /// Target key id, except for add.
    #[serde(default)]
    pub id: String,
    /// New key name for add or rename.
    #[serde(default)]
    pub name: String,
    /// Optional caller-supplied secret for add.
    #[serde(default)]
    pub secret: Option<String>,
    /// One-based destination for move.
    pub place: Option<usize>,
    /// Replacement model patterns.
    #[serde(default)]
    pub models: Vec<String>,
    /// Replacement account patterns.
    #[serde(default)]
    pub accounts: Vec<String>,
    /// Scoped quota flag.
    #[serde(default)]
    pub quotas: bool,
    /// Read-only flag.
    #[serde(default)]
    pub read_only: bool,
    /// Issuer flag.
    #[serde(default)]
    pub issues: bool,
}

impl GatewayKeyRequest {
    /// Constructs a version-current mutation with no optional values.
    #[must_use]
    pub fn new(action: GatewayKeyAction) -> Self {
        Self {
            version: API_VERSION,
            action,
            id: String::new(),
            name: String::new(),
            secret: None,
            place: None,
            models: Vec::new(),
            accounts: Vec::new(),
            quotas: false,
            read_only: false,
            issues: false,
        }
    }

    /// Sets the target id.
    #[must_use]
    pub fn for_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }
}

impl fmt::Debug for GatewayKeyRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayKeyRequest")
            .field("version", &self.version)
            .field("action", &self.action)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("place", &self.place)
            .field("models", &self.models)
            .field("accounts", &self.accounts)
            .field("quotas", &self.quotas)
            .field("read_only", &self.read_only)
            .field("issues", &self.issues)
            .finish()
    }
}

/// Key list or mutation response. A secret is returned only by add, rotate,
/// and copy.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GatewayKeyResponse {
    /// Body wire version.
    pub version: u32,
    /// Keys in their stored order.
    pub keys: Vec<GatewayKey>,
    /// The secret after add, rotate, or copy.
    pub secret: Option<String>,
}

impl Default for GatewayKeyResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            keys: Vec::new(),
            secret: None,
        }
    }
}

impl fmt::Debug for GatewayKeyResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayKeyResponse")
            .field("version", &self.version)
            .field("keys", &self.keys)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Key-store restore input.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct GatewayKeyRestoreRequest {
    /// Body wire version.
    pub version: u32,
    /// Complete records from a backup.
    pub keys: Vec<GatewayKey>,
}

impl GatewayKeyRestoreRequest {
    /// Constructs a version-current restore request.
    #[must_use]
    pub fn new(keys: Vec<GatewayKey>) -> Self {
        Self {
            version: API_VERSION,
            keys,
        }
    }
}
