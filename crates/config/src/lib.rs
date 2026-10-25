//! Typed configuration for the local gateway.
//!
//! The file is TOML. This crate checks its shape and reports the first
//! structural error. It does not open sockets and it does not reload a
//! running process.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// Socket connect budget for the pooled HTTP client.
    ///
    /// This is not the kernel idle deadline. That stays on each provider endpoint.
    pub connect_timeout: Duration,
    /// Idle time allowed between socket reads on the pooled HTTP client.
    pub read_timeout: Duration,
    /// Upstream endpoints, in file order.
    pub providers: Vec<Provider>,
    /// Model routes, in file order.
    pub routes: Vec<Route>,
    /// Account subscriptions, in file order.
    pub subscriptions: Vec<Subscription>,
    /// Deprecated keys that still loaded. The next release rejects them.
    pub warnings: Vec<String>,
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
///
/// Vendor settings live in [`Self::options`]. This struct does not name them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// Stable id used by routes.
    pub id: String,
    /// Preset id. The daemon fills the endpoint from the preset catalog.
    pub preset: Option<String>,
    /// Codec id, such as `openai-chat`. Absent when the preset has one protocol.
    ///
    /// The daemon converts this with `ProtocolId::try_from`. This crate stores
    /// the string and does not name that type.
    pub protocol: Option<String>,
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
    /// Plugin settings. The plugin validates the keys.
    pub options: BTreeMap<String, String>,
}

/// Which accounts of one subscription become router members.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Accounts {
    /// Every account stored for this subscription's plugin.
    All,
    /// One account id.
    One(String),
}

/// One subscription. The plugin validates [`Self::options`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    /// Subscription plugin id.
    pub id: String,
    /// Which accounts to serve.
    pub accounts: Accounts,
    /// Vendor host, client id, and any other plugin setting.
    pub options: BTreeMap<String, String>,
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
    let mut warnings = Vec::new();
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
        if provider.preset.is_none() && provider.protocol.is_none() {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` needs a protocol or a preset",
                provider.id
            )));
        }
        if provider.preset.as_ref().is_some_and(String::is_empty) {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty preset",
                provider.id
            )));
        }
        if provider.protocol.as_ref().is_some_and(String::is_empty) {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty protocol",
                provider.id
            )));
        }
        if provider.preset.is_none() && provider.base_url.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty base_url",
                provider.id
            )));
        }
        if provider.max_output_tokens == Some(0) {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has max_output_tokens 0",
                provider.id
            )));
        }
        let options = options_of(&provider, &mut warnings)?;
        providers.push(Provider {
            id: provider.id,
            preset: provider.preset,
            protocol: provider.protocol,
            base_url: provider.base_url,
            api_key: provider.api_key,
            quirks: provider.quirks,
            headers: provider.headers,
            max_output_tokens: provider.max_output_tokens,
            options,
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
    let mut subscriptions = Vec::new();
    let mut subscription_ids = BTreeSet::new();
    for subscription in raw.subscriptions {
        if !kebab(&subscription.id) {
            return Err(ConfigError::Invalid(format!(
                "subscription id `{}` is not kebab-case",
                subscription.id
            )));
        }
        if !subscription_ids.insert(subscription.id.clone()) {
            return Err(ConfigError::Invalid(format!(
                "subscription id `{}` is repeated",
                subscription.id
            )));
        }
        let accounts = match subscription.accounts.as_str() {
            "*" => Accounts::All,
            id if kebab(id) => Accounts::One(id.to_owned()),
            _ => {
                return Err(ConfigError::Invalid(format!(
                    "subscription `{}` accounts must be `*` or an account id",
                    subscription.id
                )));
            }
        };
        for key in subscription.options.keys() {
            if key.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "subscription `{}` has an empty option key",
                    subscription.id
                )));
            }
        }
        subscriptions.push(Subscription {
            id: subscription.id,
            accounts,
            options: subscription.options,
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
        connect_timeout: duration_ms("connect_timeout_ms", raw.connect_timeout_ms)?,
        read_timeout: duration_ms("read_timeout_ms", raw.read_timeout_ms)?,
        providers,
        routes,
        subscriptions,
        warnings,
    })
}

fn options_of(
    provider: &RawProvider,
    warnings: &mut Vec<String>,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut options = BTreeMap::new();
    for (key, value) in &provider.options {
        if key.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty option key",
                provider.id
            )));
        }
        options.insert(key.clone(), value.clone());
    }
    for (key, value) in legacy_options(provider) {
        let Some(value) = value else {
            continue;
        };
        if value.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty `{key}`",
                provider.id
            )));
        }
        if let Some(existing) = options.get(key)
            && existing != value
        {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` sets `{key}` and options.{key} to different values",
                provider.id
            )));
        }
        options.insert((*key).to_owned(), value.to_owned());
        warnings.push(format!(
            "provider `{}`: `{key}` moved to [providers.options]; the old key still loads",
            provider.id
        ));
    }
    Ok(options)
}

fn legacy_options(provider: &RawProvider) -> [(&str, &Option<String>); 8] {
    [
        ("project", &provider.project),
        ("location", &provider.location),
        ("credentials", &provider.credentials),
        ("impersonate", &provider.impersonate),
        ("access_key_id", &provider.access_key_id),
        ("secret_access_key", &provider.secret_access_key),
        ("session_token", &provider.session_token),
        ("region", &provider.region),
    ]
}

fn kebab(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut prev_hyphen = true;
    for byte in bytes {
        if *byte == b'-' {
            if prev_hyphen {
                return false;
            }
            prev_hyphen = true;
        } else if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            prev_hyphen = false;
        } else {
            return false;
        }
    }
    !prev_hyphen
}

fn duration_ms(name: &str, millis: u64) -> Result<Duration, ConfigError> {
    if millis == 0 {
        return Err(ConfigError::Invalid(format!("{name} is 0")));
    }
    let nanos = millis
        .checked_mul(1_000_000)
        .ok_or_else(|| ConfigError::Invalid(format!("{name} is too large")))?;
    Ok(Duration::from_nanos(nanos))
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

fn default_connect_timeout_ms() -> u64 {
    10_000
}

fn default_read_timeout_ms() -> u64 {
    60_000
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
    #[serde(default = "default_connect_timeout_ms")]
    connect_timeout_ms: u64,
    #[serde(default = "default_read_timeout_ms")]
    read_timeout_ms: u64,
    #[serde(default)]
    providers: Vec<RawProvider>,
    #[serde(default)]
    routes: Vec<RawRoute>,
    #[serde(default)]
    subscriptions: Vec<RawSubscription>,
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
    preset: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    quirks: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    options: BTreeMap<String, String>,
    /// Accepted for one release, then copied into [`Self::options`].
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    credentials: Option<String>,
    #[serde(default)]
    impersonate: Option<String>,
    #[serde(default)]
    access_key_id: Option<String>,
    #[serde(default)]
    secret_access_key: Option<String>,
    #[serde(default)]
    session_token: Option<String>,
    #[serde(default)]
    region: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubscription {
    id: String,
    accounts: String,
    #[serde(default)]
    options: BTreeMap<String, String>,
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
    use std::time::Duration;

    use super::{Accounts, load};

    #[test]
    fn defaults_and_a_route_parse() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            protocol = "openai-responses"
            base_url = "http://127.0.0.1:9/v1"
            [[routes]]
            model = "demo"
            upstreams = ["primary"]
            "#,
        )
        .expect("config");
        assert_eq!(config.listen.to_string(), "127.0.0.1:3425");
        assert_eq!(config.failover.statuses, [429, 500, 502, 503, 504].into());
        assert_eq!(
            config.providers[0].protocol.as_deref(),
            Some("openai-responses")
        );
        assert!(config.providers[0].preset.is_none());
        assert!(config.warnings.is_empty());
        assert!(config.subscriptions.is_empty());
        assert_eq!(config.routes[0].upstreams[0].provider, "primary");
        assert_eq!(config.routes[0].upstreams[0].model, None);
        assert_eq!(config.providers[0].max_output_tokens, None);
        assert_eq!(config.routes[0].upstreams[0].max_output_tokens, None);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.read_timeout, Duration::from_secs(60));
    }

    #[test]
    fn pooled_timeouts_parse_and_reject_zero_or_overflow() {
        let config = load(
            r#"
            connect_timeout_ms = 1500
            read_timeout_ms = 2500
            "#,
        )
        .expect("config");
        assert_eq!(config.connect_timeout, Duration::from_millis(1500));
        assert_eq!(config.read_timeout, Duration::from_millis(2500));
        let zero = load("connect_timeout_ms = 0\n").expect_err("zero");
        assert!(zero.to_string().contains("connect_timeout_ms is 0"));
        let huge = load("read_timeout_ms = 18446744073710\n").expect_err("overflow");
        assert!(huge.to_string().contains("read_timeout_ms is too large"));
    }

    #[test]
    fn a_route_member_can_name_its_upstream_model() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            protocol = "anthropic-messages"
            base_url = "https://example.test"
            [[providers]]
            id = "backup"
            protocol = "openai-chat"
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
            protocol = "anthropic-messages"
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
            protocol = "anthropic-messages"
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
            protocol = "anthropic-messages"
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
    fn a_protocol_string_is_not_checked_against_the_codec_ids() {
        let config = load(
            r#"
            [[providers]]
            id = "studio"
            protocol = "generative-language"
            base_url = "https://generativelanguage.googleapis.com"
            api_key = "env:GEMINI_API_KEY"
            "#,
        )
        .expect("config");
        assert_eq!(
            config.providers[0].protocol.as_deref(),
            Some("generative-language")
        );
        let stored = load(
            r#"
            [[providers]]
            id = "studio"
            protocol = "chat"
            base_url = "https://example.test"
            "#,
        )
        .expect("short name is a string");
        assert_eq!(stored.providers[0].protocol.as_deref(), Some("chat"));
    }

    #[test]
    fn the_first_structural_error_is_reported() {
        let error = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "openai_compat"
            base_url = "http://127.0.0.1:9"
            "#,
        )
        .expect_err("kind");
        assert!(error.to_string().contains("kind"));

        let error = load("listen = \"not-an-address\"\n").expect_err("listen");
        assert!(error.to_string().contains("listen"));
    }

    #[test]
    fn old_vendor_keys_move_into_options_and_warn() {
        let config = load(
            r#"
            [[providers]]
            id = "bedrock"
            preset = "bedrock"
            protocol = "bedrock-converse"
            region = "us-east-1"
            access_key_id = "AKIAIOSFODNN7EXAMPLE"
            secret_access_key = "secret"
            session_token = "token"
            "#,
        )
        .expect("converse");
        let provider = &config.providers[0];
        assert_eq!(provider.protocol.as_deref(), Some("bedrock-converse"));
        assert_eq!(
            provider.options.get("access_key_id").map(String::as_str),
            Some("AKIAIOSFODNN7EXAMPLE")
        );
        assert_eq!(
            provider
                .options
                .get("secret_access_key")
                .map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            provider.options.get("region").map(String::as_str),
            Some("us-east-1")
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("`region` moved"))
        );
        let same = load(
            r#"
            [[providers]]
            id = "bedrock"
            preset = "bedrock"
            protocol = "bedrock-converse"
            region = "us-east-1"
            [providers.options]
            region = "us-east-1"
            "#,
        )
        .expect("same value");
        assert_eq!(
            same.providers[0].options.get("region").map(String::as_str),
            Some("us-east-1")
        );
        let error = load(
            r#"
            [[providers]]
            id = "bedrock"
            preset = "bedrock"
            region = "us-east-1"
            [providers.options]
            region = "eu-west-1"
            "#,
        )
        .expect_err("conflict");
        assert!(error.to_string().contains("different values"));
    }

    #[test]
    fn a_preset_does_not_require_a_protocol_or_a_base_url() {
        let config = load(
            r#"
            [[providers]]
            id = "openai"
            preset = "openai"
            protocol = "openai-responses"
            "#,
        )
        .expect("preset");
        assert_eq!(config.providers[0].preset.as_deref(), Some("openai"));
        assert_eq!(
            config.providers[0].protocol.as_deref(),
            Some("openai-responses")
        );
        let google = load(
            r#"
            [[providers]]
            id = "gemini"
            preset = "google"
            "#,
        )
        .expect("google");
        assert!(google.providers[0].protocol.is_none());
        let error = load(
            r#"
            [[providers]]
            id = "bare"
            base_url = "https://example.test"
            "#,
        )
        .expect_err("neither");
        assert!(error.to_string().contains("protocol or a preset"));
    }

    #[test]
    fn options_keep_the_vertex_settings_the_plugin_validates() {
        let config = load(
            r#"
            [[providers]]
            id = "vertex"
            preset = "google-vertex"
            api_key = "secret"
            [providers.options]
            project = "My-Project"
            location = "US"
            credentials = "/tmp/adc.json"
            impersonate = "Worker@Example.com"
            "#,
        )
        .expect("vertex");
        let provider = &config.providers[0];
        assert_eq!(
            provider.options.get("project").map(String::as_str),
            Some("My-Project")
        );
        assert_eq!(
            provider.options.get("location").map(String::as_str),
            Some("US")
        );
        assert!(config.warnings.is_empty());
        let legacy = load(
            r#"
            [[providers]]
            id = "vertex"
            preset = "google-vertex"
            project = "my-project"
            "#,
        )
        .expect("legacy project");
        assert_eq!(
            legacy.providers[0]
                .options
                .get("project")
                .map(String::as_str),
            Some("my-project")
        );
        assert!(
            legacy
                .warnings
                .iter()
                .any(|warning| warning.contains("project"))
        );
    }

    #[test]
    fn a_subscription_names_accounts_and_opaque_options() {
        let config = load(
            r#"
            [[subscriptions]]
            id = "copilot"
            accounts = "*"
            [subscriptions.options]
            host = "example.ghe.com"
            client_id = "Iv1.example"
            [[subscriptions]]
            id = "codex"
            accounts = "work"
            "#,
        )
        .expect("subscriptions");
        assert_eq!(config.subscriptions[0].accounts, Accounts::All);
        assert_eq!(
            config.subscriptions[0]
                .options
                .get("host")
                .map(String::as_str),
            Some("example.ghe.com")
        );
        assert_eq!(
            config.subscriptions[1].accounts,
            Accounts::One("work".to_owned())
        );
        let error = load(
            r#"
            [[subscriptions]]
            id = "Copilot"
            accounts = "*"
            "#,
        )
        .expect_err("id");
        assert!(error.to_string().contains("kebab-case"));
        let error = load(
            r#"
            [[subscriptions]]
            id = "codex"
            accounts = "Work"
            "#,
        )
        .expect_err("account");
        assert!(error.to_string().contains("account id"));
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
