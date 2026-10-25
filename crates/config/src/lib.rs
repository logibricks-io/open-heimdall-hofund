//! Typed configuration for the local gateway.
//!
//! The file is TOML. This crate checks its shape and reports the first
//! structural error. It does not open sockets and it does not reload a
//! running process.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
    /// How an explicit endpoint speaks. Absent when [`Self::preset`] is set.
    pub kind: Option<Kind>,
    /// Preset id. The daemon fills the endpoint from the preset catalog.
    pub preset: Option<String>,
    /// Preset region. Absent uses the preset's own endpoints.
    pub region: Option<String>,
    /// Protocol selected inside a preset. Absent lets the preset pick.
    pub preset_wire: Option<PresetWire>,
    /// API root. The adapter appends its protocol path.
    ///
    /// Empty when a preset supplies the URL.
    pub base_url: String,
    /// Credential spec. `env:NAME` reads that variable. Any other value is the key.
    pub api_key: String,
    /// Quirk profile id. Absent when the endpoint needs no profile.
    pub quirks: Option<String>,
    /// Extra request headers.
    pub headers: BTreeMap<String, String>,
    /// Output cap used when the client omits `max_tokens`.
    ///
    /// A route member can override this. Absent leaves the cap to a quirk
    /// profile, or to the Messages codec's fallback.
    pub max_output_tokens: Option<u64>,
    /// Google Cloud project id. Used by `preset = "google-vertex"`.
    ///
    /// Empty for every other provider. The value is trimmed and lowercased.
    pub project: String,
    /// Vertex AI location (`global`, `us`, `eu`, or a region). Empty means
    /// `global` when the preset is `google-vertex`.
    pub location: String,
    /// Path of a Google credentials file. Empty uses Application Default
    /// Credentials. The file is not read into this config.
    pub credentials: String,
    /// Service account email the Vertex token is traded for. Empty keeps the
    /// account in the credentials file.
    pub impersonate: String,
    /// AWS access key id for Bedrock Converse. `env:NAME` reads that variable.
    ///
    /// Empty unless `protocol = "converse"` on preset `bedrock`, or
    /// `kind = "converse"`.
    pub access_key_id: String,
    /// AWS secret access key for Bedrock Converse. `env:NAME` reads that
    /// variable. The value is not written to logs.
    pub secret_access_key: String,
    /// AWS session token for Bedrock Converse. Empty for a long-lived key.
    /// `env:NAME` reads that variable.
    pub session_token: String,
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
    /// The Generative Language API (`generateContent`).
    ///
    /// `base_url` is the root `models/{model}:generateContent` is appended to.
    /// A bare host is completed with `/v1beta`. A pasted method URL is cut
    /// back to that root. The API key is sent as `x-goog-api-key`.
    Generative,
    /// Amazon Bedrock Converse.
    ///
    /// `base_url` is the runtime root. `region` is the signing region.
    /// `access_key_id` and `secret_access_key` sign the call. No API key.
    Converse,
}

/// Protocol selected for [`Kind::OpenAiCompat`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// `POST {base}/chat/completions`.
    Chat,
    /// `POST {base}/responses`.
    Responses,
}

/// Protocol selected inside a preset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresetWire {
    /// Chat Completions.
    Chat,
    /// The Responses API.
    Responses,
    /// The Messages API.
    Messages,
    /// The Generative Language API.
    Generative,
    /// Amazon Bedrock Converse.
    Converse,
}

/// One model route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// Model name the client sends.
    pub model: String,
    /// Providers tried in order, each with an optional upstream model name.
    pub upstreams: Vec<Upstream>,
}

/// One provider on a route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    /// Provider id.
    pub provider: String,
    /// Model name sent to this provider.
    ///
    /// Absent means the name the client used, after `provider/model` is
    /// reduced to the model portion.
    pub model: Option<String>,
    /// Output cap for this hop when the client omits `max_tokens`.
    ///
    /// Absent uses the provider's `max_output_tokens`.
    pub max_output_tokens: Option<u64>,
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

/// A config that only sets the default listen address.
///
/// `ensure_config_file` writes this when the user file does not exist yet.
pub const STARTER_CONFIG: &str = "\
# Local hofund gateway.
# Add [[providers]] and [[routes]] before a coding agent calls a model.
listen = \"127.0.0.1:3425\"
";

/// The configuration file the menu-bar app reads.
///
/// `HOFUND_CONFIG` selects the file when it is set and not empty. Otherwise
/// the path is `hofund/hofund.toml` under the platform config directory:
/// `%APPDATA%` on Windows, `~/Library/Application Support` on macOS, and
/// `XDG_CONFIG_HOME` or `~/.config` on other Unix systems.
#[must_use]
pub fn user_config_path() -> PathBuf {
    resolve_config_path(std::env::var_os("HOFUND_CONFIG"), config_home())
}

/// Creates `path` and its parent directories when the file is absent.
///
/// An existing file is left unchanged. Returns whether a new file was written.
///
/// # Errors
///
/// Returns the I/O error when a directory or the file cannot be created.
pub fn ensure_config_file(path: &Path) -> std::io::Result<bool> {
    if path.is_file() {
        return Ok(false);
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, STARTER_CONFIG)?;
    Ok(true)
}

fn resolve_config_path(explicit: Option<std::ffi::OsString>, config_home: PathBuf) -> PathBuf {
    if let Some(path) = explicit.filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    config_home.join("hofund").join("hofund.toml")
}

fn config_home() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map_or_else(|| home_dir().join("AppData").join("Roaming"), PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        home_dir().join("Library").join("Application Support")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        match std::env::var_os("XDG_CONFIG_HOME") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => home_dir().join(".config"),
        }
    }
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    let found = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let found = std::env::var_os("HOME");
    found.map_or_else(|| PathBuf::from("."), PathBuf::from)
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
        let source = source_of(&provider)?;
        if provider.max_output_tokens == Some(0) {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has max_output_tokens 0",
                provider.id
            )));
        }
        let vertex = vertex_of(&provider)?;
        let aws = aws_of(&provider)?;
        providers.push(Provider {
            id: provider.id,
            kind: source.kind,
            preset: source.preset,
            region: source.region,
            preset_wire: source.preset_wire,
            base_url: source.base_url,
            api_key: provider.api_key,
            quirks: provider.quirks,
            headers: provider.headers,
            max_output_tokens: provider.max_output_tokens,
            project: vertex.project,
            location: vertex.location,
            credentials: vertex.credentials,
            impersonate: vertex.impersonate,
            access_key_id: aws.access_key_id,
            secret_access_key: aws.secret_access_key,
            session_token: aws.session_token,
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
        let mut upstreams = Vec::new();
        for upstream in route.upstreams {
            let upstream = upstream_of(&route.model, upstream, &seen)?;
            upstreams.push(upstream);
        }
        routes.push(Route {
            model: route.model,
            upstreams,
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

fn upstream_of(
    route: &str,
    upstream: RawUpstream,
    providers: &BTreeSet<String>,
) -> Result<Upstream, ConfigError> {
    let (provider, model, max_output_tokens) = match upstream {
        RawUpstream::Id(provider) => (provider, None, None),
        RawUpstream::Mapped(mapped) => (mapped.provider, mapped.model, mapped.max_output_tokens),
    };
    if provider.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "route `{route}` names an empty provider"
        )));
    }
    if !providers.contains(&provider) {
        return Err(ConfigError::Invalid(format!(
            "route `{route}` names unknown provider `{provider}`"
        )));
    }
    if model.as_ref().is_some_and(|model| model.is_empty()) {
        return Err(ConfigError::Invalid(format!(
            "route `{route}` has an empty model for provider `{provider}`"
        )));
    }
    if max_output_tokens == Some(0) {
        return Err(ConfigError::Invalid(format!(
            "route `{route}` has max_output_tokens 0 for provider `{provider}`"
        )));
    }
    Ok(Upstream {
        provider,
        model,
        max_output_tokens,
    })
}

struct Source {
    kind: Option<Kind>,
    preset: Option<String>,
    region: Option<String>,
    preset_wire: Option<PresetWire>,
    base_url: String,
}

fn source_of(provider: &RawProvider) -> Result<Source, ConfigError> {
    match (&provider.preset, &provider.kind) {
        (Some(_), Some(_)) => Err(ConfigError::Invalid(format!(
            "provider `{}` sets both preset and kind",
            provider.id
        ))),
        (None, None) => Err(ConfigError::Invalid(format!(
            "provider `{}` needs a kind or a preset",
            provider.id
        ))),
        (None, Some(kind)) => {
            let converse = kind == "converse";
            if provider.region.is_some() && !converse {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` sets region without a preset",
                    provider.id
                )));
            }
            if converse && provider.region.as_deref().unwrap_or("").is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` is Converse and needs a region",
                    provider.id
                )));
            }
            if provider.base_url.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` has an empty base_url",
                    provider.id
                )));
            }
            Ok(Source {
                kind: Some(kind_of(provider, kind)?),
                preset: None,
                region: if converse {
                    provider.region.clone()
                } else {
                    None
                },
                preset_wire: None,
                base_url: provider.base_url.clone(),
            })
        }
        (Some(preset), None) => {
            if preset.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` has an empty preset",
                    provider.id
                )));
            }
            if provider.region.as_ref().is_some_and(String::is_empty) {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` has an empty region",
                    provider.id
                )));
            }
            Ok(Source {
                kind: None,
                preset: Some(preset.clone()),
                region: provider.region.clone(),
                preset_wire: preset_wire_of(provider)?,
                base_url: provider.base_url.clone(),
            })
        }
    }
}

struct VertexFields {
    project: String,
    location: String,
    credentials: String,
    impersonate: String,
}

fn vertex_of(provider: &RawProvider) -> Result<VertexFields, ConfigError> {
    let project = provider.project.trim().to_ascii_lowercase();
    let location = provider.location.trim().to_ascii_lowercase();
    let credentials = provider.credentials.trim().to_owned();
    let impersonate = provider.impersonate.trim().to_ascii_lowercase();
    let set = !project.is_empty()
        || !location.is_empty()
        || !credentials.is_empty()
        || !impersonate.is_empty();
    let is_vertex = provider.preset.as_deref() == Some("google-vertex");
    if set && !is_vertex {
        return Err(ConfigError::Invalid(format!(
            "provider `{}` sets project, location, credentials, or impersonate, which only preset `google-vertex` uses",
            provider.id
        )));
    }
    if is_vertex && !provider.api_key.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "provider `{}` is Google Vertex AI and does not take an API key",
            provider.id
        )));
    }
    if is_vertex && provider.protocol.is_some() {
        return Err(ConfigError::Invalid(format!(
            "provider `{}` is Google Vertex AI and has no protocol choice",
            provider.id
        )));
    }
    Ok(VertexFields {
        project,
        location,
        credentials,
        impersonate,
    })
}

struct AwsFields {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

fn aws_of(provider: &RawProvider) -> Result<AwsFields, ConfigError> {
    let access_key_id = provider.access_key_id.trim().to_owned();
    let secret_access_key = provider.secret_access_key.trim().to_owned();
    let session_token = provider.session_token.trim().to_owned();
    let set =
        !access_key_id.is_empty() || !secret_access_key.is_empty() || !session_token.is_empty();
    let converse = (provider.preset.as_deref() == Some("bedrock")
        && provider.protocol.as_deref() == Some("converse"))
        || provider.kind.as_deref() == Some("converse");
    if set && !converse {
        return Err(ConfigError::Invalid(format!(
            "provider `{}` sets access_key_id, secret_access_key, or session_token, which only Bedrock Converse uses",
            provider.id
        )));
    }
    if converse && access_key_id.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "provider `{}` is Converse and needs access_key_id",
            provider.id
        )));
    }
    if converse && secret_access_key.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "provider `{}` is Converse and needs secret_access_key",
            provider.id
        )));
    }
    if converse && !provider.api_key.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "provider `{}` is Converse and does not take an API key",
            provider.id
        )));
    }
    Ok(AwsFields {
        access_key_id,
        secret_access_key,
        session_token,
    })
}

fn preset_wire_of(provider: &RawProvider) -> Result<Option<PresetWire>, ConfigError> {
    match provider.protocol.as_deref() {
        None => Ok(None),
        Some("chat") => Ok(Some(PresetWire::Chat)),
        Some("responses") => Ok(Some(PresetWire::Responses)),
        Some("messages") => Ok(Some(PresetWire::Messages)),
        Some("generative") => Ok(Some(PresetWire::Generative)),
        Some("converse") => {
            if provider.preset.as_deref() != Some("bedrock") {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` uses protocol `converse`, which only preset `bedrock` has",
                    provider.id
                )));
            }
            Ok(Some(PresetWire::Converse))
        }
        Some(other) => Err(ConfigError::Invalid(format!(
            "provider `{}` has unknown protocol `{other}`",
            provider.id
        ))),
    }
}

fn kind_of(provider: &RawProvider, kind: &str) -> Result<Kind, ConfigError> {
    match kind {
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
        "generative" => {
            if provider.protocol.is_some() {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` does not take a protocol",
                    provider.id
                )));
            }
            Ok(Kind::Generative)
        }
        "converse" => {
            if provider.protocol.is_some() {
                return Err(ConfigError::Invalid(format!(
                    "provider `{}` does not take a protocol",
                    provider.id
                )));
            }
            Ok(Kind::Converse)
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
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    quirks: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    project: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    credentials: String,
    #[serde(default)]
    impersonate: String,
    #[serde(default)]
    access_key_id: String,
    #[serde(default)]
    secret_access_key: String,
    #[serde(default)]
    session_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    model: String,
    upstreams: Vec<RawUpstream>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawUpstream {
    Id(String),
    Mapped(RawMappedUpstream),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMappedUpstream {
    provider: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

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
            Some(Kind::OpenAiCompat {
                protocol: Protocol::Responses
            })
        ));
        assert!(config.providers[0].preset.is_none());
        assert_eq!(config.routes[0].upstreams[0].provider, "primary");
        assert_eq!(config.routes[0].upstreams[0].model, None);
        assert_eq!(config.providers[0].max_output_tokens, None);
        assert_eq!(config.routes[0].upstreams[0].max_output_tokens, None);
    }

    #[test]
    fn a_route_member_can_name_its_upstream_model() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "anthropic"
            base_url = "https://example.test"
            [[providers]]
            id = "backup"
            kind = "openai_compat"
            base_url = "http://127.0.0.1:9/v1"
            [[routes]]
            model = "demo"
            upstreams = [
              { provider = "primary", model = "claude-sonnet" },
              "backup",
            ]
            "#,
        )
        .expect("config");
        assert_eq!(
            config.routes[0].upstreams[0].model.as_deref(),
            Some("claude-sonnet")
        );
        assert_eq!(config.routes[0].upstreams[1].model, None);
    }

    #[test]
    fn max_output_tokens_can_be_set_on_a_provider_and_a_route_member() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "anthropic"
            base_url = "https://example.test"
            max_output_tokens = 8192
            [[routes]]
            model = "demo"
            upstreams = [{ provider = "primary", max_output_tokens = 1024 }]
            "#,
        )
        .expect("config");
        assert_eq!(config.providers[0].max_output_tokens, Some(8192));
        assert_eq!(config.routes[0].upstreams[0].max_output_tokens, Some(1024));
        let error = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "anthropic"
            base_url = "https://example.test"
            max_output_tokens = 0
            "#,
        )
        .expect_err("zero");
        assert!(error.to_string().contains("max_output_tokens"));
        let error = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "anthropic"
            base_url = "https://example.test"
            [[routes]]
            model = "demo"
            upstreams = [{ provider = "primary", model = "" }]
            "#,
        )
        .expect_err("empty model");
        assert!(error.to_string().contains("empty model"));
    }

    #[test]
    fn a_generative_provider_parses_without_a_protocol() {
        let config = load(
            r#"
            [[providers]]
            id = "studio"
            kind = "generative"
            base_url = "https://generativelanguage.googleapis.com"
            api_key = "env:GEMINI_API_KEY"
            "#,
        )
        .expect("config");
        assert!(matches!(config.providers[0].kind, Some(Kind::Generative)));
        let error = load(
            r#"
            [[providers]]
            id = "studio"
            kind = "generative"
            protocol = "chat"
            base_url = "https://example.test"
            "#,
        )
        .expect_err("protocol");
        assert!(error.to_string().contains("does not take a protocol"));
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

    #[test]
    fn converse_keeps_the_signing_keys_and_refuses_an_api_key() {
        let config = load(
            r#"
            [[providers]]
            id = "bedrock"
            preset = "bedrock"
            region = "us-east-1"
            protocol = "converse"
            access_key_id = "AKIAIOSFODNN7EXAMPLE"
            secret_access_key = "secret"
            session_token = "token"
            "#,
        )
        .expect("converse");
        let provider = &config.providers[0];
        assert_eq!(provider.preset_wire, Some(super::PresetWire::Converse));
        assert_eq!(provider.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(provider.secret_access_key, "secret");
        assert_eq!(provider.session_token, "token");
        let error = load(
            r#"
            [[providers]]
            id = "bedrock"
            preset = "bedrock"
            protocol = "converse"
            api_key = "ABSK"
            access_key_id = "AKIA"
            secret_access_key = "secret"
            "#,
        )
        .expect_err("api key");
        assert!(error.to_string().contains("does not take an API key"));
        let error = load(
            r#"
            [[providers]]
            id = "openai"
            preset = "openai"
            access_key_id = "AKIA"
            "#,
        )
        .expect_err("openai key");
        assert!(error.to_string().contains("only Bedrock Converse"));
    }

    #[test]
    fn a_preset_does_not_require_a_kind_or_a_base_url() {
        let config = load(
            r#"
            [[providers]]
            id = "openai"
            preset = "openai"
            protocol = "responses"
            "#,
        )
        .expect("preset");
        assert!(config.providers[0].kind.is_none());
        assert_eq!(config.providers[0].preset.as_deref(), Some("openai"));
        assert_eq!(
            config.providers[0].preset_wire,
            Some(super::PresetWire::Responses)
        );
        let google = load(
            r#"
            [[providers]]
            id = "gemini"
            preset = "google"
            protocol = "generative"
            "#,
        )
        .expect("google");
        assert_eq!(
            google.providers[0].preset_wire,
            Some(super::PresetWire::Generative)
        );
        let error = load(
            r#"
            [[providers]]
            id = "both"
            kind = "anthropic"
            preset = "anthropic"
            base_url = "https://example.test"
            "#,
        )
        .expect_err("both");
        assert!(error.to_string().contains("both preset and kind"));
    }

    #[test]
    fn a_vertex_preset_keeps_the_project_and_refuses_a_key() {
        let config = load(
            r#"
            [[providers]]
            id = "vertex"
            preset = "google-vertex"
            project = "My-Project"
            location = "US"
            credentials = "/tmp/adc.json"
            impersonate = "Worker@Example.com"
            "#,
        )
        .expect("vertex");
        let provider = &config.providers[0];
        assert_eq!(provider.project, "my-project");
        assert_eq!(provider.location, "us");
        assert_eq!(provider.credentials, "/tmp/adc.json");
        assert_eq!(provider.impersonate, "worker@example.com");
        let error = load(
            r#"
            [[providers]]
            id = "openai"
            preset = "openai"
            project = "my-project"
            "#,
        )
        .expect_err("project on openai");
        assert!(error.to_string().contains("google-vertex"));
        let error = load(
            r#"
            [[providers]]
            id = "vertex"
            preset = "google-vertex"
            project = "my-project"
            api_key = "secret"
            "#,
        )
        .expect_err("key");
        assert!(error.to_string().contains("does not take an API key"));
    }

    #[test]
    fn an_explicit_path_wins_over_the_config_home() {
        let path = super::resolve_config_path(
            Some(std::ffi::OsString::from("/tmp/hofund.toml")),
            PathBuf::from("/unused"),
        );
        assert_eq!(path, PathBuf::from("/tmp/hofund.toml"));
        let path = super::resolve_config_path(None, PathBuf::from("/cfg"));
        assert_eq!(path, PathBuf::from("/cfg/hofund/hofund.toml"));
        let path =
            super::resolve_config_path(Some(std::ffi::OsString::new()), PathBuf::from("/cfg"));
        assert_eq!(path, PathBuf::from("/cfg/hofund/hofund.toml"));
    }

    #[test]
    fn the_example_config_loads() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/hofund.toml");
        let text = std::fs::read_to_string(&path).expect("example");
        let config = load(&text).expect("load");
        assert!(config.providers.len() >= 2);
        assert!(!config.routes.is_empty());
        assert!(config.routes.iter().any(|route| route.upstreams.len() >= 2));
    }

    #[test]
    fn ensure_config_file_writes_once() {
        let dir = std::env::temp_dir().join(format!("hofund-config-{}", std::process::id()));
        let path = dir.join("hofund.toml");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(super::ensure_config_file(&path).expect("create"));
        assert!(path.is_file());
        std::fs::write(&path, "listen = \"127.0.0.1:9\"\n").expect("overwrite");
        assert!(!super::ensure_config_file(&path).expect("keep"));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("127.0.0.1:9"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
