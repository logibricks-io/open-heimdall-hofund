//! Factory Droid CLI client identity headers.

/// Droid CLI client type sent in the User-Agent on Factory API requests.
pub(crate) const CLIENT_TYPE: &str = "factory-cli";

/// Client id Factory expects beside the User-Agent.
pub(crate) const CLIENT_HEADER: &str = "cli";

/// Pinned Droid CLI release used on outbound calls.
pub(crate) const CLIENT_VERSION: &str = "0.242.0";

/// Headers every Factory API call carries.
#[must_use]
pub(crate) fn headers() -> Vec<(String, String)> {
    let mut headers = vec![("accept".to_owned(), "application/json".to_owned())];
    headers.extend(chat_headers());
    headers
}

/// Headers for model calls (wire sets Accept for streams).
#[must_use]
pub(crate) fn chat_headers() -> Vec<(String, String)> {
    vec![
        ("x-factory-client".to_owned(), CLIENT_HEADER.to_owned()),
        ("x-client-version".to_owned(), CLIENT_VERSION.to_owned()),
        (
            "user-agent".to_owned(),
            format!("{CLIENT_TYPE}/{CLIENT_VERSION}"),
        ),
    ]
}
