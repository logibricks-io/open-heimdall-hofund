//! Forwards listed client request headers to one provider.

use std::collections::BTreeMap;

use hofund_ir::Request;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Extension key: a map of client header names to values.
pub const CLIENT_MAP: &str = "hofund_client_request_headers";

/// Applies `listed` headers from `client` onto `out`, replacing fixed values.
pub fn apply(listed: &[String], client: &BTreeMap<String, String>, out: &mut HeaderMap) {
    for name in listed {
        let Some(value) = client.get(name).or_else(|| {
            client
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
        }) else {
            continue;
        };
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(mut header_value) = HeaderValue::from_str(value) else {
            continue;
        };
        out.remove(header_name.clone());
        for existing in out.keys().cloned().collect::<Vec<_>>() {
            if existing.as_str().eq_ignore_ascii_case(name) {
                out.remove(existing);
            }
        }
        header_value.set_sensitive(true);
        out.insert(header_name, header_value);
    }
}

/// Client headers stored on `request`, if any.
#[must_use]
pub fn client_map(request: &Request) -> Option<BTreeMap<String, String>> {
    let value = request.extensions.get(CLIENT_MAP)?;
    serde_json::from_value(value.clone()).ok()
}
