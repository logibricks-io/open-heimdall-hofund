//! Join a declared realtime endpoint onto a provider base URL.
//!
//! The declaration is data. An empty declaration means the provider does not
//! serve a realtime session. A path replaces the base path on the same origin.
//! An absolute HTTP or WebSocket URL is used as given. The gateway dials the
//! result; it does not rewrite a vendor path of its own.

use reqwest::Url;

/// Resolves `declared` against `base`.
///
/// An empty declaration is an empty string. A declaration that starts with
/// `/` replaces the path of `base` and drops its query. Any other declaration
/// must be an absolute `http`, `https`, `ws`, or `wss` URL with no user info.
///
/// # Errors
///
/// Returns a message when `base` or `declared` is not a usable URL.
pub fn realtime_target(base: &str, declared: &str) -> Result<String, String> {
    let declared = declared.trim();
    if declared.is_empty() {
        return Ok(String::new());
    }
    if declared.starts_with('/') {
        if !valid_path(declared) {
            return Err(
                "a realtime path must be an absolute path with no query, fragment, or whitespace"
                    .to_owned(),
            );
        }
        let mut target =
            Url::parse(base).map_err(|_| "the provider base URL is not a URL".to_owned())?;
        if target.scheme() != "http" && target.scheme() != "https" {
            return Err("the provider base URL is not an HTTP URL".to_owned());
        }
        target.set_path(declared);
        target.set_query(None);
        target.set_fragment(None);
        return Ok(target.to_string());
    }
    let url = Url::parse(declared).map_err(|_| "the realtime endpoint is not a URL".to_owned())?;
    if !matches!(url.scheme(), "http" | "https" | "ws" | "wss") {
        return Err("the realtime endpoint must be an HTTP or WebSocket URL".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the realtime endpoint must not carry a user or password".to_owned());
    }
    Ok(url.to_string())
}

/// Adds `model` as the query and switches `http`/`https` to `ws`/`wss`.
///
/// Returns `None` when either side is empty or the endpoint is not a URL.
#[must_use]
pub fn realtime_model_url(endpoint: &str, model: &str) -> Option<String> {
    if endpoint.is_empty() || model.is_empty() {
        return None;
    }
    let mut url = Url::parse(endpoint).ok()?;
    let scheme = match url.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return None,
    };
    if url.set_scheme(scheme).is_err() {
        return None;
    }
    url.query_pairs_mut().append_pair("model", model);
    Some(url.to_string())
}

fn valid_path(path: &str) -> bool {
    !path.contains(['?', '#', ' '])
        && !path.contains("://")
        && !path.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::{realtime_model_url, realtime_target};

    #[test]
    fn d230_a_path_replaces_the_base_path_and_an_absolute_url_is_kept() {
        let joined = realtime_target(
            "https://plan.example.test/compatible-mode/v1",
            "/api-ws/v1/realtime",
        )
        .expect("path");
        assert_eq!(joined, "https://plan.example.test/api-ws/v1/realtime");
        let with_port = realtime_target("http://127.0.0.1:9/v1", "/realtime").expect("port");
        assert_eq!(with_port, "http://127.0.0.1:9/realtime");
        let absolute = realtime_target(
            "https://ignored.example.test/v1",
            "wss://voice.example.test/v1/realtime",
        )
        .expect("absolute");
        assert_eq!(absolute, "wss://voice.example.test/v1/realtime");
        assert_eq!(
            realtime_target("https://example.test/v1", "").expect("empty"),
            ""
        );
        assert!(realtime_target("https://example.test/v1", "not a url").is_err());
        assert!(realtime_target("https://example.test/v1", "/realtime?model=voice").is_err());
        assert!(
            realtime_target(
                "https://example.test/v1",
                "https://user:secret@voice.example.test/v1/realtime"
            )
            .is_err()
        );
        let dial = realtime_model_url(&joined, "voice").expect("dial");
        assert!(dial.starts_with("wss://plan.example.test/api-ws/v1/realtime?"));
        assert!(dial.contains("model=voice"));
    }
}
