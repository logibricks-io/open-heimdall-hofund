//! Data the traits pass around. None of these types perform I/O.

use std::time::{Duration, SystemTime};

use hofund_ir::{ContentPart, Message, Request, Role};

use secrecy::SecretString;

use crate::error::PluginError;
use crate::id::{AccountId, PluginId, PluginKey, PluginKind, ProtocolId, TierId};

/// One account in the store. The secret stays in the store, not on this value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Account {
    key: PluginKey,
    id: AccountId,
    label: String,
}

impl Account {
    /// An account for `plugin`.
    ///
    /// Plugins use [`AccountsHandle::account`](crate::AccountsHandle::account),
    /// which fills `plugin` from the [`HostContext`](crate::HostContext).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `label` is empty.
    pub(crate) fn new(
        plugin: PluginKey,
        id: AccountId,
        label: impl Into<String>,
    ) -> Result<Self, PluginError> {
        let label = label.into();
        if label.is_empty() {
            return Err(PluginError::config("account label is empty"));
        }
        Ok(Self {
            key: plugin,
            id,
            label,
        })
    }

    /// Rebuilds a subscription account stored before kinds were explicit.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `label` is empty.
    #[doc(hidden)]
    pub fn from_stored(
        plugin: PluginId,
        id: AccountId,
        label: impl Into<String>,
    ) -> Result<Self, PluginError> {
        Self::from_stored_scoped(PluginKey::new(PluginKind::Subscription, plugin), id, label)
    }

    /// Rebuilds an account with its storage kind.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `label` is empty.
    #[doc(hidden)]
    pub fn from_stored_scoped(
        plugin: PluginKey,
        id: AccountId,
        label: impl Into<String>,
    ) -> Result<Self, PluginError> {
        Self::new(plugin, id, label)
    }

    /// The plugin that owns the account.
    #[must_use]
    pub const fn plugin(&self) -> &PluginId {
        self.key.id()
    }

    /// The kind-qualified plugin identity that owns the account.
    #[must_use]
    pub const fn key(&self) -> &PluginKey {
        &self.key
    }

    /// The account id.
    #[must_use]
    pub fn id(&self) -> &AccountId {
        &self.id
    }

    /// A label safe to show in the UI.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// One quota window returned by a poll.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaWindow {
    name: String,
    used: u64,
    limit: Option<u64>,
    resets_at: Option<SystemTime>,
}

impl QuotaWindow {
    /// A window named `name`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `name` is empty or `used` is greater than
    /// `limit`.
    pub fn new(
        name: impl Into<String>,
        used: u64,
        limit: Option<u64>,
        resets_at: Option<SystemTime>,
    ) -> Result<Self, PluginError> {
        let name = name.into();
        if name.is_empty() {
            return Err(PluginError::config("quota window name is empty"));
        }
        if let Some(limit) = limit
            && used > limit
        {
            return Err(PluginError::config("quota used exceeds limit"));
        }
        Ok(Self {
            name,
            used,
            limit,
            resets_at,
        })
    }

    /// Window name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Units already consumed.
    #[must_use]
    pub const fn used(&self) -> u64 {
        self.used
    }

    /// The cap, when the vendor publishes one.
    #[must_use]
    pub const fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// When the window resets, when the vendor says so.
    #[must_use]
    pub const fn resets_at(&self) -> Option<SystemTime> {
        self.resets_at
    }
}

/// Rate-limit headers seen on a call, including a successful response.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaObservation {
    plugin: PluginId,
    retry_after: Option<Duration>,
    remaining: Option<u64>,
    window: Option<String>,
}

impl QuotaObservation {
    /// An observation for `plugin`.
    ///
    /// Plugins use [`ObserveHandle::observation`](crate::ObserveHandle::observation).
    #[must_use]
    pub(crate) fn new(plugin: PluginId) -> Self {
        Self {
            plugin,
            retry_after: None,
            remaining: None,
            window: None,
        }
    }

    /// Rebuilds an observation the kernel already accepted.
    #[doc(hidden)]
    #[must_use]
    pub fn from_stored(plugin: PluginId) -> Self {
        Self::new(plugin)
    }

    /// Sets the retry hint.
    #[must_use]
    pub fn with_retry_after(mut self, after: Duration) -> Self {
        self.retry_after = Some(after);
        self
    }

    /// Sets the remaining units.
    #[must_use]
    pub fn with_remaining(mut self, remaining: u64) -> Self {
        self.remaining = Some(remaining);
        self
    }

    /// The plugin that saw the headers.
    #[must_use]
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// How long to wait, when the headers said.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Units still available, when the headers said.
    #[must_use]
    pub const fn remaining(&self) -> Option<u64> {
        self.remaining
    }

    /// Window name, when the headers named one.
    #[must_use]
    pub fn window(&self) -> Option<&str> {
        self.window.as_deref()
    }
}

/// The authentication headers learned for an Anthropic-wire call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthChoice {
    /// Send both `x-api-key` and `Authorization: Bearer`.
    Both,
    /// Send only `x-api-key`.
    XApiKey,
    /// Send only `Authorization: Bearer`.
    Bearer,
}

/// One model a list call returned.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelSpec {
    id: String,
    display_name: Option<String>,
    context: u64,
    max_output: u64,
    input_modalities: Vec<String>,
    output_modalities: Vec<String>,
    image_input: Option<bool>,
    endpoints: Vec<String>,
    draws: bool,
    films: bool,
    decides: bool,
    efforts: Vec<String>,
    web_search: Option<String>,
    service_tiers: Vec<String>,
}

impl ModelSpec {
    /// A model with a non-empty id.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `id` is empty.
    pub fn new(id: impl Into<String>, display_name: Option<String>) -> Result<Self, PluginError> {
        let id = id.into();
        if id.is_empty() {
            return Err(PluginError::config("model id is empty"));
        }
        Ok(Self {
            id,
            display_name,
            context: 0,
            max_output: 0,
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
            image_input: None,
            endpoints: Vec::new(),
            draws: false,
            films: false,
            decides: false,
            efforts: Vec::new(),
            web_search: None,
            service_tiers: Vec::new(),
        })
    }

    /// Sets the positive context-window size. Zero leaves it unknown.
    #[must_use]
    pub fn with_context(mut self, context: u64) -> Self {
        self.context = context;
        self
    }

    /// Sets the positive maximum output size. Zero leaves it unknown.
    #[must_use]
    pub fn with_max_output(mut self, max_output: u64) -> Self {
        self.max_output = max_output;
        self
    }

    /// Sets the input modalities reported by the vendor.
    #[must_use]
    pub fn with_input_modalities(mut self, modalities: Vec<String>) -> Self {
        self.input_modalities = modalities;
        self
    }

    /// Sets the output modalities reported by the vendor.
    #[must_use]
    pub fn with_output_modalities(mut self, modalities: Vec<String>) -> Self {
        self.output_modalities = modalities;
        self
    }

    /// Sets whether image input is supported, or `None` when it is unknown.
    #[must_use]
    pub const fn with_image_input(mut self, image_input: Option<bool>) -> Self {
        self.image_input = image_input;
        self
    }

    /// Sets the endpoint names reported by the vendor.
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: Vec<String>) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Marks whether the model draws images.
    #[must_use]
    pub const fn with_draws(mut self, draws: bool) -> Self {
        self.draws = draws;
        self
    }

    /// Marks whether the model makes films.
    #[must_use]
    pub const fn with_films(mut self, films: bool) -> Self {
        self.films = films;
        self
    }

    /// Marks whether the model is a decision model.
    #[must_use]
    pub const fn with_decides(mut self, decides: bool) -> Self {
        self.decides = decides;
        self
    }

    /// Sets the supported reasoning effort names.
    #[must_use]
    pub fn with_efforts(mut self, efforts: Vec<String>) -> Self {
        self.efforts = efforts;
        self
    }

    /// Sets native web search. Any other value leaves web search unspecified.
    #[must_use]
    pub fn with_web_search(mut self, web_search: impl Into<String>) -> Self {
        let web_search = web_search.into();
        self.web_search = (web_search == "native").then_some(web_search);
        self
    }

    /// Sets the service tiers reported by the vendor.
    #[must_use]
    pub fn with_service_tiers(mut self, service_tiers: Vec<String>) -> Self {
        self.service_tiers = service_tiers;
        self
    }

    /// The model id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// A display name, when the vendor sent one.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// The context-window size, or zero when unknown.
    #[must_use]
    pub const fn context(&self) -> u64 {
        self.context
    }

    /// The maximum output size, or zero when unknown.
    #[must_use]
    pub const fn max_output(&self) -> u64 {
        self.max_output
    }

    /// Input modalities, empty when the vendor omitted them.
    #[must_use]
    pub fn input_modalities(&self) -> &[String] {
        &self.input_modalities
    }

    /// Output modalities, empty when the vendor omitted them.
    #[must_use]
    pub fn output_modalities(&self) -> &[String] {
        &self.output_modalities
    }

    /// Whether image input is supported, or `None` when unknown.
    #[must_use]
    pub const fn image_input(&self) -> Option<bool> {
        self.image_input
    }

    /// Endpoint names, empty when the vendor omitted them.
    #[must_use]
    pub fn endpoints(&self) -> &[String] {
        &self.endpoints
    }

    /// Whether the model draws images.
    #[must_use]
    pub const fn draws(&self) -> bool {
        self.draws
    }

    /// Whether the model makes films.
    #[must_use]
    pub const fn films(&self) -> bool {
        self.films
    }

    /// Whether the model is a decision model.
    #[must_use]
    pub const fn decides(&self) -> bool {
        self.decides
    }

    /// Supported reasoning effort names.
    #[must_use]
    pub fn efforts(&self) -> &[String] {
        &self.efforts
    }

    /// `native` when native web search was reported.
    #[must_use]
    pub fn web_search(&self) -> Option<&str> {
        self.web_search.as_deref()
    }

    /// Service tiers, empty when the vendor omitted them.
    #[must_use]
    pub fn service_tiers(&self) -> &[String] {
        &self.service_tiers
    }
}

/// A page of models and the facts a host learned while obtaining it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelList {
    models: Vec<ModelSpec>,
    dropped: Vec<String>,
    next: Option<String>,
    kind: crate::ListKind,
    base_model_fallback: bool,
    v1_fallback: bool,
    auth_choice: Option<AuthChoice>,
    source_url: Option<String>,
}

impl ModelList {
    /// A live-list page. `next` is the token for the following page.
    #[must_use]
    pub fn new(models: Vec<ModelSpec>, next: Option<String>) -> Self {
        Self {
            models,
            dropped: Vec::new(),
            next,
            kind: crate::ListKind::Live,
            base_model_fallback: false,
            v1_fallback: false,
            auth_choice: None,
            source_url: None,
        }
    }

    /// Records the list URL that answered, for host-side response deduplication.
    ///
    /// Any query is replaced by `<redacted>` and any fragment is removed, so
    /// this value is safe for the derived `Debug` output.
    #[must_use]
    pub fn with_source_url(mut self, url: impl Into<String>) -> Self {
        let url = url.into();
        let without_fragment = url.split('#').next().unwrap_or(&url);
        self.source_url = Some(match without_fragment.split_once('?') {
            Some((base, _)) => format!("{base}?<redacted>"),
            None => without_fragment.to_owned(),
        });
        self
    }

    /// Records ids removed by the plugin's model classifier.
    #[must_use]
    pub fn with_dropped(mut self, dropped: Vec<String>) -> Self {
        self.dropped = dropped;
        self
    }

    /// Ids removed by the plugin's model classifier.
    #[must_use]
    pub fn dropped(&self) -> &[String] {
        &self.dropped
    }

    /// Sets the kind of result actually returned.
    #[must_use]
    pub const fn with_kind(mut self, kind: crate::ListKind) -> Self {
        self.kind = kind;
        self
    }

    /// Labels an Azure base-model result as `base models`, not deployments.
    #[must_use]
    pub const fn with_base_model_fallback(mut self, fallback: bool) -> Self {
        self.base_model_fallback = fallback;
        self
    }

    /// Marks a list found only at `{base}/v1/models` for an unversioned base.
    #[must_use]
    pub const fn with_v1_fallback(mut self, fallback: bool) -> Self {
        self.v1_fallback = fallback;
        self
    }

    /// Sets the learned Anthropic-wire authentication choice.
    #[must_use]
    pub const fn with_auth_choice(mut self, choice: AuthChoice) -> Self {
        self.auth_choice = Some(choice);
        self
    }

    /// Models on this page.
    #[must_use]
    pub fn models(&self) -> &[ModelSpec] {
        &self.models
    }

    /// The next page token, when there is another page.
    #[must_use]
    pub fn next(&self) -> Option<&str> {
        self.next.as_deref()
    }

    /// The list URL that answered, with any query redacted.
    #[must_use]
    pub fn source_url(&self) -> Option<&str> {
        self.source_url.as_deref()
    }

    /// The kind of result actually returned.
    #[must_use]
    pub const fn kind(&self) -> crate::ListKind {
        self.kind
    }

    /// Whether this is an Azure `base models` fallback, not a deployment list.
    #[must_use]
    pub const fn base_model_fallback(&self) -> bool {
        self.base_model_fallback
    }

    /// Whether only the `/v1/models` candidate of an unversioned base answered.
    #[must_use]
    pub const fn v1_fallback(&self) -> bool {
        self.v1_fallback
    }

    /// The learned Anthropic-wire authentication choice, when one was learned.
    #[must_use]
    pub const fn auth_choice(&self) -> Option<AuthChoice> {
        self.auth_choice
    }
}

/// A page request for [`crate::UpstreamProvider::list_models`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListQuery {
    limit: Option<u32>,
    page: Option<String>,
    auth_choice: Option<AuthChoice>,
}

impl ListQuery {
    /// The first page, with no limit or learned authentication choice.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: None,
            page: None,
            auth_choice: None,
        }
    }

    /// Sets the page size.
    #[must_use]
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Sets the page token from a previous [`ModelList::next`].
    #[must_use]
    pub fn with_page(mut self, page: impl Into<String>) -> Self {
        self.page = Some(page.into());
        self
    }

    /// Sets the learned Anthropic-wire authentication choice.
    #[must_use]
    pub const fn with_auth_choice(mut self, choice: AuthChoice) -> Self {
        self.auth_choice = Some(choice);
        self
    }

    /// The page size, when the caller set one.
    #[must_use]
    pub const fn limit(&self) -> Option<u32> {
        self.limit
    }

    /// The page token, when this is not the first page.
    #[must_use]
    pub fn page(&self) -> Option<&str> {
        self.page.as_deref()
    }

    /// The learned Anthropic-wire authentication choice supplied by the host.
    #[must_use]
    pub const fn auth_choice(&self) -> Option<AuthChoice> {
        self.auth_choice
    }
}

impl Default for ListQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// What `probe` sends. One minimal request for one protocol.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbeQuery {
    protocol: ProtocolId,
    model: String,
    auth_choice: Option<AuthChoice>,
    extra_body: Option<bytes::Bytes>,
}

impl ProbeQuery {
    /// A probe of `model` on `protocol`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `model` is empty.
    pub fn new(protocol: ProtocolId, model: impl Into<String>) -> Result<Self, PluginError> {
        let model = model.into();
        if model.is_empty() {
            return Err(PluginError::config("probe model is empty"));
        }
        Ok(Self {
            protocol,
            model,
            auth_choice: None,
            extra_body: None,
        })
    }

    /// The protocol under test.
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        self.protocol
    }

    /// The upstream model id to probe.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Sets the learned Anthropic-wire authentication choice.
    #[must_use]
    pub const fn with_auth_choice(mut self, choice: AuthChoice) -> Self {
        self.auth_choice = Some(choice);
        self
    }

    /// Sets opaque JSON object bytes for the plugin to validate and merge.
    #[must_use]
    pub fn with_extra_body(mut self, body: bytes::Bytes) -> Self {
        self.extra_body = Some(body);
        self
    }

    /// The learned Anthropic-wire authentication choice supplied by the host.
    #[must_use]
    pub const fn auth_choice(&self) -> Option<AuthChoice> {
        self.auth_choice
    }

    /// Opaque extra JSON object bytes, validated by the plugin or SDK merger.
    #[must_use]
    pub fn extra_body(&self) -> Option<&bytes::Bytes> {
        self.extra_body.as_ref()
    }

    /// A unary request with one short user message.
    #[must_use]
    pub fn minimal_request(&self) -> Request {
        let mut request = Request::new(self.model.clone());
        let mut message = Message::new(Role::User);
        message.content.push(ContentPart::text("hi"));
        request.messages.push(message);
        request
    }
}

impl std::fmt::Debug for ProbeQuery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProbeQuery")
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("auth_choice", &self.auth_choice)
            .field("extra_body", &self.extra_body.as_ref().map(|_| "<opaque>"))
            .finish()
    }
}

/// A quota poll.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QuotaQuery {
    account: Option<AccountId>,
}

impl QuotaQuery {
    /// A quota poll with no account filter.
    #[must_use]
    pub fn new() -> Self {
        Self { account: None }
    }

    /// Restricts the poll to one account.
    #[must_use]
    pub fn for_account(account: AccountId) -> Self {
        Self {
            account: Some(account),
        }
    }

    /// The account filter, when set.
    #[must_use]
    pub fn account(&self) -> Option<&AccountId> {
        self.account.as_ref()
    }
}

impl Default for QuotaQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// An `https` URL stored in a static sign-in flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StaticUrl {
    value: &'static str,
}

impl StaticUrl {
    /// An absolute `http` or `https` URL with no whitespace.
    ///
    /// # Panics
    ///
    /// Panics when `value` is not an absolute `http` or `https` URL.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(
            is_http_url(value),
            "sign-in URL must be absolute http or https"
        );
        Self { value }
    }

    /// The URL text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.value
    }
}

/// A binary name the process spawner resolves. Not a shell command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BinaryName {
    value: &'static str,
}

impl BinaryName {
    /// A single binary name such as `example-agent`.
    ///
    /// # Panics
    ///
    /// Panics when `value` is empty, has a path separator, or contains a shell
    /// metacharacter.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(
            is_binary_name(value),
            "bridge command must be one binary name"
        );
        Self { value }
    }

    /// The binary name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.value
    }
}

const fn is_http_url(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() || byte == b'"' || byte == b'\'' {
            return false;
        }
        index += 1;
    }
    let https = b"https://";
    let http = b"http://";
    starts_with(bytes, https) || starts_with(bytes, http)
}

const fn starts_with(bytes: &[u8], prefix: &[u8]) -> bool {
    if bytes.len() < prefix.len() {
        return false;
    }
    let mut index = 0;
    while index < prefix.len() {
        if bytes[index] != prefix[index] {
            return false;
        }
        index += 1;
    }
    true
}

const fn is_binary_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let ok = byte.is_ascii_alphanumeric()
            || byte == b'-'
            || byte == b'_'
            || byte == b'.'
            || byte == b'+';
        if !ok {
            return false;
        }
        index += 1;
    }
    true
}

/// How a subscription signs in. `Device` carries a static URL until follow-up N9.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignInFlow {
    /// Loopback PKCE.
    Pkce,
    /// A device code against a static verification URL.
    Device {
        /// Where the user opens the code.
        verification_url: StaticUrl,
    },
    /// A token the user pastes from a file or a command.
    TokenFile,
    /// A vendor binary the spawner resolves. Not a shell string.
    Bridge {
        /// The binary name, such as `example-agent`.
        command: BinaryName,
    },
}

/// What the UI shows while sign-in is in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignInChallenge {
    /// Open this URL.
    OpenUrl {
        /// Absolute URL.
        url: String,
    },
    /// Show a device code.
    DeviceCode {
        /// The code the user enters.
        user_code: String,
        /// Where the user enters it.
        verification_url: String,
    },
    /// Show instructions for a bridge command.
    Instructions {
        /// Text safe to show. It does not include a secret.
        text: String,
    },
}

impl SignInChallenge {
    /// An open-URL challenge.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `url` is not an absolute `http` or `https` URL.
    pub fn open_url(url: impl Into<String>) -> Result<Self, PluginError> {
        let url = url.into();
        if !is_http_url(&url) {
            return Err(PluginError::config(
                "sign-in URL must be absolute http or https",
            ));
        }
        Ok(Self::OpenUrl { url })
    }

    /// A device-code challenge.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the code is empty or the URL is not absolute.
    pub fn device_code(
        user_code: impl Into<String>,
        verification_url: impl Into<String>,
    ) -> Result<Self, PluginError> {
        let user_code = user_code.into();
        let verification_url = verification_url.into();
        if user_code.is_empty() {
            return Err(PluginError::config("device code is empty"));
        }
        if !is_http_url(&verification_url) {
            return Err(PluginError::config(
                "sign-in URL must be absolute http or https",
            ));
        }
        Ok(Self::DeviceCode {
            user_code,
            verification_url,
        })
    }

    /// Instruction text for a bridge.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `text` is empty.
    pub fn instructions(text: impl Into<String>) -> Result<Self, PluginError> {
        let text = text.into();
        if text.is_empty() {
            return Err(PluginError::config("sign-in instructions are empty"));
        }
        Ok(Self::Instructions { text })
    }
}

/// Input to `start_sign_in` and `complete_sign_in`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignInCtx {
    plugin: PluginId,
}

impl SignInCtx {
    /// A sign-in for `plugin`.
    #[must_use]
    pub fn new(plugin: PluginId) -> Self {
        Self { plugin }
    }

    /// The plugin signing in.
    #[must_use]
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
    }
}

/// Proof the UI collected. Secrets are redacted in [`Debug`].
#[derive(Clone)]
#[non_exhaustive]
pub enum SignInProof {
    /// An OAuth callback.
    Callback {
        /// Authorization code.
        code: SecretString,
        /// The `state` value the helper checked.
        state: String,
    },
    /// A device-flow code.
    Device {
        /// The user code.
        user_code: SecretString,
    },
    /// A pasted token.
    Token(SecretString),
}

impl SignInProof {
    /// A callback proof.
    #[must_use]
    pub fn callback(code: impl Into<SecretString>, state: impl Into<String>) -> Self {
        Self::Callback {
            code: code.into(),
            state: state.into(),
        }
    }

    /// A device proof.
    #[must_use]
    pub fn device(user_code: impl Into<SecretString>) -> Self {
        Self::Device {
            user_code: user_code.into(),
        }
    }

    /// A pasted token.
    #[must_use]
    pub fn token(token: impl Into<SecretString>) -> Self {
        Self::Token(token.into())
    }
}

impl std::fmt::Debug for SignInProof {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Callback { state, .. } => formatter
                .debug_struct("Callback")
                .field("code", &"<redacted>")
                .field("state", state)
                .finish(),
            Self::Device { .. } => formatter.write_str("Device { user_code: <redacted> }"),
            Self::Token(_) => formatter.write_str("Token(<redacted>)"),
        }
    }
}

/// Static metadata for a subscription plugin.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubscriptionSpec {
    plugin: PluginId,
    display_name: &'static str,
    flow: SignInFlow,
}

impl SubscriptionSpec {
    /// A spec. `display_name` must be non-empty. Generated code calls this.
    ///
    /// # Panics
    ///
    /// Panics when `display_name` is empty.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_static(
        plugin: PluginId,
        display_name: &'static str,
        flow: SignInFlow,
    ) -> Self {
        assert!(
            !display_name.is_empty(),
            "subscription display name is empty"
        );
        Self {
            plugin,
            display_name,
            flow,
        }
    }

    /// The plugin id.
    #[must_use]
    pub const fn plugin(&self) -> &PluginId {
        &self.plugin
    }

    /// The name the UI shows.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        self.display_name
    }

    /// How sign-in works.
    #[must_use]
    pub const fn flow(&self) -> SignInFlow {
        self.flow
    }
}

/// A non-empty, lowercase substring a tier slot matches against the model id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MatchNeedle {
    value: &'static str,
}

impl MatchNeedle {
    /// A non-empty lowercase needle such as `example-fable-`.
    ///
    /// Generated descriptions call this while building a [`Slot`].
    ///
    /// # Panics
    ///
    /// Panics when `value` is empty or contains an uppercase ASCII character.
    #[doc(hidden)]
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(!value.is_empty(), "model match needle is empty");
        let bytes = value.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            assert!(
                !bytes[index].is_ascii_uppercase(),
                "model match needle must be lowercase"
            );
            index += 1;
        }
        Self { value }
    }

    /// The needle text.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.value
    }
}

/// Where an agent description writes one value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Slot {
    /// The gateway origin, without a trailing `/v1`.
    #[doc(hidden)]
    Origin,
    /// The gateway origin plus `/v1`.
    #[doc(hidden)]
    V1,
    /// The bearer token.
    #[doc(hidden)]
    Token,
    /// A fixed string.
    #[doc(hidden)]
    Literal(&'static str),
    /// Writes a model only when the set key has no non-empty string.
    ///
    /// The value comes from the non-empty string at `from` in the same file,
    /// when present. Otherwise it comes from the only non-empty visible route;
    /// if there is not exactly one, the write is skipped.
    #[doc(hidden)]
    ModelIfUnset {
        /// Another key in the same file from which to take the model.
        from: Option<&'static str>,
    },
    /// A tier environment variable.
    ///
    /// The value is the non-empty string at the description's model key when
    /// that key is in the same logical file, otherwise the only non-empty
    /// visible route. The write is skipped when neither exists. When
    /// `when_model_matches` is set, the value is written only when it contains
    /// the needle, using ASCII case-insensitive matching.
    #[doc(hidden)]
    ModelTier {
        /// Tier name, such as `fable`.
        tier: TierId,
        /// When set, the write runs only if the model id contains this text.
        when_model_matches: Option<MatchNeedle>,
    },
    /// A JSON object with one entry per distinct non-empty visible model.
    ///
    /// Each entry has the shape `{ "name": "<id>", "variants": {} }`. The
    /// object is written as raw JSON, and this slot is valid only in JSON files.
    #[doc(hidden)]
    CatalogModels,
}

impl Slot {
    /// A model write guarded by an existing value at the destination key.
    #[doc(hidden)]
    #[must_use]
    pub const fn model_if_unset(from: Option<&'static str>) -> Self {
        Self::ModelIfUnset { from }
    }
}

/// How many models an agent file lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Visible {
    /// Every route.
    All,
    /// Only these model ids.
    Only(&'static [&'static str]),
}

/// Whether the agent talks to the gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentRoute {
    /// Write the gateway URL and token.
    Gateway,
    /// Picker only. No base URL.
    None,
}

/// File format a description can name. YAML is a later kernel change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileKind {
    /// TOML.
    Toml,
    /// JSON.
    Json,
    /// A dotenv file containing `KEY=VALUE` assignments.
    Env,
}

/// One ordered candidate location for a logical agent file.
///
/// Entries may share an id. The engine picks the first candidate whose path
/// is an existing file, otherwise the first with `create = true`, otherwise
/// the first candidate.
///
/// Path templates use `/` separators and start with `~` as the home segment.
/// They contain no drive letters, `..`, empty segments, or backslashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentFile {
    id: &'static str,
    kind: FileKind,
    linux: &'static str,
    macos: &'static str,
    windows: &'static str,
    env_override: Option<&'static str>,
    create: bool,
    env_replaces: Option<&'static str>,
}

impl AgentFile {
    /// A managed file. Paths are the template strings, not expanded paths.
    /// Generated code calls this. Files are created by default.
    ///
    /// # Panics
    ///
    /// Panics when `id` is empty or any path is empty.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_static(
        id: &'static str,
        kind: FileKind,
        linux: &'static str,
        macos: &'static str,
        windows: &'static str,
        env_override: Option<&'static str>,
    ) -> Self {
        assert!(!id.is_empty(), "agent file id is empty");
        assert!(
            !linux.is_empty() && !macos.is_empty() && !windows.is_empty(),
            "agent file path is empty"
        );
        if let Some(key) = env_override {
            assert!(!key.is_empty(), "agent file env override is empty");
        }
        Self {
            id,
            kind,
            linux,
            macos,
            windows,
            env_override,
            create: true,
            env_replaces: None,
        }
    }

    /// Sets whether the engine may create this candidate.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Sets the leading path-template prefix replaced by `env_override`.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_env_replaces(mut self, prefix: &'static str) -> Self {
        self.env_replaces = Some(prefix);
        self
    }

    /// File id inside the description.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// The file format.
    #[must_use]
    pub const fn kind(&self) -> FileKind {
        self.kind
    }

    /// Linux path template.
    #[must_use]
    pub const fn linux(&self) -> &'static str {
        self.linux
    }

    /// macOS path template.
    #[must_use]
    pub const fn macos(&self) -> &'static str {
        self.macos
    }

    /// Windows path template.
    #[must_use]
    pub const fn windows(&self) -> &'static str {
        self.windows
    }

    /// Environment variable that replaces the path when its value is absolute.
    /// Empty and relative values are treated as unset.
    #[must_use]
    pub const fn env_override(&self) -> Option<&'static str> {
        self.env_override
    }

    /// Whether the engine may create this candidate.
    #[must_use]
    pub const fn create(&self) -> bool {
        self.create
    }

    /// Leading path-template prefix replaced by `env_override`.
    ///
    /// When this is `None`, the prefix is the directory of the selected OS
    /// path template, meaning everything before its final `/`.
    #[must_use]
    pub const fn env_replaces(&self) -> Option<&'static str> {
        self.env_replaces
    }
}

/// One key a description writes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentSet {
    file: &'static str,
    key: &'static str,
    slot: Slot,
}

impl AgentSet {
    /// A write of `slot` into `key` of `file`. Generated code calls this.
    ///
    /// # Panics
    ///
    /// Panics when `file` or `key` is empty.
    #[doc(hidden)]
    #[must_use]
    pub const fn from_static(file: &'static str, key: &'static str, slot: Slot) -> Self {
        assert!(
            !file.is_empty() && !key.is_empty(),
            "agent set file and key are required"
        );
        Self { file, key, slot }
    }

    /// The file id this set writes.
    #[must_use]
    pub const fn file(&self) -> &'static str {
        self.file
    }

    /// The key inside that file.
    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.key
    }

    /// What value the engine writes.
    #[must_use]
    pub const fn slot(&self) -> &Slot {
        &self.slot
    }
}

/// Compiled agent metadata. The engine reads this. The API does not plan edits.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentDescription {
    id: PluginId,
    display_name: &'static str,
    binary: &'static str,
    files: &'static [AgentFile],
    sets: &'static [AgentSet],
    model_key: Option<&'static str>,
    model_file: Option<&'static str>,
    model_prefix: Option<&'static str>,
    effort_key: Option<&'static str>,
    effort_file: Option<&'static str>,
    visible: Visible,
    route: AgentRoute,
    requires: &'static [crate::id::SurfaceId],
}

impl AgentDescription {
    /// A description. Generated code calls this. It does not use a struct literal.
    ///
    /// # Panics
    ///
    /// Panics when the display name or binary is empty, or a key option is empty.
    #[doc(hidden)]
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn from_static(
        id: PluginId,
        display_name: &'static str,
        binary: &'static str,
        files: &'static [AgentFile],
        sets: &'static [AgentSet],
        model_key: Option<&'static str>,
        effort_key: Option<&'static str>,
        visible: Visible,
        route: AgentRoute,
        requires: &'static [crate::id::SurfaceId],
    ) -> Self {
        assert!(!display_name.is_empty(), "agent display name is empty");
        assert!(
            is_binary_name(binary),
            "agent binary must be one binary name"
        );
        if let Some(key) = model_key {
            assert!(!key.is_empty(), "model key is empty");
        }
        if let Some(key) = effort_key {
            assert!(!key.is_empty(), "effort key is empty");
        }
        Self {
            id,
            display_name,
            binary,
            files,
            sets,
            model_key,
            model_file: None,
            model_prefix: None,
            effort_key,
            effort_file: None,
            visible,
            route,
            requires,
        }
    }

    /// Sets the logical file containing the model key.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_model_file(mut self, file: &'static str) -> Self {
        assert!(!file.is_empty(), "model file is empty");
        self.model_file = Some(file);
        self
    }

    /// Sets the prefix the agent uses for gateway route names.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_model_prefix(mut self, prefix: &'static str) -> Self {
        assert!(!prefix.is_empty(), "model prefix is empty");
        self.model_prefix = Some(prefix);
        self
    }

    /// Sets the logical file containing the effort key.
    #[doc(hidden)]
    #[must_use]
    pub const fn with_effort_file(mut self, file: &'static str) -> Self {
        assert!(!file.is_empty(), "effort file is empty");
        self.effort_file = Some(file);
        self
    }

    /// Plugin id.
    #[must_use]
    pub const fn id(&self) -> &PluginId {
        &self.id
    }

    /// Name the CLI prints.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        self.display_name
    }

    /// Binary the engine looks for on `PATH`.
    #[must_use]
    pub const fn binary(&self) -> &'static str {
        self.binary
    }

    /// Managed files.
    #[must_use]
    pub const fn files(&self) -> &'static [AgentFile] {
        self.files
    }

    /// Key writes.
    #[must_use]
    pub const fn sets(&self) -> &'static [AgentSet] {
        self.sets
    }

    /// Optional model key.
    #[must_use]
    pub const fn model_key(&self) -> Option<&'static str> {
        self.model_key
    }

    /// Logical file containing the model key, when explicitly assigned.
    #[must_use]
    pub const fn model_file(&self) -> Option<&'static str> {
        self.model_file
    }

    /// Prefix the agent uses for gateway route names.
    #[must_use]
    pub const fn model_prefix(&self) -> Option<&'static str> {
        self.model_prefix
    }

    /// Optional effort key.
    #[must_use]
    pub const fn effort_key(&self) -> Option<&'static str> {
        self.effort_key
    }

    /// Logical file containing the effort key, when explicitly assigned.
    #[must_use]
    pub const fn effort_file(&self) -> Option<&'static str> {
        self.effort_file
    }

    /// Which models the file lists.
    #[must_use]
    pub const fn visible(&self) -> Visible {
        self.visible
    }

    /// Whether the agent receives the gateway URL.
    #[must_use]
    pub const fn route(&self) -> AgentRoute {
        self.route
    }

    /// Inbound surfaces this agent needs. The registry checks each id.
    #[must_use]
    pub const fn requires(&self) -> &'static [crate::id::SurfaceId] {
        self.requires
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Account, AuthChoice, BinaryName, ListQuery, MatchNeedle, ModelList, ModelSpec, ProbeQuery,
        QuotaWindow, SignInChallenge, StaticUrl,
    };
    use crate::ListKind;
    use crate::id::{AccountId, PluginId, ProtocolId};

    #[test]
    fn model_metadata_round_trips_and_unknown_values_stay_empty() {
        let model = ModelSpec::new("model", Some("Model".to_owned()))
            .expect("model")
            .with_context(32_000)
            .with_max_output(4096)
            .with_input_modalities(vec!["text".to_owned(), "image".to_owned()])
            .with_output_modalities(vec!["text".to_owned()])
            .with_image_input(Some(true))
            .with_endpoints(vec!["chat".to_owned()])
            .with_draws(true)
            .with_films(true)
            .with_decides(true)
            .with_efforts(vec!["low".to_owned(), "high".to_owned()])
            .with_web_search("native")
            .with_service_tiers(vec!["default".to_owned()]);
        assert_eq!(model.context(), 32_000);
        assert_eq!(model.max_output(), 4096);
        assert_eq!(model.input_modalities(), ["text", "image"]);
        assert_eq!(model.output_modalities(), ["text"]);
        assert_eq!(model.image_input(), Some(true));
        assert_eq!(model.endpoints(), ["chat"]);
        assert!(model.draws());
        assert!(model.films());
        assert!(model.decides());
        assert_eq!(model.efforts(), ["low", "high"]);
        assert_eq!(model.web_search(), Some("native"));
        assert_eq!(model.service_tiers(), ["default"]);

        let unknown = ModelSpec::new("model", None)
            .expect("model")
            .with_context(0)
            .with_max_output(0)
            .with_web_search("unsupported");
        assert_eq!(unknown.context(), 0);
        assert_eq!(unknown.max_output(), 0);
        assert_eq!(unknown.image_input(), None);
        assert_eq!(unknown.web_search(), None);
    }

    #[test]
    fn list_result_labels_and_learned_auth_round_trip() {
        let list = ModelList::new(Vec::new(), None)
            .with_kind(ListKind::Deployments)
            .with_base_model_fallback(true)
            .with_v1_fallback(true)
            .with_auth_choice(AuthChoice::XApiKey);
        assert_eq!(list.kind(), ListKind::Deployments);
        assert!(list.base_model_fallback());
        assert!(list.v1_fallback());
        assert_eq!(list.auth_choice(), Some(AuthChoice::XApiKey));

        let query = ListQuery::new().with_auth_choice(AuthChoice::Bearer);
        assert_eq!(query.auth_choice(), Some(AuthChoice::Bearer));
    }

    #[test]
    fn probe_carries_learned_auth_and_redacts_extra_body() {
        let query = ProbeQuery::new(ProtocolId::ANTHROPIC_MESSAGES, "claude-test")
            .expect("query")
            .with_auth_choice(AuthChoice::XApiKey)
            .with_extra_body(bytes::Bytes::from_static(br#"{"key":"SECRET-BODY"}"#));
        assert_eq!(query.auth_choice(), Some(AuthChoice::XApiKey));
        assert_eq!(
            query.extra_body().map(bytes::Bytes::as_ref),
            Some(&br#"{"key":"SECRET-BODY"}"#[..])
        );
        let debug = format!("{query:?}");
        assert!(!debug.contains("SECRET-BODY"));
        assert!(debug.contains("<opaque>"));
    }

    #[test]
    fn account_label_and_quota_limit_are_checked() {
        assert!(
            Account::from_stored(
                PluginId::from_static("example-agent"),
                AccountId::from_static("work"),
                ""
            )
            .is_err()
        );
        assert!(QuotaWindow::new("five-hour", 3, Some(2), None).is_err());
        assert!(ModelSpec::new("", None).is_err());
    }

    #[test]
    fn probe_request_is_one_short_user_message() {
        let query = ProbeQuery::new(ProtocolId::OPENAI_CHAT, "gpt-test").expect("query");
        let request = query.minimal_request();
        assert_eq!(request.model, "gpt-test");
        assert!(!request.stream);
        assert_eq!(request.messages.len(), 1);
    }

    #[test]
    #[should_panic(expected = "model match needle must be lowercase")]
    fn model_match_needle_rejects_uppercase() {
        let _ = MatchNeedle::new("Example-");
    }

    #[test]
    fn sign_in_urls_and_bridge_names_reject_shell_text() {
        assert!(SignInChallenge::open_url("javascript:alert(1)").is_err());
        assert!(SignInChallenge::device_code("", "https://example.com").is_err());
        let _url = StaticUrl::new("https://auth.example.com/device");
        let _binary = BinaryName::new("example-agent");
    }
}
