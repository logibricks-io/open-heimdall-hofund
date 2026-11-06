use serde::{Deserialize, Serialize};

use crate::{
    API_VERSION, ControlError, ControlErrorKind, FieldIssue, MAX_ID_BYTES, MAX_IDS,
    validate_revision,
};

/// Optional model selection for a configured provider connectivity test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProviderTestRequest {
    /// Body wire version.
    pub version: u32,
    /// Models to test, or `None` for host-side selection.
    pub models: Option<Vec<String>>,
}

impl ProviderTestRequest {
    /// Constructs a version-current provider test request.
    #[must_use]
    pub fn new(models: Option<Vec<String>>) -> Self {
        Self {
            version: API_VERSION,
            models,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if let Some(models) = &self.models {
            validate_ids("models", models)?;
        }
        Ok(())
    }
}

/// Minimal secret-free configured-provider summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderSummary {
    /// Configured provider id.
    pub id: String,
    /// Display name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Implementation kind label.
    pub kind: String,
    /// Protocol label.
    pub protocol: String,
    /// Whether the provider is ready for calls.
    pub ready: bool,
    /// Whether the provider accepts requests.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional visibility family tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Configured API root, when set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Preset id, when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Subscription account label for account-type rows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_label: String,
    /// Account row with every active login lapsed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub signed_out: bool,
    /// Whether the provider appears in agent model pickers.
    #[serde(default = "default_true")]
    pub listed: bool,
    /// Last model-list error, sanitized.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub list_error: String,
    /// Masked primary key fingerprint, when present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_masked: String,
    /// Whether a project-style base URL can be formed from configured options.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_address_ready: bool,
    /// Service account impersonation target, when configured.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub impersonate_account: String,
    /// Number of currently listed models.
    pub model_count: u64,
    /// Models with an active model-specific rest on this provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resting_models: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Provider list response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderListResponse {
    /// Body wire version.
    pub version: u32,
    /// Configured providers.
    pub providers: Vec<ProviderSummary>,
}

impl Default for ProviderListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            providers: Vec::new(),
        }
    }
}

/// One model connectivity result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderTestResult {
    /// Tested model id.
    pub model: String,
    /// Whether the probe succeeded.
    pub ok: bool,
    /// Upstream HTTP status when available.
    pub status: Option<u16>,
    /// Unsigned elapsed milliseconds.
    pub latency_ms: Option<u64>,
    /// Sanitized failure details.
    pub error: Option<ControlError>,
}

/// Provider connectivity test response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderTestResponse {
    /// Body wire version.
    pub version: u32,
    /// Configured provider id.
    pub provider: String,
    /// Per-model results.
    pub results: Vec<ProviderTestResult>,
}

impl Default for ProviderTestResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            provider: String::new(),
            results: Vec::new(),
        }
    }
}

/// Revision-checked full-list replacement or delta edit of model picks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct PicksSetRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_picks_revision: String,
    /// Complete replacement ids, mutually exclusive with `edit`.
    pub ids: Option<Vec<String>>,
    /// Delta edit tokens or a sole reset token, mutually exclusive with `ids`.
    pub edit: Option<Vec<String>>,
}

impl PicksSetRequest {
    /// Constructs a full-list replacement request.
    #[must_use]
    pub fn replace(expected_picks_revision: impl Into<String>, ids: Vec<String>) -> Self {
        Self {
            version: API_VERSION,
            expected_picks_revision: expected_picks_revision.into(),
            ids: Some(ids),
            edit: None,
        }
    }

    /// Constructs a delta-edit or sole-token reset request.
    #[must_use]
    pub fn edit(expected_picks_revision: impl Into<String>, changes: Vec<String>) -> Self {
        Self {
            version: API_VERSION,
            expected_picks_revision: expected_picks_revision.into(),
            ids: None,
            edit: Some(changes),
        }
    }

    /// Validates and returns the typed picks operation without applying it.
    ///
    /// `all`, `-`, and `none` are recognized only as the sole whole edit
    /// argument. `none` is deferred until host state has a no-models flag.
    /// State-dependent application and prefix handling belong to the server.
    pub fn form(&self) -> Result<PicksForm, ControlError> {
        match (&self.ids, &self.edit) {
            (Some(_), Some(_)) => Err(picks_error(
                ControlErrorKind::BadRequest,
                "edit",
                "both_present",
                "exactly one of ids or edit is required",
            )),
            (None, None) => Err(picks_error(
                ControlErrorKind::BadRequest,
                "ids",
                "missing_form",
                "exactly one of ids or edit is required",
            )),
            (Some(ids), None) => {
                validate_ids("ids", ids)?;
                if ids.is_empty() {
                    return Err(picks_error(
                        ControlErrorKind::Unprocessable,
                        "ids",
                        "empty",
                        "a replacement pick list must not be empty",
                    ));
                }
                if ids.as_slice() == ["none"] {
                    return Err(picks_error(
                        ControlErrorKind::Unprocessable,
                        "ids",
                        "none_deferred",
                        "the no-models pick form is deferred",
                    ));
                }
                Ok(PicksForm::Replace(ids.clone()))
            }
            (None, Some(edit)) => {
                validate_id_count("edit", edit.len())?;
                if edit.is_empty() {
                    return Err(picks_error(
                        ControlErrorKind::Unprocessable,
                        "edit",
                        "empty",
                        "an edit list must not be empty",
                    ));
                }
                if edit.as_slice() == ["none"] {
                    return Err(picks_error(
                        ControlErrorKind::Unprocessable,
                        "edit",
                        "none_deferred",
                        "the no-models pick form is deferred",
                    ));
                }
                if edit.as_slice() == ["all"] || edit.as_slice() == ["-"] {
                    return Ok(PicksForm::Reset);
                }
                let mut changes = Vec::with_capacity(edit.len());
                for token in edit {
                    let Some(operation) = token.as_bytes().first().copied() else {
                        return Err(edit_token_error(
                            "invalid",
                            "an edit token must not be empty",
                        ));
                    };
                    if !matches!(operation, b'+' | b'-') {
                        return Err(mixed_form());
                    }
                    let id = &token[1..];
                    if id.is_empty() {
                        return Err(edit_token_error("invalid", "an edit id must not be empty"));
                    }
                    if id.len() > MAX_ID_BYTES {
                        return Err(edit_token_error(
                            "too_large",
                            "an edit id exceeds the 256-byte limit",
                        ));
                    }
                    if operation == b'+' {
                        changes.push(PickEdit::Add(id.to_owned()));
                    } else {
                        changes.push(PickEdit::Remove(id.to_owned()));
                    }
                }
                Ok(PicksForm::Edit(changes))
            }
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_picks_revision", &self.expected_picks_revision)?;
        self.form().map(|_| ())
    }
}

/// Validated picks operation. Applying it to host state is deliberately not
/// part of the protocol crate.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PicksForm {
    /// Replace the complete pick list.
    Replace(Vec<String>),
    /// Apply ordered additions and removals.
    Edit(Vec<PickEdit>),
    /// Restore the default list.
    Reset,
}

/// One validated delta edit.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PickEdit {
    /// Add a model id.
    Add(String),
    /// Remove a model id.
    Remove(String),
}

/// Current host-owned picks and content revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PicksResponse {
    /// Body wire version.
    pub version: u32,
    /// Model ids picked for exposure.
    pub picks: Vec<String>,
    /// Ids in the source list when picks were written.
    pub picked_from: Vec<String>,
    /// Former picks absent from the latest nonempty list.
    pub delisted: Vec<String>,
    /// SHA-256 host-state content revision.
    pub picks_revision: String,
}

impl Default for PicksResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            picks: Vec::new(),
            picked_from: Vec::new(),
            delisted: Vec::new(),
            picks_revision: String::new(),
        }
    }
}

fn mixed_form() -> ControlError {
    edit_token_error("mixed_forms", "edit tokens must all be +id or -id changes")
}

fn edit_token_error(reason: &str, message: &str) -> ControlError {
    picks_error(ControlErrorKind::Unprocessable, "edit", reason, message)
}

fn picks_error(kind: ControlErrorKind, field: &str, reason: &str, message: &str) -> ControlError {
    let mut error = ControlError::new(kind, message, "validate");
    error.details.push(FieldIssue::new(field, reason, message));
    error
}

fn validate_id_count(field: &str, count: usize) -> Result<(), ControlError> {
    if count > MAX_IDS {
        return Err(picks_error(
            ControlErrorKind::Unprocessable,
            field,
            "too_many",
            "the id list exceeds the 4096-entry limit",
        ));
    }
    Ok(())
}

fn validate_ids(field: &str, ids: &[String]) -> Result<(), ControlError> {
    validate_id_count(field, ids.len())?;
    if ids.iter().any(|id| id.is_empty()) {
        return Err(picks_error(
            ControlErrorKind::Unprocessable,
            field,
            "invalid",
            "an id must not be empty",
        ));
    }
    if ids.iter().any(|id| id.len() > MAX_ID_BYTES) {
        return Err(picks_error(
            ControlErrorKind::Unprocessable,
            field,
            "too_large",
            "an id exceeds the 256-byte limit",
        ));
    }
    Ok(())
}

/// Provider rename request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProviderRenameRequest {
    /// Body wire version.
    pub version: u32,
    /// New provider id.
    pub new_id: String,
}

impl ProviderRenameRequest {
    /// Constructs a rename request.
    #[must_use]
    pub fn new(new_id: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            new_id: new_id.into(),
        }
    }
}

/// Provider rename outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderRenameResponse {
    /// Body wire version.
    pub version: u32,
    /// Previous provider id.
    pub from: String,
    /// New provider id.
    pub to: String,
    /// Config revision after the rename.
    pub disk_revision: String,
    /// Agent ids whose model references were rewritten.
    pub agents: Vec<String>,
}

impl Default for ProviderRenameResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            from: String::new(),
            to: String::new(),
            disk_revision: String::new(),
            agents: Vec::new(),
        }
    }
}

/// Adds a configured provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProviderCreateRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_revision: String,
    /// Preset id from the preset catalog.
    pub preset: String,
    /// Optional provider id; generated when absent.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional API root.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Write-only credential material.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional visibility family.
    #[serde(default)]
    pub family: Option<String>,
    /// Preset-specific options named by the catalog.
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, String>,
}

impl ProviderCreateRequest {
    /// Validates revision and ids.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)?;
        if self.preset.trim().is_empty() {
            return Err(provider_field_error(
                "preset",
                "empty",
                "a preset id is required",
            ));
        }
        if let Some(id) = &self.id {
            validate_provider_id("id", id)?;
        }
        Ok(())
    }
}

/// Patches one configured provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProviderPatchRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_revision: String,
    /// Enables or disables the provider for requests and agent pickers.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Sets or clears the visibility family tag.
    #[serde(default)]
    pub family: Option<Option<String>>,
    /// Keeps the provider routable but hides it from agent model pickers.
    #[serde(default)]
    pub listed: Option<bool>,
    /// Sets the API root.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Replaces fallback route ids (`provider/model` or `group/id`).
    #[serde(default)]
    pub fallback: Option<Vec<String>>,
}

impl ProviderPatchRequest {
    /// Validates revision and fallback entries when present.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)?;
        if let Some(fallback) = &self.fallback {
            validate_id_count("fallback", fallback.len())?;
            for entry in fallback {
                if entry.trim().is_empty() {
                    return Err(provider_field_error(
                        "fallback",
                        "empty",
                        "a fallback entry must not be empty",
                    ));
                }
                if entry.len() > MAX_ID_BYTES {
                    return Err(provider_field_error(
                        "fallback",
                        "too_large",
                        "a fallback entry exceeds the 256-byte limit",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Removes one configured provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct ProviderDeleteRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_revision: String,
}

impl ProviderDeleteRequest {
    /// Validates the expected revision.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)
    }
}

/// Outcome of a provider create or patch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderWriteResponse {
    /// Body wire version.
    pub version: u32,
    /// Updated provider summary.
    pub provider: ProviderSummary,
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// SHA-256 published runtime revision.
    pub runtime_revision: String,
    /// Restart-only fields not yet active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_restart: Vec<String>,
}

impl Default for ProviderWriteResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            provider: ProviderSummary::default(),
            disk_revision: String::new(),
            runtime_revision: String::new(),
            pending_restart: Vec::new(),
        }
    }
}

/// Outcome of a provider delete.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProviderDeleteResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// Removed provider id.
    pub removed: String,
}

impl Default for ProviderDeleteResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            disk_revision: String::new(),
            removed: String::new(),
        }
    }
}

fn validate_provider_id(field: &str, id: &str) -> Result<(), ControlError> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_ID_BYTES {
        return Err(provider_field_error(
            field,
            "invalid",
            "a provider id is missing or too long",
        ));
    }
    Ok(())
}

fn provider_field_error(field: &str, reason: &str, message: &str) -> ControlError {
    let mut error = ControlError::new(ControlErrorKind::Unprocessable, message, "validate");
    error.details.push(FieldIssue::new(field, reason, message));
    error
}
