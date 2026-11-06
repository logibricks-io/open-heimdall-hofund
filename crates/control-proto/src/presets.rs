use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// Preset catalog kind exposed to control clients.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub enum PresetKind {
    /// Catalog preset with a fixed or regional endpoint.
    #[default]
    Preset,
    /// Local endpoint the user supplies.
    Local,
    /// Remote gateway preset.
    Remote,
    /// User-defined custom provider.
    Custom,
    /// A newer peer supplied an unknown kind.
    #[serde(other)]
    Unknown,
}

/// One preset row from the catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PresetEntry {
    /// Preset id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Preset kind.
    pub kind: PresetKind,
    /// Whether a credential is required.
    pub needs_key: bool,
    /// Whether the user must supply a base URL.
    pub needs_address: bool,
    /// Whether project or location options are required (regional cloud presets).
    pub needs_project: bool,
    /// Declared option keys the preset accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub option_keys: Vec<String>,
}

/// Preset catalog response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PresetListResponse {
    /// Body wire version.
    pub version: u32,
    /// Presets in id order.
    pub presets: Vec<PresetEntry>,
}

impl Default for PresetListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            presets: Vec::new(),
        }
    }
}
