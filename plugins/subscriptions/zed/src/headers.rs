//! Client version and User-Agent strings the reference sends.

/// Zed release the cloud expects on completion calls.
pub(crate) const VERSION: &str = "1.23.0";

/// User-Agent as Zed's editor sends it: `Zed/<version> (<os>; <arch>)`.
#[must_use]
pub(crate) fn user_agent() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!("Zed/{VERSION} ({os}; {arch})")
}

/// Headers every `/client/*` call carries besides authorization.
pub(crate) fn cloud_headers() -> [(&'static str, String); 1] {
    [("User-Agent", user_agent())]
}

/// Headers on GET `/models` with a model token.
pub(crate) fn model_list_headers() -> [(&'static str, String); 3] {
    [
        ("User-Agent", user_agent()),
        ("x-zed-client-supports-x-ai", "true".to_owned()),
        ("x-zed-version", VERSION.to_owned()),
    ]
}

/// Headers on POST `/completions` with a model token.
pub(crate) fn model_token_headers() -> [(&'static str, String); 2] {
    [
        ("User-Agent", user_agent()),
        ("x-zed-version", VERSION.to_owned()),
    ]
}

/// Extra headers on POST `/completions`.
pub(crate) fn completion_headers() -> [(&'static str, String); 2] {
    [
        ("x-zed-client-supports-status-messages", "true".to_owned()),
        (
            "x-zed-client-supports-stream-ended-request-completion-status",
            "true".to_owned(),
        ),
    ]
}
