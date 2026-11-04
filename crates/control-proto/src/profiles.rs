use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, MAX_ID_BYTES};

/// One named profile on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProfileSummary {
    /// Profile name.
    pub name: String,
    /// Whether applying this profile would change nothing now.
    pub in_use: bool,
}

/// Listed profile names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProfileListResponse {
    /// Body wire version.
    pub version: u32,
    /// Profiles in name order.
    pub profiles: Vec<ProfileSummary>,
}

impl Default for ProfileListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            profiles: Vec::new(),
        }
    }
}

/// One profile's stored fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProfileDetailResponse {
    /// Body wire version.
    pub version: u32,
    /// Profile name.
    pub name: String,
    /// Whether applying this profile would change nothing now.
    pub in_use: bool,
    /// `agent_id.field_key` to value. Empty string is the field default.
    pub fields: std::collections::BTreeMap<String, String>,
}

impl Default for ProfileDetailResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            name: String::new(),
            in_use: false,
            fields: std::collections::BTreeMap::new(),
        }
    }
}

/// Saves a profile from the current agent settings or explicit fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProfileSaveRequest {
    /// Body wire version.
    pub version: u32,
    /// Profile name.
    pub name: String,
    /// When true, capture the current detected agents instead of `fields`.
    #[serde(default)]
    pub from_current: bool,
    /// Explicit fields when `from_current` is false.
    #[serde(default)]
    pub fields: std::collections::BTreeMap<String, String>,
}

impl ProfileSaveRequest {
    /// Constructs a save-from-current request.
    #[must_use]
    pub fn from_current(name: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            name: name.into(),
            from_current: true,
            fields: std::collections::BTreeMap::new(),
        }
    }

    /// Rejects an empty or overlong profile name.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_profile_name(&self.name)
    }
}

/// Applies a stored profile to detected agents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProfileSwitchRequest {
    /// Body wire version.
    pub version: u32,
    /// Profile name.
    pub name: String,
}

impl ProfileSwitchRequest {
    /// Constructs a version-current switch request.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            name: name.into(),
        }
    }

    /// Rejects an empty or overlong profile name.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_profile_name(&self.name)
    }
}

/// Removes a stored profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProfileDeleteRequest {
    /// Body wire version.
    pub version: u32,
    /// Profile name.
    pub name: String,
}

impl ProfileDeleteRequest {
    /// Constructs a version-current delete request.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            name: name.into(),
        }
    }

    /// Rejects an empty or overlong profile name.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_profile_name(&self.name)
    }
}

/// Result of applying a profile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProfileSwitchResponse {
    /// Body wire version.
    pub version: u32,
    /// How many agent fields changed.
    pub changed: u64,
    /// Skipped agents or fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Non-fatal apply messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl ProfileSwitchResponse {
    /// Constructs a version-current switch response.
    #[must_use]
    pub fn new(changed: u64) -> Self {
        Self {
            version: API_VERSION,
            changed,
            notes: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

fn validate_profile_name(name: &str) -> Result<(), ControlError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_ID_BYTES {
        return Err(ControlError::new(
            ControlErrorKind::Unprocessable,
            "profile name is missing or too long",
            "validate",
        ));
    }
    Ok(())
}
