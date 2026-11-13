//! Read-only library listing contracts.

use serde::{Deserialize, Serialize};

use crate::API_VERSION;

/// One library row in display order supplied by the control plane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LibraryRow {
    /// Row kind; clients must accept unknown values.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Agent ids that have or receive this entry.
    pub on: Vec<String>,
    /// Short description or location.
    pub about: String,
    /// Number of agents with a standing problem for this entry.
    pub bad: usize,
    /// Whether an agent application owns this entry itself.
    pub own: bool,
}

/// Agent paths and display name used to annotate library rows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LibraryAgent {
    /// Agent plugin id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Instructions target path, or empty when unsupported.
    pub instructions: String,
    /// MCP target path, or empty when unsupported.
    pub mcp: String,
    /// Skills target path, or empty when unsupported.
    pub skills: String,
}

/// Read-only library view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LibraryResponse {
    /// Body wire version.
    pub version: u32,
    /// Library directory on the daemon host.
    pub dir: String,
    /// A library file read failure, when one occurred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Library, discovered, and RTK rows.
    pub rows: Vec<LibraryRow>,
    /// Agents that can annotate those rows.
    pub agents: Vec<LibraryAgent>,
}

impl Default for LibraryResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            dir: String::new(),
            error: None,
            rows: Vec::new(),
            agents: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LibraryResponse;
    use crate::{API_VERSION, decode_response};

    #[test]
    fn empty_view_is_versioned_and_omits_absent_error() {
        let response = LibraryResponse::default();
        let body = serde_json::to_vec(&response).expect("library JSON");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON value");
        assert_eq!(value["version"], API_VERSION);
        assert!(value.get("error").is_none());
        assert_eq!(value["rows"], serde_json::json!([]));
        assert_eq!(value["agents"], serde_json::json!([]));
    }

    #[test]
    fn decode_accepts_an_unknown_row_kind() {
        let body = format!(
            r#"{{"version":{API_VERSION},"dir":"/tmp/library","rows":[{{"kind":"future","name":"row","on":[],"about":"","bad":0,"own":false}}],"agents":[]}}"#
        );
        let response: LibraryResponse =
            decode_response(API_VERSION, body.as_bytes()).expect("view");
        assert_eq!(response.rows[0].kind, "future");
    }
}
