//! Typed configuration for the local gateway.
//!
//! The file is TOML. This crate checks its shape and reports the first
//! structural error. It does not open sockets and it does not reload a
//! running process.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// A validated configuration file.
///
/// [`Debug`] redacts every provider API key, header value, and options value.
/// Option keys and header names stay visible. The options table is opaque and
/// may hold secrets, so values are redacted rather than matched against an
/// allowlist of safe keys.
#[derive(Clone, PartialEq, Eq)]
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
            statuses: [401, 402, 403, 404, 408, 429]
                .into_iter()
                .chain(500..=599)
                .collect(),
        }
    }
}

/// How long a direct request to one provider, or a request to a route, waits
/// out busy failures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Patience {
    /// Wait for the default 60 seconds.
    #[default]
    Default,
    /// Return the failure without another attempt.
    Off,
    /// Wait for a validated custom budget.
    Wait(Duration),
}

impl Patience {
    /// The effective retry budget, or `None` when patience is off.
    #[must_use]
    pub fn duration(self) -> Option<Duration> {
        match self {
            Self::Default => Some(Duration::from_secs(60)),
            Self::Off => None,
            Self::Wait(duration) => Some(duration),
        }
    }
}

/// One value in an opaque options table.
///
/// The shapes match the plugin table: string, integer, boolean, array, and
/// table. A float or a datetime is a syntax error. Numbers are integers.
///
/// A string that is empty or only whitespace is absent. The key is omitted,
/// including a string nested in a table. That is not an error and not a
/// warning. A blank string inside an array is kept, because an array element
/// is not a key. `0`, `false`, an empty array, and an empty table are values
/// and are kept.
///
/// [`Debug`] prints `<redacted>` for every leaf. Keys of a nested table stay
/// visible.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Value {
    /// A string.
    String(String),
    /// An integer.
    Integer(i64),
    /// A boolean.
    Boolean(bool),
    /// An ordered list.
    Array(Vec<Value>),
    /// A nested table.
    Table(BTreeMap<String, Value>),
}

impl Value {
    /// The string, when this value is a string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::String(_) | Self::Integer(_) | Self::Boolean(_) => {
                formatter.write_str("\"<redacted>\"")
            }
            Self::Array(items) => formatter.debug_list().entries(items).finish(),
            Self::Table(table) => formatter.debug_map().entries(table).finish(),
        }
    }
}

/// How long a conversation stays with the member that answered it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stays {
    /// Stay within a turn and across turns while the prompt cache is warm.
    #[default]
    Auto,
    /// Stay for the whole conversation unless the member must be bypassed.
    Session,
    /// Stay only while tool results continue the current turn.
    Turn,
    /// Always let routing choose again.
    Off,
    /// Never move to another member while the answerer remains configured.
    Strict,
}

impl Stays {
    /// The configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Session => "session",
            Self::Turn => "turn",
            Self::Off => "off",
            Self::Strict => "strict",
        }
    }
}

impl fmt::Display for Stays {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One upstream endpoint.
///
/// Vendor settings live in [`Self::options`]. This struct does not name them.
#[derive(Clone, PartialEq, Eq)]
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
    /// Display name of the first key.
    pub key_name: String,
    /// Only codec the first key works with. Absent allows the provider's codec.
    pub key_protocol: Option<String>,
    /// Preset region or plan of the first key. Absent uses the provider's plan.
    pub key_plan: Option<String>,
    /// Weighted-routing share of the first key.
    pub key_weight: u64,
    /// Additional API keys, in candidate order after the first key.
    pub keys: Vec<ProviderKey>,
    /// How many keys take requests; the rest wait in reserve. Absent uses the
    /// plugin's declared default, and zero means no limit.
    pub at_once: Option<u64>,
    /// Quirk profile id. Absent when the endpoint needs no profile.
    pub quirks: Option<String>,
    /// Extra request headers.
    pub headers: BTreeMap<String, String>,
    /// Output cap used when the client omits `max_tokens`.
    ///
    /// A route member can override this. Absent leaves the cap to a quirk
    /// profile, or to the Messages codec's fallback.
    pub max_output_tokens: Option<u64>,
    /// Whether every model on this provider is free, overriding catalog prices.
    pub free: bool,
    /// Models or routing groups tried after this provider fails.
    pub fallback: Vec<Fallback>,
    /// Conversation affinity for routes that do not override it.
    pub stays: Stays,
    /// Retry budget for a direct request to this provider.
    pub patience: Patience,
    /// Plugin settings. The plugin validates the keys.
    ///
    /// See [`Value`] for which TOML shapes load and for the empty-string rule.
    pub options: BTreeMap<String, Value>,
    /// Host-owned model listing settings.
    pub model_list: ModelList,
    /// Optional balance endpoint settings.
    pub balance: ProviderBalance,
}

/// A provider's optional balance endpoint settings.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderBalance {
    /// Endpoint URL. Empty selects a known endpoint from the provider host.
    #[serde(default)]
    pub url: String,
    /// Field or expression to read from the response.
    #[serde(default)]
    pub path: String,
    /// Account token used instead of the API key.
    #[serde(default)]
    pub token: String,
    /// HTTP method: GET or POST. Empty means GET.
    #[serde(default)]
    pub method: String,
}

impl fmt::Debug for ProviderBalance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderBalance")
            .field("url", &self.url)
            .field("path", &self.path)
            .field("token", &"<redacted>")
            .field("method", &self.method)
            .finish()
    }
}

/// The largest weighted-routing share one API key may have.
pub const MAX_KEY_WEIGHT: u64 = 1000;
/// The largest accounts-at-once limit a provider or subscription may declare.
pub const MAX_AT_ONCE: u64 = 1000;

/// The codecs one API key may be restricted to.
pub const KEY_PROTOCOLS: [&str; 5] = [
    "openai-chat",
    "openai-responses",
    "anthropic-messages",
    "generative-language",
    "bedrock-converse",
];

/// One additional provider API key from `[[providers.keys]]`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderKey {
    /// Credential spec. `env:NAME` reads that variable. Any other value is the key.
    #[serde(default)]
    pub key: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Whether this key is a route candidate.
    #[serde(default = "default_true")]
    pub on: bool,
    /// Only codec this key works with. Absent allows the provider's codec.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Preset region or plan. Absent uses the provider's plan.
    #[serde(default)]
    pub plan: Option<String>,
    /// Weighted-routing share. One is the default.
    #[serde(default = "default_key_weight")]
    pub weight: u64,
}

impl Provider {
    /// Every nonempty key once, with the first key first and duplicates keeping
    /// their first position and settings.
    #[must_use]
    pub fn key_entries(&self) -> Vec<ProviderKey> {
        let first = ProviderKey {
            key: self.api_key.clone(),
            name: self.key_name.clone(),
            on: true,
            protocol: self.key_protocol.clone(),
            plan: self.key_plan.clone(),
            weight: self.key_weight,
        };
        let mut seen = BTreeSet::new();
        std::iter::once(first)
            .chain(self.keys.iter().cloned())
            .filter(|key| !key.key.trim().is_empty() && seen.insert(key.key.clone()))
            .collect()
    }
}

/// Replaces one provider's ordered key list while preserving every unrelated
/// TOML field and each credential spec as written.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when the document is invalid, the provider
/// is absent, or `keys` is empty.
pub fn edit_provider_keys(
    text: &str,
    provider_id: &str,
    keys: &[ProviderKey],
) -> Result<String, ConfigError> {
    use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

    fn set_string(table: &mut Table, field: &str, text: &str) {
        if text.is_empty() {
            table.remove(field);
        } else {
            table.insert(field, value(text));
        }
    }

    let Some((first, additional)) = keys.split_first() else {
        return Err(ConfigError::Invalid(
            "a provider must keep one key".to_owned(),
        ));
    };
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let table = document
        .get_mut("providers")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|providers| {
            providers
                .iter_mut()
                .find(|table| table.get("id").and_then(Item::as_str) == Some(provider_id))
        })
        .ok_or_else(|| ConfigError::Invalid(format!("unknown provider `{provider_id}`")))?;
    set_string(table, "api_key", &first.key);
    set_string(table, "key_name", &first.name);
    if let Some(protocol) = &first.protocol {
        table.insert("key_protocol", value(protocol));
    } else {
        table.remove("key_protocol");
    }
    if let Some(plan) = &first.plan {
        table.insert("key_plan", value(plan));
    } else {
        table.remove("key_plan");
    }
    if first.weight == 1 {
        table.remove("key_weight");
    } else {
        table.insert(
            "key_weight",
            value(i64::try_from(first.weight).unwrap_or(i64::MAX)),
        );
    }
    if additional.is_empty() {
        table.remove("keys");
    } else {
        let mut raw = ArrayOfTables::new();
        for entry in additional {
            let mut table = Table::new();
            table.insert("key", value(&entry.key));
            if !entry.name.is_empty() {
                table.insert("name", value(&entry.name));
            }
            if !entry.on {
                table.insert("on", value(false));
            }
            if let Some(protocol) = &entry.protocol {
                table.insert("protocol", value(protocol));
            }
            if let Some(plan) = &entry.plan {
                table.insert("plan", value(plan));
            }
            if entry.weight != 1 {
                table.insert(
                    "weight",
                    value(i64::try_from(entry.weight).unwrap_or(i64::MAX)),
                );
            }
            raw.push(table);
        }
        table.insert("keys", Item::ArrayOfTables(raw));
    }
    Ok(document.to_string())
}

/// Sets one provider's or subscription's accounts-at-once override while
/// preserving every unrelated TOML field. `None` restores the plugin default.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when the value is out of range or the id is
/// absent, or [`ConfigError::Syntax`] when the document is not valid TOML.
pub fn edit_at_once(text: &str, id: &str, at_once: Option<u64>) -> Result<String, ConfigError> {
    use toml_edit::{DocumentMut, Item, value};

    validated_at_once("provider or subscription", id, at_once)?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let mut found = None;
    for kind in ["providers", "subscriptions"] {
        found = document
            .get_mut(kind)
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|entries| {
                entries
                    .iter_mut()
                    .find(|table| table.get("id").and_then(Item::as_str) == Some(id))
            });
        if found.is_some() {
            break;
        }
    }
    let table = found
        .ok_or_else(|| ConfigError::Invalid(format!("unknown provider or subscription `{id}`")))?;
    if let Some(limit) = at_once {
        table.insert("at_once", value(i64::try_from(limit).unwrap_or(i64::MAX)));
    } else {
        table.remove("at_once");
    }
    Ok(document.to_string())
}

impl fmt::Debug for ProviderKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderKey")
            .field("key", &"<redacted>")
            .field("name", &self.name)
            .field("on", &self.on)
            .field("protocol", &self.protocol)
            .field("plan", &self.plan)
            .field("weight", &self.weight)
            .finish()
    }
}

/// Host-owned model listing settings from `[providers.models]`.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelList {
    /// Refresh interval text.
    #[serde(default)]
    pub refresh: String,
    /// Case-insensitive glob or `re:` rules used by the auto-include job.
    #[serde(default)]
    pub auto_include: Vec<String>,
    /// An exact model-list URL, or an `env:NAME` reference to one.
    #[serde(default)]
    pub url: String,
}

impl ModelList {
    /// The parsed refresh interval, or `None` when refresh is off.
    #[must_use]
    pub fn refresh_interval(&self) -> Option<Duration> {
        parse_refresh(&self.refresh)
    }
}

/// Compiles one validated auto-include rule for a whole, case-insensitive id match.
///
/// A glob contains `*`; every other character is literal. A `re:` rule contains
/// a regular expression and may use `(?-i)` to disable case folding from that
/// point onward.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] for a bare id, an empty expression, or an
/// invalid glob or regular expression.
pub fn compile_auto_include(rule: &str) -> Result<regex::Regex, ConfigError> {
    let rule = rule.trim();
    if let Some(expression) = rule.strip_prefix("re:") {
        if expression.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "an auto-include `re:` rule needs a regular expression".to_owned(),
            ));
        }
        return regex::Regex::new(&format!("(?i)^(?:{expression})$")).map_err(|error| {
            ConfigError::Invalid(format!(
                "auto-include rule `{rule}` is not a valid regular expression: {error}"
            ))
        });
    }
    if !rule.contains('*') {
        return Err(ConfigError::Invalid(format!(
            "auto-include rule `{rule}` is a bare model id; use `*` or a `re:` prefix"
        )));
    }
    let glob = rule
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*");
    regex::Regex::new(&format!("(?i)^{glob}$")).map_err(|error| {
        ConfigError::Invalid(format!(
            "auto-include rule `{rule}` is not a valid glob: {error}"
        ))
    })
}

impl fmt::Debug for ModelList {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelList")
            .field("refresh", &self.refresh)
            .field("auto_include", &self.auto_include)
            .field("url", &"<redacted>")
            .finish()
    }
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
#[derive(Clone, PartialEq, Eq)]
pub struct Subscription {
    /// Subscription plugin id.
    pub id: String,
    /// Which accounts to serve.
    pub accounts: Accounts,
    /// Account weights by lower-cased account id. Missing accounts weigh one.
    pub account_weights: BTreeMap<String, u64>,
    /// Member-ordering strategy used inside this subscription.
    pub routing: String,
    /// How many accounts take requests; the rest wait in reserve. Absent uses
    /// the plugin's declared default, and zero means no limit.
    pub at_once: Option<u64>,
    /// Conversation affinity for routes that do not override it.
    pub stays: Stays,
    /// Vendor host, client id, and any other plugin setting.
    ///
    /// The empty-string rule is the same as [`Provider::options`].
    /// Boolean `keep_login` pins the first account as the one the agent
    /// returns to. Boolean `switch_only` moves only the agent sign-in and
    /// leaves gateway routing on the account it already chose.
    pub options: BTreeMap<String, Value>,
}

impl Subscription {
    /// The configured weight for `account`, or one when it has no entry.
    #[must_use]
    pub fn account_weight(&self, account: &str) -> u64 {
        self.account_weights
            .get(&account.trim().to_lowercase())
            .copied()
            .unwrap_or(1)
    }
}

/// One model route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// Model name the client sends.
    pub model: String,
    /// Registered member-ordering strategy.
    pub routing: String,
    /// Conversation affinity for this routing group.
    pub stays: Stays,
    /// Whether `stays` was written explicitly rather than defaulted.
    pub stays_explicit: bool,
    /// Providers tried in order, each with an optional upstream model name.
    pub upstreams: Vec<Upstream>,
    /// How long a request to this route waits for busy members to come
    /// back. Members are judged by the route's patience, not their
    /// provider's, which applies only to a direct request to that provider.
    pub patience: Patience,
    /// Whether a rate-limited member with quota left moves behind the others.
    pub sink: bool,
}

/// Which kind of upstream a route hop names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpstreamTarget {
    /// A configured provider id.
    Provider(String),
    /// A configured subscription id. Membership expands this to account members.
    Subscription(String),
    /// Another route named `group/<id>`.
    Group(String),
}

/// One provider fallback entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// A model on a configured provider.
    Model {
        /// Configured provider id.
        provider: String,
        /// Model name sent to that provider.
        model: String,
    },
    /// A whole routing group.
    Group(String),
}

/// One hop on a route: a provider or a subscription, plus an optional model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    /// Provider or subscription id.
    pub target: UpstreamTarget,
    /// Model name sent to this upstream.
    ///
    /// Absent means the name the client used, after `provider/model` is
    /// reduced to the model portion.
    pub model: Option<String>,
    /// Output cap for this hop when the client omits `max_tokens`.
    ///
    /// Absent uses the provider's `max_output_tokens` when the target is a
    /// provider.
    pub max_output_tokens: Option<u64>,
    /// Relative weighted-routing share. Absent means one.
    pub weight: Option<u64>,
}

impl Upstream {
    /// The provider id when this hop names a provider.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        match &self.target {
            UpstreamTarget::Provider(id) => Some(id.as_str()),
            UpstreamTarget::Subscription(_) | UpstreamTarget::Group(_) => None,
        }
    }

    /// The subscription id when this hop names a subscription.
    #[must_use]
    pub fn subscription(&self) -> Option<&str> {
        match &self.target {
            UpstreamTarget::Subscription(id) => Some(id.as_str()),
            UpstreamTarget::Provider(_) | UpstreamTarget::Group(_) => None,
        }
    }

    /// The group id when this hop names another routing group.
    #[must_use]
    pub fn group(&self) -> Option<&str> {
        match &self.target {
            UpstreamTarget::Group(id) => Some(id.as_str()),
            UpstreamTarget::Provider(_) | UpstreamTarget::Subscription(_) => None,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("listen", &self.listen)
            .field("token_file", &self.token_file)
            .field("usage_file", &self.usage_file)
            .field("failover", &self.failover)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("providers", &self.providers)
            .field("routes", &self.routes)
            .field("subscriptions", &self.subscriptions)
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl fmt::Debug for Provider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Provider")
            .field("id", &self.id)
            .field("preset", &self.preset)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("key_name", &self.key_name)
            .field("key_protocol", &self.key_protocol)
            .field("key_plan", &self.key_plan)
            .field("key_weight", &self.key_weight)
            .field("keys", &self.keys)
            .field("at_once", &self.at_once)
            .field("quirks", &self.quirks)
            .field("headers", &RedactedPairs(&self.headers))
            .field("max_output_tokens", &self.max_output_tokens)
            .field("free", &self.free)
            .field("patience", &self.patience)
            .field("options", &self.options)
            .field("models", &self.model_list)
            .field("balance", &self.balance)
            .finish()
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Subscription")
            .field("id", &self.id)
            .field("accounts", &self.accounts)
            .field("account_weights", &self.account_weights)
            .field("options", &self.options)
            .finish()
    }
}

struct RedactedPairs<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedPairs<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = formatter.debug_map();
        for key in self.0.keys() {
            map.entry(key, &"<redacted>");
        }
        map.finish()
    }
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

/// The environment variable that overrides the configured model `listen`.
pub const ADDR_ENV: &str = "HOFUND_ADDR";

/// Where the effective model listen address came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListenSource {
    /// `HOFUND_ADDR` was set.
    Environment,
    /// The config file's `listen`.
    Config,
}

/// The value of `HOFUND_ADDR` in this process, if any. A value that is not
/// UTF-8 is returned lossily, so [`effective_listen`] rejects it.
#[must_use]
pub fn addr_env() -> Option<String> {
    std::env::var_os(ADDR_ENV).map(|value| value.to_string_lossy().into_owned())
}

/// The model listen address: `env` (the `HOFUND_ADDR` value) when it is set
/// and not empty, otherwise the config's `listen`.
///
/// The daemon and the healthcheck both resolve the address this way.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when `env` is set but is not a socket
/// address.
pub fn effective_listen(
    config: &Config,
    env: Option<&str>,
) -> Result<(SocketAddr, ListenSource), ConfigError> {
    match env.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value
            .parse()
            .map(|addr| (addr, ListenSource::Environment))
            .map_err(|_| {
                ConfigError::Invalid(format!("{ADDR_ENV} must be host:port, got {value:?}"))
            }),
        None => Ok((config.listen, ListenSource::Config)),
    }
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
    let group_names = raw
        .routes
        .iter()
        .filter_map(|route| route.model.strip_prefix("group/").map(str::to_owned))
        .collect::<BTreeSet<_>>();
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
        if provider.preset.as_ref().is_some_and(String::is_empty) {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty preset",
                provider.id
            )));
        }
        let protocol = resolve_protocol(&provider, &mut warnings)?;
        if provider.preset.is_none() && protocol.is_none() {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` needs a protocol or a preset",
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
        validate_refresh(&provider.id, &provider.models.refresh)?;
        let key_weight = validate_key_weight(&provider.id, "key_weight", provider.key_weight)?;
        let auto_include = clean_auto_include(&provider.id, &provider.models.auto_include)?;
        let patience = patience_of("provider", &provider.id, provider.patience.as_ref())?;
        let options = options_of(&provider, &mut warnings)?;
        let key_protocol = validated_key_protocol(&provider.id, provider.key_protocol)?;
        let keys = validated_keys(&provider.id, provider.keys)?;
        if !matches!(
            provider.balance.method.trim().to_uppercase().as_str(),
            "" | "GET" | "POST"
        ) {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` balance method must be GET or POST",
                provider.id
            )));
        }
        let fallback = provider
            .fallback
            .iter()
            .map(|entry| parse_fallback(&provider.id, entry))
            .collect::<Result<Vec<_>, _>>()?;
        let at_once = validated_at_once("provider", &provider.id, provider.at_once)?;
        let mut model_list = provider.models;
        model_list.auto_include = auto_include;
        providers.push(Provider {
            id: provider.id,
            preset: provider.preset,
            protocol,
            base_url: provider.base_url,
            api_key: provider.api_key,
            key_name: provider.key_name,
            key_protocol,
            key_plan: provider.key_plan,
            key_weight,
            keys,
            at_once,
            quirks: provider.quirks,
            headers: provider.headers,
            max_output_tokens: provider.max_output_tokens,
            free: provider.free,
            fallback,
            stays: provider.stays,
            patience,
            options,
            model_list,
            balance: provider.balance,
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
        let account_weights = normalized_account_weights(&subscription)?;
        for key in subscription.options.keys() {
            if key.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "subscription `{}` has an empty option key",
                    subscription.id
                )));
            }
        }
        validated_routing_name("subscription", &subscription.id, &subscription.routing)?;
        let at_once = validated_at_once("subscription", &subscription.id, subscription.at_once)?;
        subscriptions.push(Subscription {
            id: subscription.id,
            accounts,
            account_weights,
            routing: subscription.routing,
            at_once,
            stays: subscription.stays,
            options: prune_table(subscription.options),
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
        let routing = validated_routing(&route)?;
        let stays_explicit = route.stays.is_some();
        let stays = route.stays.unwrap_or_default();
        let patience = patience_of("route", &route.model, route.patience.as_ref())?;
        let mut upstreams = Vec::new();
        for upstream in route.upstreams {
            let upstream = upstream_of(
                &route.model,
                upstream,
                &seen,
                &subscription_ids,
                &group_names,
            )?;
            upstreams.push(upstream);
        }
        // Q1: one id as both a provider and a subscription in one route is
        // ambiguous to read, so it is a config error.
        let named_providers = upstreams
            .iter()
            .filter_map(Upstream::provider)
            .collect::<BTreeSet<_>>();
        if let Some(shared) = upstreams
            .iter()
            .filter_map(Upstream::subscription)
            .find(|id| named_providers.contains(id))
        {
            return Err(ConfigError::Invalid(format!(
                "route `{}` names `{shared}` as both a provider and a subscription",
                route.model
            )));
        }
        routes.push(Route {
            model: route.model,
            routing,
            stays,
            stays_explicit,
            upstreams,
            patience,
            sink: route.sink,
        });
    }
    validate_group_links(&routes)?;
    validate_fallbacks(&providers, &seen, &group_names)?;
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
) -> Result<BTreeMap<String, Value>, ConfigError> {
    let mut options = BTreeMap::new();
    for (key, value) in &provider.options {
        if key.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has an empty option key",
                provider.id
            )));
        }
        if let Some(value) = prune_value(value.clone()) {
            options.insert(key.clone(), value);
        }
    }
    for (key, value) in legacy_options(provider) {
        let Some(value) = value else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        if let Some(existing) = options.get(key)
            && !matches!(existing, Value::String(stored) if stored == value)
        {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` sets `{key}` and options.{key} to different values",
                provider.id
            )));
        }
        options.insert((*key).to_owned(), Value::String((*value).to_owned()));
        warnings.push(format!(
            "provider `{id}`: `{key}` moved to [providers.options]. The old key still loads and is removed next release.",
            id = provider.id
        ));
    }
    Ok(options)
}

/// `kind` and the five short protocol names still load for this release.
///
/// A non-empty value is rewritten to a codec id and the load records a warning
/// that names the replacement. The next release rejects both. `kind` selects
/// the family: `openai_compat` may be Chat or Responses. A `kind` that
/// disagrees with `protocol` is an error. `protocol` is trimmed first. A blank
/// value is absent, the same as a blank `kind`. A string that is neither a
/// short name nor a codec id is kept, and the daemon rejects it.
fn resolve_protocol(
    provider: &RawProvider,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, ConfigError> {
    let kind = match provider.kind.as_deref() {
        Some(kind) if kind.trim().is_empty() => None,
        Some(kind) => Some(kind),
        None => None,
    };
    let raw_protocol = provider
        .protocol
        .as_deref()
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty());
    let family = match kind {
        None => None,
        Some("openai_compat" | "anthropic" | "generative" | "converse") => kind,
        Some(other) => {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` has unknown kind `{other}`; expected openai_compat, anthropic, generative, or converse",
                provider.id
            )));
        }
    };
    let (canonical, short_name) = match raw_protocol {
        None => (None, None),
        Some("chat") => (Some("openai-chat"), Some("chat")),
        Some("responses") => (Some("openai-responses"), Some("responses")),
        Some("messages") => (Some("anthropic-messages"), Some("messages")),
        Some("generative") => (Some("generative-language"), Some("generative")),
        Some("converse") => (Some("bedrock-converse"), Some("converse")),
        Some(protocol) => (Some(protocol), None),
    };
    let chosen = match (family, canonical) {
        (None, protocol) => protocol,
        (Some("openai_compat"), None | Some("openai-chat")) => Some("openai-chat"),
        (Some("openai_compat"), Some("openai-responses")) => Some("openai-responses"),
        (Some("anthropic"), None | Some("anthropic-messages")) => Some("anthropic-messages"),
        (Some("generative"), None | Some("generative-language")) => Some("generative-language"),
        (Some("converse"), None | Some("bedrock-converse")) => Some("bedrock-converse"),
        (Some(kind_name), _) => {
            return Err(ConfigError::Invalid(format!(
                "provider `{}` sets kind `{kind_name}` and protocol `{}` to different protocols",
                provider.id,
                raw_protocol.unwrap_or_default()
            )));
        }
    };
    if family.is_some() {
        let replacement = chosen.expect("kind selects a protocol");
        warnings.push(format!(
            "provider `{id}`: `kind` was removed; set `protocol = \"{replacement}\"`. The old key still loads and is removed next release.",
            id = provider.id
        ));
    }
    if let Some(short_name) = short_name {
        let replacement = chosen.expect("short name selects a protocol");
        warnings.push(format!(
            "provider `{id}`: protocol `{short_name}` was renamed to `{replacement}`. The old name still loads and is removed next release.",
            id = provider.id
        ));
    }
    Ok(chosen.map(str::to_owned))
}

fn prune_table(table: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    table
        .into_iter()
        .filter_map(|(key, value)| prune_value(value).map(|value| (key, value)))
        .collect()
}

fn prune_value(value: Value) -> Option<Value> {
    match value {
        Value::String(text) if text.trim().is_empty() => None,
        Value::Table(table) => Some(Value::Table(prune_table(table))),
        other => Some(other),
    }
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

fn normalized_account_weights(
    subscription: &RawSubscription,
) -> Result<BTreeMap<String, u64>, ConfigError> {
    let mut weights = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for (account, weight) in &subscription.account_weights {
        let account = account.trim().to_lowercase();
        if account.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "subscription `{}` has an empty account weight name",
                subscription.id
            )));
        }
        if *weight == 0 {
            return Err(ConfigError::Invalid(format!(
                "subscription `{subscription}` account `{account}` has weight 0; weight must be at least 1",
                subscription = subscription.id
            )));
        }
        if !seen.insert(account.clone()) {
            return Err(ConfigError::Invalid(format!(
                "subscription `{subscription}` repeats account weight `{account}` with different casing",
                subscription = subscription.id
            )));
        }
        if *weight > 1 {
            weights.insert(account, *weight);
        }
    }
    Ok(weights)
}

const ROUTING_NAMES: [&str; 9] = [
    "smart", "order", "rotate", "usage", "pace", "weight", "free", "fast", "cheap",
];

fn validated_routing_name(kind: &str, id: &str, routing: &str) -> Result<(), ConfigError> {
    if !ROUTING_NAMES.contains(&routing) {
        return Err(ConfigError::Invalid(format!(
            "{kind} `{id}` has unknown routing `{routing}`; expected {}",
            ROUTING_NAMES.join(", ")
        )));
    }
    Ok(())
}

fn validated_routing(route: &RawRoute) -> Result<String, ConfigError> {
    validated_routing_name("route", &route.model, &route.routing)?;
    let group = route.upstreams.len() > 1
        || route.upstreams.iter().any(|upstream| {
            matches!(upstream, RawUpstream::Mapped(mapped) if mapped.subscription.is_some() || mapped.group.is_some())
        });
    if route.routing == "cheap" && !group {
        return Err(ConfigError::Invalid(format!(
            "route `{}` uses cheap routing outside a group",
            route.model
        )));
    }
    Ok(route.routing.clone())
}

fn upstream_of(
    route: &str,
    upstream: RawUpstream,
    providers: &BTreeSet<String>,
    subscriptions: &BTreeSet<String>,
    groups: &BTreeSet<String>,
) -> Result<Upstream, ConfigError> {
    let (target, model, max_output_tokens, weight) = match upstream {
        RawUpstream::Id(provider) => (UpstreamTarget::Provider(provider), None, None, None),
        RawUpstream::Mapped(mapped) => {
            if mapped.weight == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` has weight 0; weight must be at least 1"
                )));
            }
            let weight = mapped.weight.filter(|weight| *weight > 1);
            match (mapped.provider, mapped.subscription, mapped.group) {
                (Some(_), Some(_), _) => {
                    return Err(ConfigError::Invalid(format!(
                        "route `{route}` names both provider and subscription"
                    )));
                }
                (None, None, None) => {
                    return Err(ConfigError::Invalid(format!(
                        "route `{route}` upstream needs provider or subscription"
                    )));
                }
                (Some(provider), None, None) => (
                    UpstreamTarget::Provider(provider),
                    mapped.model,
                    mapped.max_output_tokens,
                    weight,
                ),
                (None, Some(subscription), None) => (
                    UpstreamTarget::Subscription(subscription),
                    mapped.model,
                    mapped.max_output_tokens,
                    weight,
                ),
                (None, None, Some(group)) => {
                    if mapped.model.is_some() || mapped.max_output_tokens.is_some() {
                        return Err(ConfigError::Invalid(format!(
                            "route `{route}` group upstream `{group}` cannot override model or max_output_tokens"
                        )));
                    }
                    (UpstreamTarget::Group(group), None, None, weight)
                }
                _ => {
                    return Err(ConfigError::Invalid(format!(
                        "route `{route}` upstream must name only one of provider, subscription, or group"
                    )));
                }
            }
        }
    };
    match &target {
        UpstreamTarget::Provider(provider) => {
            if provider.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` names an empty provider"
                )));
            }
            if !providers.contains(provider) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` names unknown provider `{provider}`"
                )));
            }
            if model.as_ref().is_some_and(String::is_empty) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` has an empty model for provider `{provider}`"
                )));
            }
            if max_output_tokens == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` has max_output_tokens 0 for provider `{provider}`"
                )));
            }
        }
        UpstreamTarget::Subscription(subscription) => {
            if subscription.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` names an empty subscription"
                )));
            }
            if !subscriptions.contains(subscription) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` names unknown subscription `{subscription}`"
                )));
            }
            if model.as_ref().is_some_and(String::is_empty) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` has an empty model for subscription `{subscription}`"
                )));
            }
            if max_output_tokens == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` has max_output_tokens 0 for subscription `{subscription}`"
                )));
            }
        }
        UpstreamTarget::Group(group) => {
            if group.is_empty() || group.contains('/') {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` has an invalid group id `{group}`"
                )));
            }
            if !groups.contains(group) {
                return Err(ConfigError::Invalid(format!(
                    "route `{route}` names unknown group `{group}`"
                )));
            }
        }
    }
    Ok(Upstream {
        target,
        model,
        max_output_tokens,
        weight,
    })
}

fn parse_fallback(provider: &str, entry: &str) -> Result<Fallback, ConfigError> {
    if let Some(group) = entry.strip_prefix("group/") {
        if group.is_empty() || group.contains('/') {
            return Err(ConfigError::Invalid(format!(
                "provider `{provider}` has invalid fallback group `{entry}`"
            )));
        }
        return Ok(Fallback::Group(group.to_owned()));
    }
    let Some((target, model)) = entry.split_once('/') else {
        return Err(ConfigError::Invalid(format!(
            "provider `{provider}` fallback `{entry}` must be `provider/model` or `group/id`"
        )));
    };
    if target.is_empty() || model.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "provider `{provider}` has empty fallback target in `{entry}`"
        )));
    }
    Ok(Fallback::Model {
        provider: target.to_owned(),
        model: model.to_owned(),
    })
}

fn validate_fallbacks(
    providers: &[Provider],
    provider_ids: &BTreeSet<String>,
    groups: &BTreeSet<String>,
) -> Result<(), ConfigError> {
    for provider in providers {
        for fallback in &provider.fallback {
            match fallback {
                Fallback::Model {
                    provider: target, ..
                } if !provider_ids.contains(target) => {
                    return Err(ConfigError::Invalid(format!(
                        "provider `{}` fallback names unknown provider `{target}`",
                        provider.id
                    )));
                }
                Fallback::Group(group) if !groups.contains(group) => {
                    return Err(ConfigError::Invalid(format!(
                        "provider `{}` fallback names unknown group `{group}`",
                        provider.id
                    )));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn validate_group_links(routes: &[Route]) -> Result<(), ConfigError> {
    fn walk(
        model: &str,
        depth: usize,
        routes: &BTreeMap<&str, &Route>,
        visiting: &mut BTreeSet<String>,
        checked: &mut BTreeSet<String>,
    ) -> Result<(), ConfigError> {
        if depth > 8 {
            return Err(ConfigError::Invalid(format!(
                "route `{model}` nests routing groups more than 8 deep"
            )));
        }
        if checked.contains(model) {
            return Ok(());
        }
        if !visiting.insert(model.to_owned()) {
            return Err(ConfigError::Invalid(format!(
                "route `{model}` forms a routing group cycle"
            )));
        }
        let route = routes.get(model).expect("validated group target");
        for group in route.upstreams.iter().filter_map(Upstream::group) {
            walk(
                &format!("group/{group}"),
                depth + 1,
                routes,
                visiting,
                checked,
            )?;
        }
        visiting.remove(model);
        checked.insert(model.to_owned());
        Ok(())
    }

    let routes = routes
        .iter()
        .map(|route| (route.model.as_str(), route))
        .collect::<BTreeMap<_, _>>();
    let mut checked = BTreeSet::new();
    for model in routes.keys() {
        walk(model, 0, &routes, &mut BTreeSet::new(), &mut checked)?;
    }
    Ok(())
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

fn default_key_weight() -> u64 {
    1
}

/// Zero, like an absent weight, counts as the default share of one.
fn validate_key_weight(provider: &str, field: &str, weight: u64) -> Result<u64, ConfigError> {
    if weight <= MAX_KEY_WEIGHT {
        return Ok(weight.max(1));
    }
    Err(ConfigError::Invalid(format!(
        "provider `{provider}` {field} must be 0 to {MAX_KEY_WEIGHT} (0 counts as 1)"
    )))
}

fn validated_at_once(
    kind: &str,
    id: &str,
    at_once: Option<u64>,
) -> Result<Option<u64>, ConfigError> {
    if at_once.is_some_and(|limit| limit > MAX_AT_ONCE) {
        return Err(ConfigError::Invalid(format!(
            "{kind} `{id}` at_once must be 0 to {MAX_AT_ONCE} (0 means no limit)"
        )));
    }
    Ok(at_once)
}

fn validated_key_protocol(
    provider: &str,
    protocol: Option<String>,
) -> Result<Option<String>, ConfigError> {
    let protocol = protocol.and_then(|protocol| {
        let protocol = protocol.trim();
        (!protocol.is_empty()).then(|| protocol.to_owned())
    });
    if let Some(protocol) = &protocol
        && !KEY_PROTOCOLS.contains(&protocol.as_str())
    {
        return Err(ConfigError::Invalid(format!(
            "provider `{provider}` has unknown key protocol `{protocol}`"
        )));
    }
    Ok(protocol)
}

fn validated_keys(provider: &str, keys: Vec<ProviderKey>) -> Result<Vec<ProviderKey>, ConfigError> {
    let mut validated = Vec::with_capacity(keys.len());
    for mut key in keys {
        key.weight = validate_key_weight(provider, "key weight", key.weight)?;
        key.protocol = validated_key_protocol(provider, key.protocol)?;
        validated.push(key);
    }
    Ok(validated)
}

fn default_connect_timeout_ms() -> u64 {
    10_000
}

fn default_read_timeout_ms() -> u64 {
    60_000
}

fn validate_refresh(provider: &str, value: &str) -> Result<(), ConfigError> {
    if refresh_off(value) || parse_refresh(value).is_some() {
        return Ok(());
    }
    Err(ConfigError::Invalid(format!(
        "provider `{provider}` models.refresh must be off, a day count, or a duration of at least 10m"
    )))
}

fn refresh_off(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || matches!(
            value.to_ascii_lowercase().as_str(),
            "off" | "never" | "no" | "none" | "0" | "-"
        )
}

fn parse_refresh(value: &str) -> Option<Duration> {
    if refresh_off(value) {
        return None;
    }
    let value = value.trim().to_ascii_lowercase();
    let duration = if let Some(days) = value.strip_suffix('d') {
        days.parse::<f64>()
            .ok()
            .filter(|days| days.is_finite() && *days > 0.0)
            .and_then(|days| Duration::try_from_secs_f64(days * 86_400.0).ok())?
    } else {
        parse_duration(&value)?
    };
    (duration >= Duration::from_secs(600)).then_some(duration)
}

fn clean_auto_include(provider: &str, rules: &[String]) -> Result<Vec<String>, ConfigError> {
    let mut seen = BTreeSet::new();
    let mut cleaned = Vec::new();
    for rule in rules {
        let rule = rule.trim();
        if rule.is_empty() || !seen.insert(rule.to_owned()) {
            continue;
        }
        compile_auto_include(rule).map_err(|error| {
            ConfigError::Invalid(format!(
                "provider `{provider}` models.auto_include: {error}"
            ))
        })?;
        cleaned.push(rule.to_owned());
    }
    Ok(cleaned)
}

fn parse_duration(value: &str) -> Option<Duration> {
    let mut rest = value;
    let mut seconds = 0.0;
    let mut parts = 0;
    while !rest.is_empty() {
        let digits = rest
            .char_indices()
            .take_while(|(_, character)| character.is_ascii_digit() || *character == '.')
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0);
        let number = rest[..digits].parse::<f64>().ok()?;
        if !number.is_finite() || number < 0.0 {
            return None;
        }
        rest = &rest[digits..];
        let (unit, scale) = [
            ("ns", 1e-9),
            ("us", 1e-6),
            ("\u{b5}s", 1e-6),
            ("ms", 1e-3),
            ("s", 1.0),
            ("m", 60.0),
            ("h", 3600.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        seconds += number * scale;
        rest = &rest[unit.len()..];
        parts += 1;
    }
    (parts > 0 && seconds.is_finite())
        .then(|| Duration::try_from_secs_f64(seconds).ok())
        .flatten()
}

fn patience_of(kind: &str, id: &str, value: Option<&Value>) -> Result<Patience, ConfigError> {
    let invalid = || {
        ConfigError::Invalid(format!(
            "{kind} `{id}` patience must be `off`, `default`, or at most 240 seconds"
        ))
    };
    let duration = match value {
        None => return Ok(Patience::Default),
        Some(Value::String(value)) => {
            let value = value.trim().to_ascii_lowercase();
            match value.as_str() {
                "off" | "0" => return Ok(Patience::Off),
                "default" => return Ok(Patience::Default),
                _ => parse_duration(&value).ok_or_else(invalid)?,
            }
        }
        Some(Value::Integer(seconds)) if (0..=240).contains(seconds) => {
            if *seconds == 0 {
                return Ok(Patience::Off);
            }
            Duration::from_secs(*seconds as u64)
        }
        _ => return Err(invalid()),
    };
    if duration > Duration::from_secs(240) {
        return Err(invalid());
    }
    if duration.is_zero() {
        Ok(Patience::Off)
    } else {
        Ok(Patience::Wait(duration))
    }
}

#[derive(Deserialize)]
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
    [401, 402, 403, 404, 408, 429]
        .into_iter()
        .chain(500..=599)
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    id: String,
    #[serde(default)]
    preset: Option<String>,
    /// Accepted for one release, then rewritten to a codec id.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    key_name: String,
    #[serde(default)]
    key_protocol: Option<String>,
    #[serde(default)]
    key_plan: Option<String>,
    #[serde(default = "default_key_weight")]
    key_weight: u64,
    #[serde(default)]
    keys: Vec<ProviderKey>,
    #[serde(default)]
    at_once: Option<u64>,
    #[serde(default)]
    quirks: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    free: bool,
    #[serde(default)]
    fallback: Vec<String>,
    #[serde(default)]
    stays: Stays,
    patience: Option<Value>,
    #[serde(default)]
    options: BTreeMap<String, Value>,
    #[serde(default)]
    models: ModelList,
    #[serde(default)]
    balance: ProviderBalance,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubscription {
    id: String,
    accounts: String,
    #[serde(default)]
    account_weights: BTreeMap<String, u64>,
    #[serde(default = "default_smart")]
    routing: String,
    #[serde(default)]
    at_once: Option<u64>,
    #[serde(default)]
    stays: Stays,
    #[serde(default)]
    options: BTreeMap<String, Value>,
}

impl fmt::Debug for RawFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawFile")
            .field("listen", &self.listen)
            .field("token_file", &self.token_file)
            .field("usage_file", &self.usage_file)
            .field("failover", &self.failover)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("read_timeout_ms", &self.read_timeout_ms)
            .field("providers", &self.providers)
            .field("routes", &self.routes)
            .field("subscriptions", &self.subscriptions)
            .finish()
    }
}

impl fmt::Debug for RawProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawProvider")
            .field("id", &self.id)
            .field("preset", &self.preset)
            .field("kind", &self.kind)
            .field("protocol", &self.protocol)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("key_name", &self.key_name)
            .field("key_protocol", &self.key_protocol)
            .field("key_plan", &self.key_plan)
            .field("key_weight", &self.key_weight)
            .field("keys", &self.keys)
            .field("quirks", &self.quirks)
            .field("headers", &RedactedPairs(&self.headers))
            .field("max_output_tokens", &self.max_output_tokens)
            .field("free", &self.free)
            .field("options", &self.options)
            .field("models", &self.models)
            .field("balance", &self.balance)
            .field("project", &redacted_opt(&self.project))
            .field("location", &redacted_opt(&self.location))
            .field("credentials", &redacted_opt(&self.credentials))
            .field("impersonate", &redacted_opt(&self.impersonate))
            .field("access_key_id", &redacted_opt(&self.access_key_id))
            .field("secret_access_key", &redacted_opt(&self.secret_access_key))
            .field("session_token", &redacted_opt(&self.session_token))
            .field("region", &redacted_opt(&self.region))
            .finish()
    }
}

impl fmt::Debug for RawSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawSubscription")
            .field("id", &self.id)
            .field("accounts", &self.accounts)
            .field("account_weights", &self.account_weights)
            .field("options", &self.options)
            .finish()
    }
}

fn redacted_opt(value: &Option<String>) -> Option<&'static str> {
    value.as_ref().map(|_| "<redacted>")
}

fn default_smart() -> String {
    "smart".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    model: String,
    #[serde(default = "default_smart")]
    routing: String,
    #[serde(default)]
    stays: Option<Stays>,
    upstreams: Vec<RawUpstream>,
    patience: Option<Value>,
    #[serde(default)]
    sink: bool,
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
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    subscription: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    weight: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{Accounts, ConfigError, Failover, ListenSource, effective_listen, load};

    #[test]
    fn effective_listen_prefers_hofund_addr() {
        let config = load("listen = \"127.0.0.1:3425\"\n").expect("config");
        let configured = "127.0.0.1:3425".parse().expect("addr");

        assert_eq!(
            effective_listen(&config, Some("0.0.0.0:4000")).expect("env"),
            (
                "0.0.0.0:4000".parse().expect("addr"),
                ListenSource::Environment
            )
        );
        assert_eq!(
            effective_listen(&config, Some(" [::1]:4001 ")).expect("trimmed env"),
            (
                "[::1]:4001".parse().expect("addr"),
                ListenSource::Environment
            )
        );
        assert_eq!(
            effective_listen(&config, None).expect("config"),
            (configured, ListenSource::Config)
        );
        assert_eq!(
            effective_listen(&config, Some("  ")).expect("empty env is unset"),
            (configured, ListenSource::Config)
        );
        let error = effective_listen(&config, Some("localhost")).expect_err("not an address");
        assert!(
            matches!(&error, ConfigError::Invalid(message) if message.contains("HOFUND_ADDR")),
            "{error:?}"
        );
    }

    #[test]
    fn model_listing_settings_parse_refresh_and_clean_rules() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            preset = "openai"
            [providers.models]
            refresh = "1h30m"
            auto_include = [" gpt-* ", "", "re:claude", "gpt-*"]
            url = "env:MODEL_LIST_URL"
            "#,
        )
        .expect("model settings");
        let models = &config.providers[0].model_list;
        assert_eq!(models.refresh, "1h30m");
        assert_eq!(models.refresh_interval(), Some(Duration::from_secs(5400)));
        assert_eq!(models.auto_include, ["gpt-*", "re:claude"]);
        assert_eq!(models.url, "env:MODEL_LIST_URL");
        assert!(!format!("{config:?}").contains("MODEL_LIST_URL"));

        for (valid, expected) in [
            ("0.5d", Some(Duration::from_secs(43_200))),
            ("1h30m", Some(Duration::from_secs(5400))),
            ("10m", Some(Duration::from_secs(600))),
            ("", None),
            ("off", None),
            ("NEVER", None),
            ("no", None),
            ("none", None),
            ("0", None),
            ("-", None),
        ] {
            let source = format!(
                "[[providers]]\nid = \"primary\"\npreset = \"openai\"\n[providers.models]\nrefresh = \"{valid}\"\n"
            );
            let config = load(&source).unwrap_or_else(|error| panic!("refresh {valid}: {error}"));
            assert_eq!(config.providers[0].model_list.refresh_interval(), expected);
        }
        for invalid in ["9m59s", "0.001d", "tomorrow", "10", "1h30", "-1h"] {
            let source = format!(
                "[[providers]]\nid = \"primary\"\npreset = \"openai\"\n[providers.models]\nrefresh = \"{invalid}\"\n"
            );
            let error = load(&source).expect_err("invalid refresh");
            assert!(error.to_string().contains("models.refresh"), "{error}");
        }
        let unknown = load(
            "[[providers]]\nid = \"primary\"\npreset = \"openai\"\n[providers.models]\nunknown = true\n",
        )
        .expect_err("unknown model setting");
        assert!(unknown.to_string().contains("unknown"), "{unknown}");
    }

    #[test]
    fn auto_include_patterns_match_whole_ids_and_invalid_rules_are_rejected() {
        let glob = super::compile_auto_include("gpt-*").expect("glob");
        assert!(glob.is_match("GPT-5"));
        assert!(!glob.is_match("xgpt-5"));
        let expression = super::compile_auto_include("re:claude-.*").expect("expression");
        assert!(expression.is_match("CLAUDE-x"));
        let sensitive = super::compile_auto_include("re:(?-i)claude-.*").expect("expression");
        assert!(sensitive.is_match("claude-x"));
        assert!(!sensitive.is_match("Claude-x"));

        for invalid in ["bare-id", "re:", "re:   ", "re:("] {
            let source = format!(
                "[[providers]]\nid = \"primary\"\npreset = \"openai\"\n[providers.models]\nauto_include = [\"{invalid}\"]\n"
            );
            let error = load(&source).expect_err("invalid auto-include rule");
            assert!(error.to_string().contains("models.auto_include"), "{error}");
        }
    }

    #[test]
    fn default_failover_lists_auth_and_every_server_status() {
        let statuses = Failover::default().statuses;
        for status in [401, 402, 403, 404, 408, 429, 500, 599] {
            assert!(statuses.contains(&status), "{status}");
        }
        assert!(!statuses.contains(&400));
    }

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
        assert_eq!(
            config.failover.statuses,
            [401, 402, 403, 404, 408, 429]
                .into_iter()
                .chain(500..=599)
                .collect()
        );
        assert_eq!(
            config.providers[0].protocol.as_deref(),
            Some("openai-responses")
        );
        assert!(config.providers[0].preset.is_none());
        assert!(config.warnings.is_empty());
        assert!(config.subscriptions.is_empty());
        assert_eq!(config.routes[0].upstreams[0].provider(), Some("primary"));
        assert_eq!(config.routes[0].upstreams[0].model, None);
        assert_eq!(config.providers[0].max_output_tokens, None);
        assert_eq!(config.routes[0].upstreams[0].max_output_tokens, None);
        assert_eq!(config.routes[0].routing, "smart");
        assert!(!config.providers[0].free);
        assert_eq!(config.connect_timeout, Duration::from_secs(10));
        assert_eq!(config.read_timeout, Duration::from_secs(60));
    }

    #[test]
    fn c1a_stays_modes_default_and_unknown_values_are_rejected() {
        let config = load(
            r#"
            [[providers]]
            id = "one"
            preset = "openai"
            stays = "session"
            [[subscriptions]]
            id = "codex"
            accounts = "*"
            stays = "strict"
            [[routes]]
            model = "demo"
            stays = "turn"
            upstreams = [{ subscription = "codex" }]
            "#,
        )
        .expect("stays modes");
        assert_eq!(config.providers[0].stays, super::Stays::Session);
        assert_eq!(config.subscriptions[0].stays, super::Stays::Strict);
        assert_eq!(config.routes[0].stays, super::Stays::Turn);
        assert_eq!(config.routes[0].stays.as_str(), "turn");

        let defaults = load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"one\"]\n",
        )
        .expect("defaults");
        assert_eq!(defaults.providers[0].stays, super::Stays::Auto);
        assert_eq!(defaults.routes[0].stays, super::Stays::Auto);
        for (kind, source) in [
            (
                "provider",
                "[[providers]]\nid = \"one\"\npreset = \"openai\"\nstays = \"sometimes\"\n",
            ),
            (
                "subscription",
                "[[subscriptions]]\nid = \"codex\"\naccounts = \"*\"\nstays = \"sometimes\"\n",
            ),
            (
                "route",
                "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nstays = \"sometimes\"\nupstreams = [\"one\"]\n",
            ),
        ] {
            let error = load(source).expect_err(kind);
            assert!(error.to_string().contains("sometimes"), "{kind}: {error}");
        }
        for mode in ["auto", "session", "turn", "off", "strict"] {
            load(&format!(
                "[[providers]]\nid = \"one\"\npreset = \"openai\"\nstays = \"{mode}\"\n"
            ))
            .unwrap_or_else(|error| panic!("mode {mode}: {error}"));
        }
    }

    #[test]
    fn b6f_provider_patience_is_validated_at_load() {
        let configured = |value: &str| {
            load(&format!(
                "[[providers]]\nid = \"primary\"\npreset = \"openai\"\npatience = {value}\n"
            ))
        };
        assert_eq!(
            configured("90").expect("seconds").providers[0].patience,
            super::Patience::Wait(Duration::from_secs(90))
        );
        assert_eq!(
            configured("\"2m\"").expect("duration").providers[0].patience,
            super::Patience::Wait(Duration::from_secs(120))
        );
        assert_eq!(
            configured("\"off\"").expect("off").providers[0].patience,
            super::Patience::Off
        );
        assert_eq!(
            configured("0").expect("zero is off").providers[0].patience,
            super::Patience::Off
        );
        assert_eq!(
            configured("240").expect("maximum").providers[0]
                .patience
                .duration(),
            Some(Duration::from_secs(240))
        );
        assert_eq!(
            configured("\"default\"").expect("default").providers[0]
                .patience
                .duration(),
            Some(Duration::from_secs(60))
        );
        for invalid in ["241", "-1", "\"5m\"", "\"soon\"", "true"] {
            configured(invalid).expect_err("invalid patience");
        }
    }

    #[test]
    fn b6h_patience_off_and_240_second_cap() {
        let source = |value: &str| {
            format!("[[providers]]\nid = \"primary\"\npreset = \"openai\"\npatience = {value}\n")
        };
        assert_eq!(
            load(&source("\"off\"")).expect("off").providers[0].patience,
            super::Patience::Off
        );
        assert_eq!(
            load(&source("240")).expect("cap").providers[0]
                .patience
                .duration(),
            Some(Duration::from_secs(240))
        );
        load(&source("300")).expect_err("patience over the cap is refused at load");
    }

    #[test]
    fn b11_route_patience_is_validated_like_a_providers() {
        let source = |value: &str| {
            format!(
                "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [{{ provider = \"one\" }}]\n{value}"
            )
        };
        assert_eq!(
            load(&source("")).expect("default").routes[0].patience,
            super::Patience::Default
        );
        assert_eq!(
            load(&source("patience = \"off\"\n")).expect("off").routes[0].patience,
            super::Patience::Off
        );
        assert_eq!(
            load(&source("patience = 240\n")).expect("cap").routes[0]
                .patience
                .duration(),
            Some(Duration::from_secs(240))
        );
        let error = load(&source("patience = 300\n")).expect_err("over the cap");
        assert!(
            error.to_string().contains("route `demo` patience"),
            "{error}"
        );
    }

    #[test]
    fn a2_routing_names_validate_and_cheap_requires_a_group() {
        for name in [
            "smart", "order", "rotate", "usage", "pace", "weight", "free", "fast", "cheap",
        ] {
            let config = load(&format!(
                "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[providers]]\nid = \"two\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nrouting = \"{name}\"\nupstreams = [\"one\", \"two\"]\n"
            ))
            .unwrap_or_else(|error| panic!("routing {name}: {error}"));
            assert_eq!(config.routes[0].routing, name);
        }
        let error = load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nrouting = \"cheap\"\nupstreams = [\"one\"]\n",
        )
        .expect_err("cheap outside a group");
        assert!(error.to_string().contains("outside a group"), "{error}");
        let error = load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nrouting = \"unknown\"\nupstreams = [\"one\"]\n",
        )
        .expect_err("unknown routing");
        assert!(error.to_string().contains("unknown routing"), "{error}");
        load(
            "[[subscriptions]]\nid = \"codex\"\naccounts = \"*\"\n[[routes]]\nmodel = \"demo\"\nrouting = \"cheap\"\nupstreams = [{ subscription = \"codex\" }]\n",
        )
        .expect("a subscription is an account group");
    }

    #[test]
    fn a3_member_and_account_weights_default_and_normalize() {
        let config = load(
            r#"
            [[providers]]
            id = "one"
            preset = "openai"
            free = true
            [[providers]]
            id = "two"
            preset = "openai"
            [[subscriptions]]
            id = "codex"
            accounts = "*"
            [subscriptions.account_weights]
            " Me@Example.com " = 3
            "default" = 1
            "SECOND" = 2
            [[routes]]
            model = "demo"
            upstreams = [
              { provider = "one", weight = 3 },
              { provider = "two", weight = 1 },
              { subscription = "codex" },
            ]
            "#,
        )
        .expect("weights");
        assert!(config.providers[0].free);
        assert_eq!(config.routes[0].upstreams[0].weight, Some(3));
        assert_eq!(config.routes[0].upstreams[1].weight, None);
        assert_eq!(config.routes[0].upstreams[2].weight, None);
        let subscription = &config.subscriptions[0];
        assert_eq!(subscription.account_weights.len(), 2);
        assert_eq!(subscription.account_weight("ME@example.COM"), 3);
        assert_eq!(subscription.account_weight("second"), 2);
        assert_eq!(subscription.account_weight("default"), 1);
        assert_eq!(subscription.account_weight("missing"), 1);

        for (table, message) in [
            ("\"zero\" = 0", "weight 0"),
            ("\" \" = 2", "empty account weight"),
            ("\"Name\" = 2\n\"name\" = 3", "repeats account weight"),
            ("\"Name\" = 1\n\"name\" = 3", "repeats account weight"),
            ("\"Name\" = 3\n\"name\" = 1", "repeats account weight"),
        ] {
            let error = load(&format!(
                "[[subscriptions]]\nid = \"codex\"\naccounts = \"*\"\n[subscriptions.account_weights]\n{table}\n"
            ))
            .expect_err(table);
            assert!(error.to_string().contains(message), "{table}: {error}");
        }
        let error = load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nupstreams = [{ provider = \"one\", weight = 0 }]\n",
        )
        .expect_err("zero member weight");
        assert!(error.to_string().contains("weight 0"), "{error}");
    }

    #[test]
    fn d1_provider_fallback_accepts_a_whole_routing_group() {
        let config = load(
            r#"
            [[providers]]
            id = "plan"
            preset = "openai"
            fallback = ["group/backup", "other/x"]
            [[providers]]
            id = "other"
            preset = "openai"
            [[routes]]
            model = "group/backup"
            upstreams = ["other"]
            [[routes]]
            model = "demo"
            upstreams = ["plan"]
            "#,
        )
        .expect("fallback group");
        assert_eq!(
            config.providers[0].fallback,
            [
                super::Fallback::Group("backup".to_owned()),
                super::Fallback::Model {
                    provider: "other".to_owned(),
                    model: "x".to_owned(),
                },
            ]
        );
        let error = load(
            "[[providers]]\nid = \"plan\"\npreset = \"openai\"\nfallback = [\"group/missing\"]\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"plan\"]\n",
        )
        .expect_err("unknown fallback group");
        assert!(error.to_string().contains("unknown group `missing`"));
    }

    #[test]
    fn d2_group_members_keep_their_manual_order() {
        let config = load(
            r#"
            [[providers]]
            id = "a"
            preset = "openai"
            [[providers]]
            id = "b"
            preset = "openai"
            [[routes]]
            model = "group/ordered"
            routing = "order"
            upstreams = [{ provider = "b", model = "second" }, { provider = "a", model = "first" }]
            "#,
        )
        .expect("ordered group");
        let upstreams = &config.routes[0].upstreams;
        assert_eq!(upstreams[0].provider(), Some("b"));
        assert_eq!(upstreams[0].model.as_deref(), Some("second"));
        assert_eq!(upstreams[1].provider(), Some("a"));
        assert_eq!(upstreams[1].model.as_deref(), Some("first"));
    }

    #[test]
    fn d3_route_sink_is_opt_in() {
        let config = load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"demo\"\nsink = true\nupstreams = [\"one\"]\n[[routes]]\nmodel = \"plain\"\nupstreams = [\"one\"]\n",
        )
        .expect("sink setting");
        assert!(config.routes[0].sink);
        assert!(!config.routes[1].sink);
    }

    #[test]
    fn d5_subscription_routing_and_nested_groups_validate() {
        let config = load(
            r#"
            [[providers]]
            id = "one"
            preset = "openai"
            [[subscriptions]]
            id = "codex"
            accounts = "*"
            routing = "weight"
            [[routes]]
            model = "group/inner"
            upstreams = [{ subscription = "codex" }]
            [[routes]]
            model = "group/outer"
            routing = "weight"
            upstreams = [{ group = "inner", weight = 2 }, "one"]
            [[routes]]
            model = "demo"
            upstreams = [{ group = "outer" }]
            "#,
        )
        .expect("nested groups");
        assert_eq!(config.subscriptions[0].routing, "weight");
        assert_eq!(config.routes[1].upstreams[0].group(), Some("inner"));
        let cycle = load(
            "[[providers]]\nid = \"one\"\npreset = \"openai\"\n[[routes]]\nmodel = \"group/a\"\nupstreams = [{ group = \"b\" }]\n[[routes]]\nmodel = \"group/b\"\nupstreams = [{ group = \"a\" }]\n[[routes]]\nmodel = \"demo\"\nupstreams = [\"one\"]\n",
        )
        .expect_err("group cycle");
        assert!(cycle.to_string().contains("cycle"), "{cycle}");
        let unknown =
            load("[[subscriptions]]\nid = \"codex\"\naccounts = \"*\"\nrouting = \"unknown\"\n")
                .expect_err("unknown subscription routing");
        assert!(unknown.to_string().contains("unknown routing"));
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
            protocol = "not-a-codec"
            base_url = "https://example.test"
            "#,
        )
        .expect("unknown id is still a string");
        assert_eq!(stored.providers[0].protocol.as_deref(), Some("not-a-codec"));
        assert!(stored.warnings.is_empty());
    }

    #[test]
    fn the_first_structural_error_is_reported() {
        let error = load(
            r#"
            [[providers]]
            id = "primary"
            colour = "blue"
            base_url = "http://127.0.0.1:9"
            "#,
        )
        .expect_err("unknown field");
        assert!(error.to_string().contains("colour"));

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
            provider
                .options
                .get("access_key_id")
                .and_then(super::Value::as_str),
            Some("AKIAIOSFODNN7EXAMPLE")
        );
        assert_eq!(
            provider
                .options
                .get("secret_access_key")
                .and_then(super::Value::as_str),
            Some("secret")
        );
        assert_eq!(
            provider
                .options
                .get("region")
                .and_then(super::Value::as_str),
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
            same.providers[0]
                .options
                .get("region")
                .and_then(super::Value::as_str),
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
            provider
                .options
                .get("project")
                .and_then(super::Value::as_str),
            Some("My-Project")
        );
        assert_eq!(
            provider
                .options
                .get("location")
                .and_then(super::Value::as_str),
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
                .and_then(super::Value::as_str),
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
    fn route_may_name_a_subscription() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            preset = "openai"
            [[subscriptions]]
            id = "codex"
            accounts = "*"
            [[routes]]
            model = "demo"
            upstreams = [{ subscription = "codex", model = "gpt" }]
            "#,
        )
        .expect("subscription route");
        assert_eq!(config.routes[0].upstreams[0].subscription(), Some("codex"));
        assert_eq!(config.routes[0].upstreams[0].provider(), None);
        assert_eq!(config.routes[0].upstreams[0].model.as_deref(), Some("gpt"));
    }

    #[test]
    fn route_rejects_provider_and_subscription_together() {
        let error = load(
            r#"
            [[providers]]
            id = "primary"
            preset = "openai"
            [[subscriptions]]
            id = "codex"
            accounts = "*"
            [[routes]]
            model = "demo"
            upstreams = [{ provider = "primary", subscription = "codex" }]
            "#,
        )
        .expect_err("both keys");
        assert!(
            error.to_string().contains("both provider and subscription"),
            "{error}"
        );
    }

    #[test]
    fn same_id_provider_and_subscription_in_route_is_config_error() {
        let error = load(
            r#"
            [[providers]]
            id = "codex"
            preset = "openai"
            [[subscriptions]]
            id = "codex"
            accounts = "*"
            [[routes]]
            model = "demo"
            upstreams = ["codex", { subscription = "codex" }]
            "#,
        )
        .expect_err("same id twice");
        assert!(
            error
                .to_string()
                .contains("names `codex` as both a provider and a subscription"),
            "{error}"
        );
        load(
            r#"
            [[providers]]
            id = "codex"
            preset = "openai"
            [[subscriptions]]
            id = "codex"
            accounts = "*"
            [[routes]]
            model = "one"
            upstreams = ["codex"]
            [[routes]]
            model = "two"
            upstreams = [{ subscription = "codex" }]
            "#,
        )
        .expect("separate routes may reuse the id");
    }

    #[test]
    fn route_rejects_unknown_or_empty_subscription() {
        for (upstream, message) in [
            (
                r#"{ subscription = "other" }"#,
                "unknown subscription `other`",
            ),
            (r#"{ subscription = "" }"#, "names an empty subscription"),
            (r#"{ provider = "" }"#, "names an empty provider"),
            (r#"{ model = "x" }"#, "needs provider or subscription"),
            (
                r#"{ subscription = "codex", model = "" }"#,
                "empty model for subscription `codex`",
            ),
            (
                r#"{ subscription = "codex", max_output_tokens = 0 }"#,
                "max_output_tokens 0 for subscription `codex`",
            ),
        ] {
            let error = load(&format!(
                r#"
                [[subscriptions]]
                id = "codex"
                accounts = "*"
                [[routes]]
                model = "demo"
                upstreams = [{upstream}]
                "#
            ))
            .expect_err(upstream);
            assert!(error.to_string().contains(message), "{upstream}: {error}");
        }
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
                .and_then(super::Value::as_str),
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
    fn kind_and_short_protocol_names_rewrite_and_warn() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "openai_compat"
            protocol = "responses"
            base_url = "http://127.0.0.1:9"
            [[providers]]
            id = "studio"
            kind = "generative"
            base_url = "https://example.test"
            [[providers]]
            id = "aws"
            protocol = "converse"
            base_url = "https://example.test"
            "#,
        )
        .expect("legacy names");
        assert_eq!(
            config.providers[0].protocol.as_deref(),
            Some("openai-responses")
        );
        assert_eq!(
            config.providers[1].protocol.as_deref(),
            Some("generative-language")
        );
        assert_eq!(
            config.providers[2].protocol.as_deref(),
            Some("bedrock-converse")
        );
        let warnings = config.warnings.join("\n");
        assert!(warnings.contains("protocol = \"openai-responses\""));
        assert!(warnings.contains("protocol = \"generative-language\""));
        assert!(warnings.contains("renamed to `bedrock-converse`"));
        assert!(warnings.contains("removed next release"));
        let conflict = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "anthropic"
            protocol = "chat"
            base_url = "http://127.0.0.1:9"
            "#,
        )
        .expect_err("conflict");
        assert!(conflict.to_string().contains("different protocols"));
        assert!(!conflict.to_string().contains("replace-me"));
        let unknown = load(
            r#"
            [[providers]]
            id = "primary"
            kind = "vertex"
            base_url = "http://127.0.0.1:9"
            "#,
        )
        .expect_err("kind");
        assert!(unknown.to_string().contains("unknown kind `vertex`"));
    }

    #[test]
    fn a_blank_protocol_is_absent_and_surrounding_space_is_trimmed() {
        let blank = load(
            r#"
            [[providers]]
            id = "bare"
            protocol = "   "
            base_url = "https://example.test"
            "#,
        )
        .expect_err("blank");
        assert!(blank.to_string().contains("needs a protocol or a preset"));
        let empty = load(
            r#"
            [[providers]]
            id = "gemini"
            preset = "google"
            protocol = ""
            "#,
        )
        .expect("empty with a preset");
        assert!(empty.providers[0].protocol.is_none());
        assert!(empty.warnings.is_empty());
        let padded = load(
            r#"
            [[providers]]
            id = "studio"
            protocol = " chat "
            base_url = "https://example.test"
            "#,
        )
        .expect("padded short name");
        assert_eq!(padded.providers[0].protocol.as_deref(), Some("openai-chat"));
        assert!(
            padded
                .warnings
                .iter()
                .any(|warning| warning.contains("renamed to `openai-chat`"))
        );
        let canonical = load(
            r#"
            [[providers]]
            id = "studio"
            protocol = " openai-chat "
            base_url = "https://example.test"
            "#,
        )
        .expect("padded codec id");
        assert_eq!(
            canonical.providers[0].protocol.as_deref(),
            Some("openai-chat")
        );
        assert!(canonical.warnings.is_empty());
        let with_kind = load(
            r#"
            [[providers]]
            id = "backup"
            kind = "anthropic"
            protocol = "  "
            base_url = "https://example.test"
            "#,
        )
        .expect("blank protocol with kind");
        assert_eq!(
            with_kind.providers[0].protocol.as_deref(),
            Some("anthropic-messages")
        );
        assert!(
            with_kind
                .warnings
                .iter()
                .all(|warning| !warning.contains("renamed"))
        );
    }

    #[test]
    fn the_v0_1_0_example_rewrites_kind_and_chat() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/v0.1.0-hofund.toml");
        let text = std::fs::read_to_string(&path).expect("fixture");
        let config = load(&text).expect("v0.1.0");
        assert_eq!(config.providers[0].protocol.as_deref(), Some("openai-chat"));
        assert_eq!(
            config.providers[1].protocol.as_deref(),
            Some("anthropic-messages")
        );
        let warnings = config.warnings.join("\n");
        assert!(warnings.contains("protocol = \"openai-chat\""));
        assert!(warnings.contains("protocol = \"anthropic-messages\""));
        assert!(warnings.contains("renamed to `openai-chat`"));
        assert!(!warnings.contains("replace-me"));
    }

    #[test]
    fn empty_strings_are_absent_in_legacy_keys_and_options() {
        let legacy = load(
            r#"
            [[providers]]
            id = "vertex"
            preset = "google-vertex"
            credentials = ""
            impersonate = "   "
            session_token = ""
            project = ""
            "#,
        )
        .expect("empty legacy");
        assert!(legacy.providers[0].options.is_empty());
        assert!(legacy.warnings.is_empty());
        let options = load(
            r#"
            [[providers]]
            id = "vertex"
            preset = "google-vertex"
            [providers.options]
            credentials = ""
            location = "   "
            port = 0
            enterprise = false
            tags = ["", "kept"]
            [providers.options.extra]
            note = ""
            name = "kept"
            "#,
        )
        .expect("empty options");
        let table = &options.providers[0].options;
        assert!(table.get("credentials").is_none());
        assert!(table.get("location").is_none());
        assert_eq!(table.get("port"), Some(&super::Value::Integer(0)));
        assert_eq!(table.get("enterprise"), Some(&super::Value::Boolean(false)));
        assert_eq!(
            table.get("tags"),
            Some(&super::Value::Array(vec![
                super::Value::String(String::new()),
                super::Value::String("kept".to_owned()),
            ]))
        );
        match table.get("extra") {
            Some(super::Value::Table(extra)) => {
                assert!(extra.get("note").is_none());
                assert_eq!(
                    extra.get("name").and_then(super::Value::as_str),
                    Some("kept")
                );
            }
            other => panic!("nested table, got {other:?}"),
        }
        assert!(options.warnings.is_empty());
        let typed = load(
            r#"
            [[providers]]
            id = "vertex"
            preset = "google-vertex"
            region = "us-east-1"
            [providers.options]
            region = 1
            "#,
        )
        .expect_err("type conflict");
        assert!(typed.to_string().contains("different values"));
        assert!(!typed.to_string().contains("us-east-1"));
    }

    #[test]
    fn duplicate_subscriptions_and_empty_option_keys_are_rejected() {
        let error = load(
            r#"
            [[subscriptions]]
            id = "copilot"
            accounts = "*"
            [[subscriptions]]
            id = "copilot"
            accounts = "work"
            "#,
        )
        .expect_err("duplicate");
        assert!(error.to_string().contains("repeated"));
        let error = load(
            r#"
            [[subscriptions]]
            id = "copilot"
            "#,
        )
        .expect_err("missing accounts");
        assert!(error.to_string().contains("accounts"));
        let error = load(
            r#"
            [[providers]]
            id = "primary"
            protocol = "openai-chat"
            base_url = "https://example.test"
            [providers.options]
            "" = "x"
            "#,
        )
        .expect_err("empty provider key");
        assert!(error.to_string().contains("empty option key"));
        let error = load(
            r#"
            [[subscriptions]]
            id = "copilot"
            accounts = "*"
            [subscriptions.options]
            "" = "x"
            "#,
        )
        .expect_err("empty subscription key");
        assert!(error.to_string().contains("empty option key"));
    }

    #[test]
    fn k1_provider_keys_validate_deduplicate_and_redact() {
        let config = load(
            r#"
            [[providers]]
            id = "primary"
            preset = "openai"
            api_key = "env:FIRST_KEY"
            key_name = "Personal"
            key_protocol = "openai-chat"
            key_plan = "api"
            key_weight = 2
            [[providers.keys]]
            key = "env:SECOND_KEY"
            name = "Team"
            protocol = "openai-responses"
            plan = "coding"
            weight = 3
            [[providers.keys]]
            key = "env:SECOND_KEY"
            on = false
            [[providers.keys]]
            key = "   "
            [[providers.keys]]
            key = "env:FIRST_KEY"
            "#,
        )
        .expect("provider keys");
        let provider = &config.providers[0];
        let keys = provider.key_entries();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].name, "Personal");
        assert_eq!(keys[0].protocol.as_deref(), Some("openai-chat"));
        assert_eq!(keys[0].plan.as_deref(), Some("api"));
        assert_eq!(keys[0].weight, 2);
        assert_eq!(keys[1].name, "Team");
        assert_eq!(keys[1].plan.as_deref(), Some("coding"));
        assert!(keys[1].on);
        assert_eq!(keys[1].weight, 3);
        let debug = format!("{config:?} {provider:?} {keys:?}");
        assert!(!debug.contains("FIRST_KEY"), "{debug}");
        assert!(!debug.contains("SECOND_KEY"), "{debug}");

        for (field, message) in [
            ("weight = 1001", "0 to 1000"),
            ("protocol = \"carrier-pigeon\"", "unknown key protocol"),
            ("unknown = \"paid\"", "unknown field"),
        ] {
            let error = load(&format!(
                "[[providers]]\nid = \"primary\"\npreset = \"openai\"\n[[providers.keys]]\nkey = \"k\"\n{field}\n"
            ))
            .expect_err(field);
            assert!(error.to_string().contains(message), "{field}: {error}");
        }
        load(
            "[[providers]]\nid = \"primary\"\npreset = \"openai\"\napi_key = \"k\"\nkey_weight = 1001\n",
        )
        .expect_err("first-key weight");
        // Zero, like an absent weight, counts as one, as in the reference.
        let zero = load(
            "[[providers]]\nid = \"primary\"\npreset = \"openai\"\napi_key = \"k\"\nkey_weight = 0\n[[providers.keys]]\nkey = \"k2\"\nweight = 0\n",
        )
        .expect("zero weights");
        load("[[providers]]\nid = \"primary\"\npreset = \"openai\"\napi_key = \"k\"\nkey_weight = 1000\n")
            .expect("largest weight");
        let zero_keys = zero.providers[0].key_entries();
        assert_eq!(zero_keys[0].weight, 1);
        assert_eq!(zero_keys[1].weight, 1);
    }

    #[test]
    fn k6_config_writes_keep_env_specs_as_written() {
        let original = r#"[[providers]]
id = "primary"
preset = "openai"
api_key = "env:FIRST_KEY"
[[providers.keys]]
key = "env:SECOND_KEY"
name = "Team"
protocol = "openai-chat"
weight = 2
"#;
        let config = load(original).expect("original");
        let mut keys = config.providers[0].key_entries();
        let promoted = keys.remove(1);
        keys.insert(0, promoted);
        let edited = super::edit_provider_keys(original, "primary", &keys).expect("edited");
        assert!(edited.contains("api_key = \"env:SECOND_KEY\""), "{edited}");
        assert!(edited.contains("key = \"env:FIRST_KEY\""), "{edited}");
        let provider = &load(&edited).expect("edited config").providers[0];
        assert_eq!(provider.key_name, "Team");
        assert_eq!(provider.key_protocol.as_deref(), Some("openai-chat"));
        assert_eq!(provider.key_weight, 2);
    }

    #[test]
    fn d26_1_config_at_once_validates_and_writes_provider_and_subscription() {
        let original = "# keep\n[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"k\"\n[[subscriptions]]\nid = \"sub\"\naccounts = \"*\"\n";
        let edited = super::edit_at_once(original, "p", Some(2)).expect("provider edit");
        let edited = super::edit_at_once(&edited, "sub", Some(0)).expect("subscription edit");
        assert!(edited.starts_with("# keep\n"), "{edited}");
        let config = load(&edited).expect("edited config");
        assert_eq!(config.providers[0].at_once, Some(2));
        assert_eq!(config.subscriptions[0].at_once, Some(0));
        let cleared = super::edit_at_once(&edited, "p", None).expect("default");
        assert_eq!(load(&cleared).expect("cleared").providers[0].at_once, None);
        assert!(cleared.contains("at_once = 0"), "{cleared}");
        for bad in ["at_once = 1001", "at_once = -1"] {
            let text =
                format!("[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"k\"\n{bad}\n");
            assert!(load(&text).is_err(), "{bad}");
        }
        assert!(super::edit_at_once(original, "p", Some(1001)).is_err());
        assert!(super::edit_at_once(original, "missing", Some(1)).is_err());
    }

    #[test]
    fn debug_and_warnings_omit_secret_values() {
        let config = load(
            r#"
            [[providers]]
            id = "bedrock"
            protocol = "bedrock-converse"
            base_url = "https://example.test"
            api_key = "APIKEYSECRET"
            access_key_id = "LEGACYKEY"
            headers = { Authorization = "HEADERSECRET" }
            [providers.options]
            secret_access_key = "SECRETVALUE"
            [[subscriptions]]
            id = "copilot"
            accounts = "*"
            [subscriptions.options]
            client_secret = "SUBSECRET"
            "#,
        )
        .expect("secrets");
        let text = format!("{config:?}");
        for secret in [
            "APIKEYSECRET",
            "HEADERSECRET",
            "SECRETVALUE",
            "LEGACYKEY",
            "SUBSECRET",
        ] {
            assert!(!text.contains(secret), "{secret} leaked in {text}");
            assert!(
                config
                    .warnings
                    .iter()
                    .all(|warning| !warning.contains(secret)),
                "{secret} leaked in {:?}",
                config.warnings
            );
        }
        assert!(text.contains("secret_access_key"));
        assert!(text.contains("access_key_id"));
        assert!(text.contains("client_secret"));
        assert!(text.contains("<redacted>"));
        let raw = format!("{:?}", config.providers[0]);
        assert!(!raw.contains("LEGACYKEY"), "{raw}");
        assert!(!raw.contains("APIKEYSECRET"), "{raw}");
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
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../examples/hofund.toml");
        let text = std::fs::read_to_string(&path).expect("example");
        let config = load(&text).expect("load");
        assert!(config.providers.len() >= 2);
        assert!(!config.routes.is_empty());
        assert!(config.routes.iter().any(|route| route.upstreams.len() >= 2));
    }

    #[test]
    fn ensure_config_file_writes_once() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("hofund.toml");
        assert!(super::ensure_config_file(&path).expect("create"));
        assert!(path.is_file());
        std::fs::write(&path, "listen = \"127.0.0.1:9\"\n").expect("overwrite");
        assert!(!super::ensure_config_file(&path).expect("keep"));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("127.0.0.1:9"));
    }
}
