use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, FieldIssue, MAX_CONFIG_TEXT_BYTES};

/// Whole-document configuration validation input.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ConfigValidateRequest {
    /// Body wire version.
    pub version: u32,
    /// Full TOML document. Its bytes are redacted from `Debug`.
    pub text: String,
}

impl ConfigValidateRequest {
    /// Constructs a version-current validation request.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            text: text.into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        validate_config_text(&self.text)
    }
}

impl fmt::Debug for ConfigValidateRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigValidateRequest")
            .field("version", &self.version)
            .field("text", &format_args!("<{} bytes>", self.text.len()))
            .finish()
    }
}

/// Whole-document, revision-checked configuration apply input.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ConfigApplyRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 disk-content revision.
    pub expected_revision: String,
    /// Full TOML document. Its bytes are redacted from `Debug`.
    pub text: String,
}

impl ConfigApplyRequest {
    /// Constructs a version-current apply request.
    #[must_use]
    pub fn new(expected_revision: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            expected_revision: expected_revision.into(),
            text: text.into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        validate_config_text(&self.text)
    }
}

impl fmt::Debug for ConfigApplyRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigApplyRequest")
            .field("version", &self.version)
            .field("expected_revision", &self.expected_revision)
            .field("text", &format_args!("<{} bytes>", self.text.len()))
            .finish()
    }
}

/// Explicit reload input guarded by the expected disk revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ReloadRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 disk-content revision.
    pub expected_revision: String,
}

impl ReloadRequest {
    /// Constructs a version-current reload request.
    #[must_use]
    pub fn new(expected_revision: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            expected_revision: expected_revision.into(),
        }
    }
}

/// Current on-disk and published configuration state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 revision of the file on disk.
    pub disk_revision: String,
    /// SHA-256 revision of the published runtime configuration.
    pub runtime_revision: String,
    /// Monotonically increasing publication generation.
    pub generation: u64,
    /// Full TOML document. Its bytes are redacted from `Debug`.
    pub text: String,
    /// Restart-only fields written but not active.
    pub pending_restart: Vec<String>,
    /// Live fields whose runtime update was not applied.
    pub not_applied: Vec<FieldIssue>,
}

impl Default for ConfigResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            disk_revision: String::new(),
            runtime_revision: String::new(),
            generation: 0,
            text: String::new(),
            pending_restart: Vec::new(),
            not_applied: Vec::new(),
        }
    }
}

impl ConfigResponse {
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        validate_config_text(&self.text)
    }
}

impl fmt::Debug for ConfigResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigResponse")
            .field("version", &self.version)
            .field("disk_revision", &self.disk_revision)
            .field("runtime_revision", &self.runtime_revision)
            .field("generation", &self.generation)
            .field("text", &format_args!("<{} bytes>", self.text.len()))
            .field("pending_restart", &self.pending_restart)
            .field("not_applied", &self.not_applied)
            .finish()
    }
}

/// Configuration validation result without runtime publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigValidationResponse {
    /// Body wire version.
    pub version: u32,
    /// Whether the complete candidate validated.
    pub valid: bool,
    /// Sanitized configuration issues.
    pub issues: Vec<FieldIssue>,
    /// Fields that would remain inactive until restart.
    pub pending_restart: Vec<String>,
}

impl Default for ConfigValidationResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            valid: false,
            issues: Vec::new(),
            pending_restart: Vec::new(),
        }
    }
}

fn validate_config_text(text: &str) -> Result<(), ControlError> {
    if text.len() <= MAX_CONFIG_TEXT_BYTES {
        return Ok(());
    }
    let mut error = ControlError::new(
        ControlErrorKind::BadRequest,
        "decoded configuration text exceeds the one-mebibyte limit",
        "validate",
    );
    error.details.push(FieldIssue::new(
        "text",
        "too_large",
        "configuration text exceeds the one-mebibyte limit",
    ));
    Err(error)
}
