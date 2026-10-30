//! Protocol wires and credential placement.
//!
//! Preset rows live in compiled plugin fragments (`plugins/registry`). This
//! module keeps only the protocol [`Wire`], the region-resolved [`ModelPlan`]
//! the host model aggregator reads, and [`credential_for`], which derives the
//! header an API key uses from the wire and the base host.

use crate::http::Credential;

/// Which protocol a resolved preset speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wire {
    /// `POST {base}/chat/completions`.
    Chat,
    /// `POST {base}/responses`.
    Responses,
    /// `POST {base}/v1/messages`.
    Messages,
    /// `POST {base}/models/{model}:generateContent`.
    Generative,
    /// Vertex AI publisher `generateContent`. The base includes the project
    /// and the location. The adapter appends `publishers/google/models/{model}`.
    Vertex,
    /// Bedrock Converse. The base is the runtime root. The adapter appends
    /// `/model/{model}/converse`.
    Converse,
}

/// Region-resolved preset data used by the host model aggregator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelPlan {
    /// Chat Completions base URL.
    pub chat: String,
    /// Responses base URL.
    pub responses: String,
    /// Anthropic Messages base URL.
    pub anthropic: String,
    /// Generative Language base URL.
    pub generative: String,
    /// Whether the preset has no ordinary live list.
    pub no_list: bool,
    /// Whether this region enables a live list despite `no_list`.
    pub lists: bool,
    /// Required live-id prefix, empty when there is none.
    pub only: String,
    /// Static ids, with a region's ids replacing the preset's ids.
    pub models: Vec<String>,
    /// Optional quirk profile name recorded by the preset.
    pub quirks: Option<String>,
    /// Optional JSON object merged into a probe body.
    pub extra_body: Option<String>,
}

/// Derives the credential placement for a preset, wire, and resolved base URL.
///
/// The placement follows the wire and the base host. No kernel preset needs
/// its own placement, so the preset id is not read.
#[must_use]
pub fn credential_for(_preset_id: &str, wire: Wire, base_url: &str) -> Credential {
    if wire == Wire::Vertex || wire == Wire::Converse {
        return Credential::Bearer;
    }
    if wire == Wire::Generative {
        return Credential::GoogApiKey;
    }
    let host = host_of(base_url);
    if wire == Wire::Messages {
        if crate::models::anthropic_host(&host)
            || crate::models::azure_host(&host)
            || crate::models::bedrock_host(&host)
        {
            return Credential::XApiKey;
        }
        return Credential::MessagesBoth;
    }
    Credential::Bearer
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::Wire;

    #[test]
    fn bedrock_hosts_keep_x_api_key_on_messages() {
        use crate::Credential;
        for base in [
            "https://bedrock-runtime.us-east-1.amazonaws.com/anthropic",
            "https://bedrock-mantle.us-east-1.api.aws/anthropic",
        ] {
            assert_eq!(
                super::credential_for("custom", Wire::Messages, base),
                Credential::XApiKey,
                "{base}"
            );
        }
        assert_eq!(
            super::credential_for(
                "custom",
                Wire::Messages,
                "https://bedrock-mantle.us-east-1.amazonaws.com/anthropic"
            ),
            Credential::MessagesBoth
        );
    }

    #[test]
    fn anthropic_workspace_host_keeps_x_api_key() {
        use crate::Credential;
        for base in [
            "https://api.anthropic.com",
            "https://workspace.anthropic.com/v1",
        ] {
            assert_eq!(
                super::credential_for("custom", Wire::Messages, base),
                Credential::XApiKey,
                "{base}"
            );
        }
        for base in [
            "https://evilanthropic.com",
            "https://anthropic.com.example.test",
        ] {
            assert_eq!(
                super::credential_for("custom", Wire::Messages, base),
                Credential::MessagesBoth,
                "{base}"
            );
        }
    }
}
