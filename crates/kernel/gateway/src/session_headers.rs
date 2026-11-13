//! Session header names passed to the HTTP edge.

use hofund_codec::{kernel_session_header_names, merge_extra_session_header_names};
use hofund_plugin_api::AgentDescription;

/// Builds the ordered session header list for the gateway.
#[must_use]
pub fn gateway_session_header_names(descriptions: &[&AgentDescription]) -> Vec<String> {
    let mut extra = Vec::new();
    for description in descriptions {
        for header in description.session_headers() {
            extra.push((*header).to_owned());
        }
    }
    merge_extra_session_header_names(kernel_session_header_names(), &extra)
}
