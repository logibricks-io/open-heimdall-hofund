use std::fmt;

use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Provider API-key management action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum ProviderKeyAction {
    /// Add one or more keys.
    Add,
    /// Remove a key.
    Remove,
    /// Remove several keys at once, named by fingerprint in `keys`.
    RemoveMany,
    /// Enable a key.
    On,
    /// Disable a key.
    Off,
    /// Make a key first and enable it.
    Use,
    /// Rename a key.
    Rename,
    /// Replace a key's credential in place.
    Replace,
    /// Set a key's weighted-routing share (0 resets to the default, 1).
    Weight,
    /// Restrict a key to one codec, or clear the restriction with an empty protocol.
    Protocol,
    /// Select a preset region or plan for one key, or clear it to inherit the provider's.
    Plan,
}

/// One provider key's active rest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderKeyRest {
    /// Stable cooldown cause.
    pub why: String,
    /// Upstream status, when one caused the rest.
    pub status: Option<u16>,
    /// Unix milliseconds when the rest ends.
    pub until_ms: i64,
    /// Whether a background model-list probe may lift the rest.
    pub probed: bool,
}

/// Secret-free provider API-key record.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderKey {
    /// Ten-hex-character credential fingerprint.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Masked credential display form.
    pub masked: String,
    /// Whether the key is enabled.
    pub on: bool,
    /// Only codec the key works with, or empty for any.
    pub protocol: String,
    /// Preset region or plan, or empty to inherit the provider's plan.
    #[serde(default)]
    pub plan: String,
    /// Weighted-routing share.
    pub weight: u64,
    /// Active member-wide rest, when present.
    pub rest: Option<ProviderKeyRest>,
}

/// Provider key list or mutation response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderKeysResponse {
    /// Body wire version.
    pub version: u32,
    /// Configured provider id.
    pub provider: String,
    /// Keys in candidate order.
    pub keys: Vec<ProviderKey>,
    /// Keys newly added by this mutation.
    pub added: u64,
    /// Submitted keys already present.
    pub already_had: u64,
    /// Keys removed by a bulk removal.
    #[serde(default)]
    pub removed: u64,
    /// Fingerprint before a replacement.
    pub before: String,
    /// Fingerprint after a replacement.
    pub after: String,
}

impl Default for ProviderKeysResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            provider: String::new(),
            keys: Vec::new(),
            added: 0,
            already_had: 0,
            removed: 0,
            before: String::new(),
            after: String::new(),
        }
    }
}

/// Provider key mutation.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProviderKeyRequest {
    /// Body wire version.
    pub version: u32,
    /// Requested action.
    pub action: ProviderKeyAction,
    /// Target key fingerprint, except for add.
    #[serde(default)]
    pub key_id: String,
    /// New keys for add, or the replacement credential spec.
    #[serde(default)]
    pub keys: Vec<String>,
    /// New display name for rename.
    #[serde(default)]
    pub name: String,
    /// New weight for the weight action.
    #[serde(default)]
    pub weight: u64,
    /// Codec for the protocol action, or for keys being added; empty means any.
    #[serde(default)]
    pub protocol: String,
    /// Preset region or plan for the plan action; empty inherits the provider's plan.
    #[serde(default)]
    pub plan: String,
}

impl ProviderKeyRequest {
    /// Constructs a version-current mutation with no optional values.
    #[must_use]
    pub fn new(action: ProviderKeyAction) -> Self {
        Self {
            version: API_VERSION,
            action,
            key_id: String::new(),
            keys: Vec::new(),
            name: String::new(),
            weight: 0,
            protocol: String::new(),
            plan: String::new(),
        }
    }

    /// Sets the target key fingerprint.
    #[must_use]
    pub fn for_id(mut self, id: impl Into<String>) -> Self {
        self.key_id = id.into();
        self
    }
}

impl fmt::Debug for ProviderKeyRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderKeyRequest")
            .field("version", &self.version)
            .field("action", &self.action)
            .field("key_id", &self.key_id)
            .field("keys", &self.keys.len())
            .field("name", &self.name)
            .field("weight", &self.weight)
            .field("protocol", &self.protocol)
            .field("plan", &self.plan)
            .finish()
    }
}
