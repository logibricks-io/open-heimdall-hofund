//! Factory Droid CLI client identity headers.

/// Droid CLI client type sent on Factory API requests.
pub(crate) const CLIENT_TYPE: &str = "factory-cli";

/// Pinned Droid CLI release used on outbound calls.
pub(crate) const CLIENT_VERSION: &str = "0.242.0";

/// Headers every Factory API call carries.
#[must_use]
pub(crate) fn headers() -> Vec<(String, String)> {
    vec![
        ("accept".to_owned(), "application/json".to_owned()),
        ("x-factory-client".to_owned(), CLIENT_TYPE.to_owned()),
        (
            "user-agent".to_owned(),
            format!("{CLIENT_TYPE}/{CLIENT_VERSION}"),
        ),
    ]
}
