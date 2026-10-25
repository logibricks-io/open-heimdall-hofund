//! Data the traits pass around. None of these types perform I/O.

use std::time::{Duration, SystemTime};

use hofund_ir::{ContentPart, Message, Request, Role};

use secrecy::SecretString;

use crate::error::PluginError;
use crate::id::{AccountId, PluginId, ProtocolId, TierId};

/// One account in the store. The secret stays in the store, not on this value.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Account {
    plugin: PluginId,
    id: AccountId,
    label: String,
}

impl Account {
    /// An account for `plugin`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when `label` is empty.
    pub fn new(
        plugin: PluginId,
        id: AccountId,
        label: impl Into<String>,
    ) -> Result<Self, PluginError> {
        let label = label.into();
        if label.is_empty() {
            return Err(PluginError::config("account label is empty"));
        }
        Ok(Self { plugin, id, label })
    }

    /// The plugin that owns the account.
    #[must_use]
    pub fn plugin(&self) -> &PluginId {
        &self.plugin
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
    #[must_use]
    pub fn new(plugin: PluginId) -> Self {
        Self {
            plugin,
            retry_after: None,
            remaining: None,
            window: None,
        }
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

/// One model a list call returned.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelSpec {
    id: String,
    display_name: Option<String>,
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
        Ok(Self { id, display_name })
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
}

/// A page of models.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelList {
    models: Vec<ModelSpec>,
    next: Option<String>,
}

impl ModelList {
    /// A page. `next` is the token for the following page.
    #[must_use]
    pub fn new(models: Vec<ModelSpec>, next: Option<String>) -> Self {
        Self { models, next }
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
}

/// A page request for [`crate::UpstreamProvider::list_models`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListQuery {
    limit: Option<u32>,
    page: Option<String>,
}

impl ListQuery {
    /// The first page, with no limit.
    #[must_use]
    pub fn new() -> Self {
        Self {
            limit: None,
            page: None,
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
}

impl Default for ListQuery {
    fn default() -> Self {
        Self::new()
    }
}

/// What `probe` sends. One minimal request for one protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbeQuery {
    protocol: ProtocolId,
    model: String,
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
        Ok(Self { protocol, model })
    }

    /// The protocol under test.
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        self.protocol
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
    /// A single binary name such as `claude`.
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
        /// The binary name, such as `claude`.
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
    /// A spec. `display_name` must be non-empty.
    ///
    /// # Panics
    ///
    /// Panics when `display_name` is empty.
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

/// A non-empty substring a tier slot matches against the model id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MatchNeedle {
    value: &'static str,
}

impl MatchNeedle {
    /// A non-empty needle such as `claude-fable-`.
    ///
    /// # Panics
    ///
    /// Panics when `value` is empty.
    #[must_use]
    pub const fn new(value: &'static str) -> Self {
        assert!(!value.is_empty(), "model match needle is empty");
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
    Origin,
    /// The gateway origin plus `/v1`.
    V1,
    /// The bearer token.
    Token,
    /// A fixed string.
    Literal(&'static str),
    /// A tier environment variable. The needle skips the write when it does not match.
    ModelTier {
        /// Tier name, such as `fable`.
        tier: TierId,
        /// When set, the write runs only if the model id contains this text.
        when_model_matches: Option<MatchNeedle>,
    },
    /// One models object per visible route.
    CatalogModels,
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
}

/// One file an agent description manages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentFile {
    id: &'static str,
    kind: FileKind,
    linux: &'static str,
    macos: &'static str,
    windows: &'static str,
    env_override: Option<&'static str>,
}

impl AgentFile {
    /// A managed file. Paths are the template strings, not expanded paths.
    ///
    /// # Panics
    ///
    /// Panics when `id` is empty or any path is empty.
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
        }
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

    /// Environment variable that replaces the path, when set.
    #[must_use]
    pub const fn env_override(&self) -> Option<&'static str> {
        self.env_override
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
    /// A write of `slot` into `key` of `file`.
    ///
    /// # Panics
    ///
    /// Panics when `file` or `key` is empty.
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
    effort_key: Option<&'static str>,
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
            effort_key,
            visible,
            route,
            requires,
        }
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

    /// Optional effort key.
    #[must_use]
    pub const fn effort_key(&self) -> Option<&'static str> {
        self.effort_key
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
        Account, BinaryName, ModelSpec, ProbeQuery, QuotaWindow, SignInChallenge, StaticUrl,
    };
    use crate::id::{AccountId, PluginId, ProtocolId};

    #[test]
    fn account_label_and_quota_limit_are_checked() {
        assert!(
            Account::new(
                PluginId::from_static("codex"),
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
    fn sign_in_urls_and_bridge_names_reject_shell_text() {
        assert!(SignInChallenge::open_url("javascript:alert(1)").is_err());
        assert!(SignInChallenge::device_code("", "https://example.com").is_err());
        let _url = StaticUrl::new("https://auth.example.com/device");
        let _binary = BinaryName::new("claude");
    }
}
