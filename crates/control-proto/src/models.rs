use serde::{Deserialize, Serialize};

use crate::{API_VERSION, ControlError, ControlErrorKind, FieldIssue, MAX_ID_BYTES, MAX_IDS};

/// Minimal secret-free model entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelEntry {
    /// Model id.
    pub id: String,
    /// Optional display label.
    pub display_name: Option<String>,
    /// Optional context-window size.
    pub context: Option<u64>,
    /// Optional output-token limit.
    pub max_output: Option<u64>,
}

/// Cached or refreshed provider model list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelListResponse {
    /// Body wire version.
    pub version: u32,
    /// Configured provider id.
    pub provider: String,
    /// Aggregated models.
    pub models: Vec<ModelEntry>,
    /// Unix milliseconds when the list was fetched, when known.
    pub fetched_at_ms: Option<u64>,
    /// Picks added by refresh reconciliation.
    pub added: Vec<String>,
    /// Picks removed by refresh reconciliation.
    pub removed: Vec<String>,
    /// Former picks absent from the refreshed list.
    pub delisted: Vec<String>,
}

impl Default for ModelListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            provider: String::new(),
            models: Vec::new(),
            fetched_at_ms: None,
            added: Vec::new(),
            removed: Vec::new(),
            delisted: Vec::new(),
        }
    }
}

/// One member held in reserve by accounts at once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouteReserve {
    /// Stable member id.
    pub member: String,
    /// Provider or subscription at-once limit.
    pub at_once: u64,
}

/// One client-facing route model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouteModel {
    /// Route model id.
    pub id: String,
    /// Registered member-ordering strategy.
    #[serde(default = "default_routing")]
    pub routing: String,
    /// Conversation affinity mode.
    #[serde(default = "default_stays")]
    pub stays: String,
    /// Members currently held in reserve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserves: Vec<RouteReserve>,
}

impl Default for RouteModel {
    fn default() -> Self {
        Self {
            id: String::new(),
            routing: default_routing(),
            stays: default_stays(),
            reserves: Vec::new(),
        }
    }
}

fn default_routing() -> String {
    "smart".to_owned()
}

fn default_stays() -> String {
    "auto".to_owned()
}

/// Route model list response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RouteModelListResponse {
    /// Body wire version.
    pub version: u32,
    /// Configured client-facing models.
    pub models: Vec<RouteModel>,
}

impl Default for RouteModelListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            models: Vec::new(),
        }
    }
}

/// Request to disable or enable one `[catalog].disabled` pattern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct CatalogModelRequest {
    /// Body wire version.
    pub version: u32,
    /// Route or provider/model pattern to add or remove.
    pub model: String,
}

impl CatalogModelRequest {
    /// Constructs a version-current catalog model request.
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            version: API_VERSION,
            model: model.into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        let model = self.model.trim();
        let (reason, message) = if model.is_empty() {
            ("invalid", "model must not be empty")
        } else if model.len() > MAX_ID_BYTES {
            ("too_large", "model exceeds the 256-byte limit")
        } else {
            return Ok(());
        };
        let mut error = ControlError::new(ControlErrorKind::Unprocessable, message, "validate");
        error
            .details
            .push(FieldIssue::new("model", reason, message));
        Err(error)
    }
}

/// `[catalog].disabled` patterns after a disable or enable mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CatalogDisabledListResponse {
    /// Body wire version.
    pub version: u32,
    /// Disabled patterns currently in config.
    pub disabled: Vec<String>,
}

impl Default for CatalogDisabledListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            disabled: Vec::new(),
        }
    }
}

impl CatalogDisabledListResponse {
    pub(crate) fn validate(&self) -> Result<(), ControlError> {
        if self.disabled.len() > MAX_IDS {
            return Err(ControlError::new(
                ControlErrorKind::Unprocessable,
                format!("disabled exceeds the {MAX_IDS}-entry limit"),
                "validate",
            ));
        }
        for pattern in &self.disabled {
            if pattern.len() > MAX_ID_BYTES {
                return Err(ControlError::new(
                    ControlErrorKind::Unprocessable,
                    "a disabled pattern exceeds the 256-byte limit",
                    "validate",
                ));
            }
        }
        Ok(())
    }
}
