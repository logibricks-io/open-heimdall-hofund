use serde::{Deserialize, Serialize};

use crate::API_VERSION;

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
