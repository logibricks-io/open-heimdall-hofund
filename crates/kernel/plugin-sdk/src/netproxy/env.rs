//! `EnvWith` and `EnvForBunWith` for subprocess environments.

use std::ffi::OsString;

use hofund_plugin_api::PluginError;
use url::Url;

use super::bridge;

const PROXY_KEYS: &[&str] = &[
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "https_proxy",
    "http_proxy",
    "all_proxy",
    "no_proxy",
];

/// A proxy URL could not be parsed or a SOCKS bridge could not start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyError(pub String);

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProxyError {}

/// Whether `url` is a SOCKS5 proxy address.
#[must_use]
pub fn is_socks(url: &Url) -> bool {
    matches!(url.scheme(), "socks5" | "socks5h")
}

/// Parses a proxy setting; bare `host:port` is treated as HTTP.
///
/// # Errors
///
/// Returns [`ProxyError`] when the address is empty or invalid.
pub fn parse_proxy(value: &str) -> Result<Url, ProxyError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ProxyError("proxy address is empty".into()));
    }
    let text = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    };
    Url::parse(&text).map_err(|error| ProxyError(format!("proxy address is invalid: {error}")))
}

/// Returns a Bun-usable proxy string, bridging SOCKS when needed.
///
/// # Errors
///
/// Returns [`ProxyError`] when the address is invalid or the bridge cannot start.
pub fn for_bun(proxy: &str) -> Result<String, ProxyError> {
    let url = parse_proxy(proxy)?;
    if !is_socks(&url) {
        return Ok(proxy.trim().to_owned());
    }
    bridge::bridge(&url).map(|address| address.to_string())
}

/// Replaces `*_PROXY` in `env` for one provider run. `choice` is the account
/// proxy (`""` keeps existing proxy variables when any are already set).
#[must_use]
pub fn env_with(choice: &str, env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    let trimmed = choice.trim();
    if trimmed.eq_ignore_ascii_case("direct") {
        return strip_proxy_keys(env);
    }
    if trimmed.is_empty() {
        if env.iter().any(has_non_no_proxy) {
            return env.to_vec();
        }
        return env.to_vec();
    }
    let Ok(url) = parse_proxy(trimmed) else {
        return env.to_vec();
    };
    let authority = url
        .host_str()
        .map(|host| {
            if let Some(port) = url.port() {
                format!("{host}:{port}")
            } else {
                host.to_owned()
            }
        })
        .unwrap_or_default();
    let proxy = format!("{}://{authority}", url.scheme());
    with_proxy(env, &proxy, &[])
}

fn has_non_no_proxy((key, value): &(OsString, OsString)) -> bool {
    let Some(text) = key.to_str() else {
        return false;
    };
    if text.eq_ignore_ascii_case("NO_PROXY") {
        return false;
    }
    proxy_key(text) && !value.is_empty()
}

fn proxy_key(key: &str) -> bool {
    PROXY_KEYS.iter().any(|name| key.eq_ignore_ascii_case(name))
}

fn strip_proxy_keys(env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {
    env.iter()
        .filter(|(key, _)| key.to_str().is_none_or(|name| !proxy_key(name)))
        .cloned()
        .collect()
}

fn with_proxy(
    env: &[(OsString, OsString)],
    proxy: &str,
    bypass: &[&str],
) -> Vec<(OsString, OsString)> {
    let mut out = strip_proxy_keys(env);
    if proxy.is_empty() {
        return out;
    }
    let mut no = vec!["localhost", "127.0.0.1", "::1"];
    for entry in bypass {
        let trimmed = entry.trim().trim_start_matches('*');
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("<local>") {
            continue;
        }
        no.push(trimmed);
    }
    let joined = no.join(",");
    for key in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        out.push((OsString::from(key), OsString::from(proxy)));
    }
    out.push((OsString::from("NO_PROXY"), OsString::from(&joined)));
    out.push((OsString::from("no_proxy"), OsString::from(joined)));
    out
}

/// Like [`env_with`], but SOCKS proxies are bridged to loopback HTTP for Bun.
/// When `loopback` is false and the choice is SOCKS, returns an error at once.
///
/// # Errors
///
/// Returns [`PluginError`] when a SOCKS proxy cannot be bridged for Bun.
pub fn env_for_bun_with(
    choice: &str,
    env: &[(OsString, OsString)],
    loopback: bool,
) -> Result<Vec<(OsString, OsString)>, PluginError> {
    let base = env_with(choice, env);
    let mut out = Vec::with_capacity(base.len());
    for (key, value) in base {
        let Some(name) = key.to_str() else {
            out.push((key, value));
            continue;
        };
        if !matches!(
            name.to_ascii_uppercase().as_str(),
            "HTTPS_PROXY" | "HTTP_PROXY" | "ALL_PROXY"
        ) {
            out.push((key, value));
            continue;
        }
        let text = value.to_string_lossy();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            out.push((key, value));
            continue;
        }
        let Ok(url) = parse_proxy(trimmed) else {
            out.push((key, value));
            continue;
        };
        if !is_socks(&url) {
            out.push((key, value));
            continue;
        }
        if !loopback {
            return Err(PluginError::config(format!(
                "the proxy {redacted} is SOCKS, which Claude Code cannot use (UnsupportedProxyProtocol), and hofund's HTTP bridge to it listens on this computer's loopback, which Claude Code in WSL does not reach: set an http:// proxy for it instead",
                redacted = bridge::redact(&url)
            )));
        }
        let http = bridge::bridge(&url).map_err(|error| {
            PluginError::config(format!(
                "the proxy {redacted} is SOCKS, which Claude Code cannot use (UnsupportedProxyProtocol), and hofund's HTTP bridge to it did not start ({error}): set an http:// proxy for it instead",
                redacted = bridge::redact(&url)
            ))
        })?;
        out.push((key, OsString::from(http.to_string())));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{env_for_bun_with, env_with, for_bun, parse_proxy};
    use std::ffi::OsString;

    #[test]
    fn direct_strips_proxy_keys() {
        let env = vec![
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("HTTPS_PROXY"), OsString::from("http://x:1")),
        ];
        let got = env_with("direct", &env);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "PATH");
    }

    #[test]
    fn http_choice_sets_all_proxy_keys() {
        let got = env_with(
            "http://127.0.0.1:7890",
            &[(OsString::from("PATH"), OsString::from("/bin"))],
        );
        assert!(
            got.iter()
                .any(|(k, v)| k == "HTTPS_PROXY" && v == "http://127.0.0.1:7890")
        );
        assert!(got.iter().any(|(k, _)| k == "NO_PROXY"));
    }

    #[test]
    fn parse_proxy_adds_http_scheme() {
        let url = parse_proxy("127.0.0.1:7890").expect("parse");
        assert_eq!(url.scheme(), "http");
    }

    #[test]
    fn for_bun_leaves_http_alone() {
        let text = "http://127.0.0.1:7890";
        assert_eq!(for_bun(text).expect("for bun"), text);
    }

    #[test]
    fn wsl_without_loopback_rejects_socks() {
        let error = env_for_bun_with(
            "socks5://127.0.0.1:1080",
            &[(OsString::from("PATH"), OsString::from("/bin"))],
            false,
        )
        .expect_err("wsl nat");
        assert!(error.to_string().contains("UnsupportedProxyProtocol"));
    }
}
