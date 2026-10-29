//! The API root, the location's models, and the setting checks.

use std::path::Path;

use hofund_plugin_api::ConfigError;
use hofund_plugin_sdk::url::{Host, Url};

/// The Gemini 3 Flash ids the `us` and `eu` multi-regions serve.
const US_EU_MODELS: &[&str] = &[
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
];

/// The Gemini 2.5 ids a region such as `us-central1` serves.
const REGION_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
];

/// The `global` list: the preset's catalog row in `presets.toml`.
pub(crate) const GLOBAL_MODELS: &[&str] = &[
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-4-argon",
    "gemini-3.1-pro-preview",
    "gemini-3.1-pro-preview-customtools",
    "gemini-3.1-flash-lite",
    "gemini-3-flash-preview",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
];

/// Static model ids a location serves.
///
/// `global` is the catalog list. `us` and `eu` serve the Gemini 3 Flash ids.
/// Any other location, including a region such as `us-central1`, serves the
/// Gemini 2.5 ids. This is not a live list.
pub(crate) fn models_for(location: &str) -> &'static [&'static str] {
    match location.trim().to_ascii_lowercase().as_str() {
        "" | "global" => GLOBAL_MODELS,
        "us" | "eu" => US_EU_MODELS,
        _ => REGION_MODELS,
    }
}

/// The API root for `project` at `location`.
///
/// An empty location is `global`. An empty `root_override` uses the host for
/// that location (`aiplatform.googleapis.com`,
/// `aiplatform.<us|eu>.rep.googleapis.com`, or
/// `<region>-aiplatform.googleapis.com`). A non-empty override replaces that
/// host, which is how a test server or a relay is addressed. A pasted
/// `:generateContent` URL is cut back to the project root.
pub(crate) fn base_url(
    project: &str,
    location: &str,
    root_override: &str,
) -> Result<String, String> {
    let project = project.trim().to_ascii_lowercase();
    let mut location = location.trim().to_ascii_lowercase();
    if project.is_empty() {
        return Err("Google Vertex AI needs the id of your Google Cloud project".to_owned());
    }
    if !project_ok(&project) {
        return Err(format!("{project:?} is no Google Cloud project id"));
    }
    if location.is_empty() {
        "global".clone_into(&mut location);
    }
    if !location_ok(&location) {
        return Err(format!(
            "{location:?} is no Vertex AI location: global, us, eu or a region such as us-central1"
        ));
    }
    let suffix = format!("/v1/projects/{project}/locations/{location}");
    let root = if root_override.trim().is_empty() {
        format!("https://{}", host(&location))
    } else {
        let mut raw = root_override.trim().trim_end_matches('/').to_owned();
        if !raw.contains("://") {
            raw = format!("https://{raw}");
        }
        if let Some(index) = raw.find("/publishers/google/models") {
            raw.truncate(index);
            raw = raw.trim_end_matches('/').to_owned();
        }
        if raw.ends_with(&suffix) {
            return parse_base(&raw);
        }
        raw
    };
    parse_base(&format!("{root}{suffix}"))
}

fn parse_base(url: &str) -> Result<String, String> {
    let parsed = Url::parse(url).map_err(|error| format!("provider URL is invalid: {error}"))?;
    if parsed.host_str().is_none() {
        return Err("provider URL has no host".to_owned());
    }
    Ok(url.trim_end_matches('/').to_owned())
}

/// The location segment of a base URL: the one after `locations`.
pub(crate) fn location_of(base: &str) -> String {
    Url::parse(base)
        .ok()
        .and_then(|url| {
            let segments = url.path_segments()?.collect::<Vec<_>>();
            segments
                .iter()
                .position(|segment| *segment == "locations")
                .and_then(|index| segments.get(index + 1))
                .map(|location| (*location).to_owned())
        })
        .unwrap_or_default()
}

/// The API host for a location that has already been checked.
pub(crate) fn host(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        other => format!("{other}-aiplatform.googleapis.com"),
    }
}

fn project_ok(project: &str) -> bool {
    let bytes = project.as_bytes();
    let [first, .., last] = bytes else {
        return false;
    };
    project_char(*first) && project_char(*last) && bytes.iter().all(|byte| project_byte(*byte))
}

fn project_char(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn project_byte(byte: u8) -> bool {
    project_char(byte) || matches!(byte, b'.' | b':' | b'-')
}

fn location_ok(location: &str) -> bool {
    let mut parts = location.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty() || !first.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return false;
    }
    parts.all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

/// `token_url` and `iam_root` are empty, an https URL, or an http URL on a
/// loopback host. The value is not repeated in the error.
pub(crate) fn check_token_urls(token_url: &str, iam_root: &str) -> Result<(), ConfigError> {
    for (key, value) in [("token_url", token_url), ("iam_root", iam_root)] {
        if !value.is_empty() && !https_or_loopback(value) {
            return Err(ConfigError::new(
                "must be an https URL, or an http URL on a loopback host",
            )
            .with_path(key));
        }
    }
    Ok(())
}

fn https_or_loopback(value: &str) -> bool {
    let Ok(parsed) = Url::parse(value) else {
        return false;
    };
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    match parsed.scheme() {
        "https" => parsed.host().is_some(),
        "http" => match parsed.host() {
            Some(Host::Domain(host)) => host == "localhost",
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// `credentials` is empty or a full path; `impersonate` is empty or an email.
pub(crate) fn check_settings(credentials: &str, impersonate: &str) -> Result<(), ConfigError> {
    check_credentials_path(credentials)?;
    if !impersonate.is_empty() && !account_ok(impersonate) {
        return Err(
            ConfigError::new(format!("{impersonate:?} is no service account's email"))
                .with_path("impersonate"),
        );
    }
    Ok(())
}

fn check_credentials_path(credentials: &str) -> Result<(), ConfigError> {
    if !credentials.is_empty() && !credentials_path_ok(credentials) {
        return Err(
            ConfigError::new("must be a full path (absolute or starting with ~/)")
                .with_path("credentials"),
        );
    }
    Ok(())
}

/// For `hofund config check`: a configured credentials file exists. Building
/// the provider and minting a token do not ask this.
pub(crate) fn check_credentials_file(credentials: &str) -> Result<(), ConfigError> {
    if credentials.is_empty() {
        return Ok(());
    }
    check_credentials_path(credentials)?;
    let file = crate::auth::expand_home(credentials)
        .map_err(|error| ConfigError::new(error.message().to_owned()).with_path("credentials"))?;
    if !file.is_file() {
        return Err(ConfigError::new(
            "names no file: create it, correct the path, or clear it to use Application Default Credentials",
        )
        .with_path("credentials"));
    }
    Ok(())
}

fn credentials_path_ok(path: &str) -> bool {
    path == "~"
        || path.starts_with("~/")
        || path.starts_with("~\\")
        || Path::new(path).is_absolute()
}

fn account_ok(email: &str) -> bool {
    let Some((local, host)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !host.is_empty()
        && !local.contains([' ', '/'])
        && !host.contains([' ', '/'])
        && !email.contains(char::is_whitespace)
}

#[cfg(test)]
mod tests;
