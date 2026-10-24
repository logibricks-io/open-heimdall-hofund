//! Typed configuration for the local gateway.
//!
//! The file is TOML. This crate checks its shape and reports the first
//! structural error. It does not open sockets and it does not reload a
//! running process.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

/// A validated configuration file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Address the gateway binds.
    pub listen: SocketAddr,
    /// Path of the bearer-token file, as written in the TOML.
    pub token_file: String,
    /// Path of the usage database, as written in the TOML.
    pub usage_file: String,
    /// Which upstream failures may try the next provider.
    pub failover: Failover,
    /// Upstream endpoints, in file order.
    pub providers: Vec<Provider>,
    /// Model routes, in file order.
    pub routes: Vec<Route>,
}

/// Failover thresholds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failover {
    /// Connect and timeout errors try the next provider.
    pub connect_errors: bool,
    /// HTTP statuses that try the next provider.
    pub statuses: BTreeSet<u16>,
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            connect_errors: true,
            statuses: [429, 500, 502, 503, 504].into_iter().collect(),
        }
    }
}

/// One upstream endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// Stable id used by routes.
    pub id: String,
    /// How the endpoint speaks.
    pub kind: Kind,
    /// API root. The adapter appends its protocol path.
    pub base_url: String,
    /// Credential spec. `env:NAME` reads that variable. Any other value is the key.
    pub api_key: String,
    /// Quirk profile id. Absent when the endpoint needs no profile.
    pub quirks: Option<String>,
    /// Extra request headers.
    pub headers: BTreeMap<String, String>,
}

/// Protocol family of one provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Chat Completions or the Responses API.
    OpenAiCompat {
        /// Which of the two protocols the endpoint expects.
        protocol: Protocol,
    },
    /// The Messages API.
    Anthropic,
}

/// Protocol selected for [`Kind::OpenAiCompat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// `POST {base}/chat/completions`.
    Chat,
    /// `POST {base}/responses`.
    Responses,
}

/// One model route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// Model name the client sends.
    pub model: String,
    /// Provider ids, tried in order.
    pub upstreams: Vec<String>,
}

/// The first structural problem in a configuration file.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read config: {0}")]
    Read(String),
    /// The TOML could not be parsed into the expected shape.
    #[error("config syntax: {0}")]
    Syntax(String),
    /// A field is present but not usable.
    #[error("config: {0}")]
    Invalid(String),
}

/// Reads `path` and returns the first error it finds.
///
/// # Errors
///
/// Returns [`ConfigError`] when the file cannot be read or the first checked
/// field is unusable.
pub fn load_path(path: &Path) -> Result<Config, ConfigError> {
    let text =
        std::fs::read_to_string(path).map_err(|error| ConfigError::Read(error.to_string()))?;
    load(&text)
}

/// Parses `text` and returns the first error it finds.
///
/// # Errors
///
/// Returns [`ConfigError`] when the TOML is not the expected shape.
pub fn load(text: &str) -> Result<Config, ConfigError> {
    let raw: RawFile =
        toml_edit::de::from_str(text).map_err(|error| ConfigError::Syntax(error.to_string()))?;
    validate(raw)
}

fn validate(raw: RawFile) -> Result<Config, ConfigError> {
    let listen = raw.listen.parse::<SocketAddr>().map_err(|_| {
        ConfigError::Invalid(format!("listen `{}` is not an ip:port address", raw.listen))
    })?;
    if raw.token_file.is_empty() {
        return Err(ConfigError::Invalid("token_file is empty".to_owned()));
    }
    if raw.usage_file.is_empty() {
        return Err(ConfigError::Invalid("usage_file is empty".to_owned()));
    }
    let mut seen = BTreeSet::new();
    let mut providers = Vec::new();
    for provider in raw.providers {
        if provider.id.is_empty() {
            return Err(ConfigError::Invalid("a provider id is empty".to_owned()));
        }
        if !seen.insert(provider.id.clone()) {
            return Err(ConfigError::Invalid(format!(
                "provider id `{}` is repeated",
                provider.id
            )));
        }
        if provider.base_url.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty base_url",
                provider.id
            )));
        }
        let kind = kind_of(&provider)?;
        providers.push(Provider {
            id: provider.id,
            kind,
            base_url: provider.base_url,
            api_key: provider.api_key,
            quirks: provider.quirks,
            headers: provider.headers,
        });
    }
    let mut models = BTreeSet::new();
    let mut routes = Vec::new();
    for route in raw.routes {
        if route.model.is_empty() {
            return Err(ConfigError::Invalid("a route model is empty".to_owned()));
        }
        if !models.insert(route.model.clone()) {
            return Err(ConfigError::Invalid(format!(
                "route model `{}` is repeated",
                route.model
            )));
        }
        if route.upstreams.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "route `{}` has no upstream",
                route.model
            )));
        }
        for upstream in &route.upstreams {
            if !seen.contains(upstream) {
                return Err(ConfigError::Invalid(format!(
                    "route `{}` names unknown provider `{upstream}`",
                    route.model
                )));
            }
        }
        routes.push(Route {
            model: route.model,
            upstreams: route.upstreams,
        });
    }
    Ok(Config {
        listen,
        token_file: raw.token_file,
        usage_file: raw.usage_file,
        failover: Failover {
            connect_errors: raw.failover.connect_errors,
            statuses: raw.failover.statuses,
        },
        providers,
        routes,
    })
}

fn kind_of(provider: &RawProvider) -> Result<Kind, ConfigError> {
    match provider.kind.as_str() {
        "openai_compat" => {
            let protocol = match provider.protocol.as_deref() {
                None | Some("chat") => Protocol::Chat,
                Some("responses") => Protocol::Responses,
                Some(other) => {
                    return Err(ConfigError::Invalid(format!(
                        "provider `{}` has unknown protocol `{other}`",
                        provider.id
                    )));
                }
            };
            Ok(Kind::OpenAiCompat { protocol })
        }
        "anthropic" => {
            if provider.protocol.is_some() {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` does not take a protocol",
                    provider.id
                )));
            }
            Ok(Kind::Anthropic)
        }
        other => Err(ConfigError::Invalid(format!(
            "provider `{}` has unknown kind `{other}`",
            provider.id
        ))),
    }
}

fn default_listen() -> String {
    "127.0.0.1:3425".to_owned()
}

fn default_token_file() -> String {
    "hofund.token".to_owned()
}

fn default_usage_file() -> String {
    "hofund-usage.sqlite".to_owned()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_token_file")]
    token_file: String,
    #[serde(default = "default_usage_file")]
    usage_file: String,
    #[serde(default)]
    failover: RawFailover,
    #[serde(default)]
    providers: Vec<RawProvider>,
    #[serde(default)]
    routes: Vec<RawRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFailover {
    #[serde(default = "default_true")]
    connect_errors: bool,
    #[serde(default = "default_statuses")]
    statuses: BTreeSet<u16>,
}

impl Default for RawFailover {
    fn default() -> Self {
        Self {
            connect_errors: true,
            statuses: default_statuses(),
        }
    }
}

fn default_statuses() -> BTreeSet<u16> {
    [429, 500, 502, 503, 504].into_iter().collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    id: String,
    kind: String,
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    quirks: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    model: String,
    upstreams: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{Kind, Protocol, load};

    #[test]
    fn defaults_and_a_route_parse() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "openai_compat"
            base_url = "http://127.0.0.1:9/v1"
            protocol = "responses"
            [[routes]]
            model = "demo"
            upstreams = ["primary"]
            "#,
        )
        .expect("config");
        assert_eq!(config.listen.to_string(), "127.0.0.1:3425");
        assert_eq!(config.failover.statuses, [429, 500, 502, 503, 504].into());
        assert!(matches!(
            config.providers[0].kind,
            Kind::OpenAiCompat {
                protocol: Protocol::Responses
            }
        ));
    }

    #[test]
    fn the_first_structural_error_is_reported() {
        let error = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "nope"
            base_url = "http://127.0.0.1:9"
            "#,
        )
        .expect_err("kind");
        assert!(error.to_string().contains("unknown kind"));

        let error = load("listen = \"not-an-address\"\n").expect_err("listen");
        assert!(error.to_string().contains("listen"));
    }
}
