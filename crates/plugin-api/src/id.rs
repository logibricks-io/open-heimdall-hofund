//! Identifiers. Constructors reject values that break the invariant.

use std::fmt;
use std::hash::{Hash, Hasher};

/// A string that is not a valid identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
#[error("{reason}")]
pub struct IdError {
    reason: &'static str,
}

impl IdError {
    const fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    /// Why the value was rejected.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        self.reason
    }
}

#[derive(Clone, Debug)]
enum Store {
    Static(&'static str),
    Owned(Box<str>),
}

impl Store {
    fn as_str(&self) -> &str {
        match self {
            Self::Static(value) => value,
            Self::Owned(value) => value,
        }
    }
}

fn eq_store(left: &Store, right: &Store) -> bool {
    left.as_str() == right.as_str()
}

fn cmp_store(left: &Store, right: &Store) -> std::cmp::Ordering {
    left.as_str().cmp(right.as_str())
}

fn hash_store<H: Hasher>(store: &Store, state: &mut H) {
    store.as_str().hash(state);
}

/// Kebab-case ASCII: lowercase letters, digits, and single hyphens.
///
/// Empty strings, leading or trailing hyphens, and uppercase are rejected.
const fn is_kebab(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    let mut prev_hyphen = true;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'-' {
            if prev_hyphen {
                return false;
            }
            prev_hyphen = true;
        } else if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            prev_hyphen = false;
        } else {
            return false;
        }
        index += 1;
    }
    !prev_hyphen
}

/// Non-empty ASCII lowercase letters and digits. No hyphen.
const fn is_tier(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit()) {
            return false;
        }
        index += 1;
    }
    true
}

macro_rules! kebab_id {
    ($(#[$meta:meta])* $name:ident, $label:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug)]
        #[non_exhaustive]
        pub struct $name {
            value: Store,
        }

        impl $name {
            /// Builds an id from a static string.
            ///
            /// # Panics
            ///
            /// Panics when `value` is not kebab-case ASCII. Generated statics
            /// use this so a bad id fails the build.
            #[must_use]
            pub const fn from_static(value: &'static str) -> Self {
                assert!(is_kebab(value.as_bytes()), concat!($label, " must be kebab-case ASCII"));
                Self {
                    value: Store::Static(value),
                }
            }

            /// The identifier text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.value.as_str()
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                if is_kebab(value.as_bytes()) {
                    Ok(Self {
                        value: Store::Owned(value.to_owned().into_boxed_str()),
                    })
                } else {
                    Err(IdError::new(concat!($label, " must be kebab-case ASCII")))
                }
            }
        }

        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                eq_store(&self.value, &other.value)
            }
        }

        impl Eq for $name {}

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                cmp_store(&self.value, &other.value)
            }
        }

        impl Hash for $name {
            fn hash<H: Hasher>(&self, state: &mut H) {
                hash_store(&self.value, state);
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

kebab_id!(
    /// A plugin directory id, such as `example-agent` or `acme-cloud`.
    ///
    /// The ids `api`, `sdk`, `host`, and `registry` are reserved and are not
    /// plugin ids. Constructors do not know that list: the registry rejects
    /// them. This type only checks the shape.
    PluginId,
    "plugin id"
);
kebab_id!(
    /// A catalog preset id, such as `acme` or `acme-cloud`.
    PresetId,
    "preset id"
);
kebab_id!(
    /// An account id inside one plugin.
    ///
    /// The shape is kebab-case so ids are stable map keys. A vendor token that
    /// is not kebab-case has to be mapped by that plugin before it is stored.
    AccountId,
    "account id"
);
kebab_id!(
    /// An inbound surface id, such as `example-surface`.
    SurfaceId,
    "surface id"
);

/// The kind of a plugin in the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum PluginKind {
    /// An upstream provider implementation.
    Provider,
    /// A subscription implementation.
    Subscription,
    /// A coding-agent integration.
    Agent,
    /// An inbound HTTP surface.
    Inbound,
}

impl PluginKind {
    /// The registry kind string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Subscription => "subscription",
            Self::Agent => "agent",
            Self::Inbound => "inbound",
        }
    }
}

impl TryFrom<&str> for PluginKind {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "provider" => Ok(Self::Provider),
            "subscription" => Ok(Self::Subscription),
            "agent" => Ok(Self::Agent),
            "inbound" => Ok(Self::Inbound),
            _ => Err(IdError::new("unknown plugin kind")),
        }
    }
}

impl fmt::Display for PluginKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The kind-qualified identity used for per-plugin host storage.
///
/// User-facing configuration and CLI commands continue to use [`PluginId`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub struct PluginKey {
    kind: PluginKind,
    id: PluginId,
}

impl PluginKey {
    /// Builds a kind-qualified plugin identity.
    #[must_use]
    pub const fn new(kind: PluginKind, id: PluginId) -> Self {
        Self { kind, id }
    }

    /// The plugin kind.
    #[must_use]
    pub const fn kind(&self) -> PluginKind {
        self.kind
    }

    /// The user-facing plugin id within that kind.
    #[must_use]
    pub const fn id(&self) -> &PluginId {
        &self.id
    }
}

impl fmt::Display for PluginKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind, self.id)
    }
}

/// The key half of a preset [`MemberId`]. Not an account id.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MemberKey {
    value: Store,
}

impl MemberKey {
    /// Builds a key from a static string.
    ///
    /// # Panics
    ///
    /// Panics when `value` is not kebab-case ASCII.
    #[must_use]
    pub const fn from_static(value: &'static str) -> Self {
        assert!(
            is_kebab(value.as_bytes()),
            "member key must be kebab-case ASCII"
        );
        Self {
            value: Store::Static(value),
        }
    }

    /// The key text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.value.as_str()
    }
}

impl TryFrom<&str> for MemberKey {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if is_kebab(value.as_bytes()) {
            Ok(Self {
                value: Store::Owned(value.to_owned().into_boxed_str()),
            })
        } else {
            Err(IdError::new("member key must be kebab-case ASCII"))
        }
    }
}

impl PartialEq for MemberKey {
    fn eq(&self, other: &Self) -> bool {
        eq_store(&self.value, &other.value)
    }
}

impl Eq for MemberKey {}

impl Hash for MemberKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        hash_store(&self.value, state);
    }
}

impl fmt::Display for MemberKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One of the five codec protocol ids.
///
/// Config stores the id as a string. The daemon converts it with [`TryFrom`].
/// An unknown string is an error here, the same class of failure as an unknown
/// preset id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProtocolId {
    /// `openai-chat`.
    OpenAiChat,
    /// `openai-responses`.
    OpenAiResponses,
    /// `anthropic-messages`.
    AnthropicMessages,
    /// `generative-language`.
    GenerativeLanguage,
    /// `bedrock-converse`.
    BedrockConverse,
}

impl ProtocolId {
    /// `openai-chat`.
    pub const OPENAI_CHAT: Self = Self::OpenAiChat;
    /// `openai-responses`.
    pub const OPENAI_RESPONSES: Self = Self::OpenAiResponses;
    /// `anthropic-messages`.
    pub const ANTHROPIC_MESSAGES: Self = Self::AnthropicMessages;
    /// `generative-language`.
    pub const GENERATIVE_LANGUAGE: Self = Self::GenerativeLanguage;
    /// `bedrock-converse`.
    pub const BEDROCK_CONVERSE: Self = Self::BedrockConverse;

    /// Every codec id, in a stable order.
    pub const ALL: &'static [Self] = &[
        Self::OPENAI_CHAT,
        Self::OPENAI_RESPONSES,
        Self::ANTHROPIC_MESSAGES,
        Self::GENERATIVE_LANGUAGE,
        Self::BEDROCK_CONVERSE,
    ];

    /// The codec id string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChat => "openai-chat",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GenerativeLanguage => "generative-language",
            Self::BedrockConverse => "bedrock-converse",
        }
    }

    /// Builds a protocol id from a static codec id.
    ///
    /// # Panics
    ///
    /// Panics when `value` is not one of the five codec ids.
    #[must_use]
    pub const fn from_static(value: &'static str) -> Self {
        match value.as_bytes() {
            b"openai-chat" => Self::OpenAiChat,
            b"openai-responses" => Self::OpenAiResponses,
            b"anthropic-messages" => Self::AnthropicMessages,
            b"generative-language" => Self::GenerativeLanguage,
            b"bedrock-converse" => Self::BedrockConverse,
            _ => panic!("unknown protocol id"),
        }
    }
}

impl TryFrom<&str> for ProtocolId {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "openai-chat" => Ok(Self::OpenAiChat),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic-messages" => Ok(Self::AnthropicMessages),
            "generative-language" => Ok(Self::GenerativeLanguage),
            "bedrock-converse" => Ok(Self::BedrockConverse),
            _ => Err(IdError::new("unknown protocol id")),
        }
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A tier name such as `opus` or `fable`.
///
/// The alphabet is `[a-z0-9]`. It is not kebab-case: tier tokens already in
/// use are single words. A model-specific skip rule is data on the slot, not a
/// special case in this type.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TierId {
    value: Store,
}

impl TierId {
    /// Builds a tier from a static name.
    ///
    /// # Panics
    ///
    /// Panics when `value` is empty or contains anything outside `[a-z0-9]`.
    #[must_use]
    pub const fn from_static(value: &'static str) -> Self {
        assert!(
            is_tier(value.as_bytes()),
            "tier id must be non-empty [a-z0-9]"
        );
        Self {
            value: Store::Static(value),
        }
    }

    /// The tier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.value.as_str()
    }
}

impl TryFrom<&str> for TierId {
    type Error = IdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if is_tier(value.as_bytes()) {
            Ok(Self {
                value: Store::Owned(value.to_owned().into_boxed_str()),
            })
        } else {
            Err(IdError::new("tier id must be non-empty [a-z0-9]"))
        }
    }
}

impl PartialEq for TierId {
    fn eq(&self, other: &Self) -> bool {
        eq_store(&self.value, &other.value)
    }
}

impl Eq for TierId {}

impl fmt::Display for TierId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MemberKind {
    Preset {
        preset: PresetId,
        key: MemberKey,
    },
    Account {
        subscription: PluginId,
        account: AccountId,
    },
}

/// A router member id that stays the same across snapshots.
///
/// Build one with [`MemberId::from_preset`] or [`MemberId::from_parts`].
/// There is no [`TryFrom<&str>`](TryFrom): a member id is not a string plugins
/// concatenate.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct MemberId {
    kind: MemberKind,
}

impl MemberId {
    /// A member for one preset and a stable key, such as the configured row id.
    #[must_use]
    pub fn from_preset(preset: PresetId, key: MemberKey) -> Self {
        Self {
            kind: MemberKind::Preset { preset, key },
        }
    }

    /// A member for one subscription account.
    ///
    /// The pair is the id. It does not change when the snapshot is rebuilt.
    #[must_use]
    pub fn from_parts(subscription: PluginId, account: AccountId) -> Self {
        Self {
            kind: MemberKind::Account {
                subscription,
                account,
            },
        }
    }

    /// The preset, when this member is a preset row.
    #[must_use]
    pub fn preset(&self) -> Option<&PresetId> {
        match &self.kind {
            MemberKind::Preset { preset, .. } => Some(preset),
            MemberKind::Account { .. } => None,
        }
    }

    /// The preset key, when this member is a preset row.
    #[must_use]
    pub fn member_key(&self) -> Option<&MemberKey> {
        match &self.kind {
            MemberKind::Preset { key, .. } => Some(key),
            MemberKind::Account { .. } => None,
        }
    }

    /// The subscription plugin, when this member is an account.
    #[must_use]
    pub fn subscription(&self) -> Option<&PluginId> {
        match &self.kind {
            MemberKind::Account { subscription, .. } => Some(subscription),
            MemberKind::Preset { .. } => None,
        }
    }

    /// The account id, when this member is an account.
    #[must_use]
    pub fn account(&self) -> Option<&AccountId> {
        match &self.kind {
            MemberKind::Account { account, .. } => Some(account),
            MemberKind::Preset { .. } => None,
        }
    }
}

impl fmt::Display for MemberId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            MemberKind::Preset { preset, key } => {
                write!(formatter, "preset:{preset}:{key}")
            }
            MemberKind::Account {
                subscription,
                account,
            } => write!(formatter, "account:{subscription}:{account}"),
        }
    }
}

const _: PluginId = PluginId::from_static("example-agent");
const _: PresetId = PresetId::from_static("acme-cloud");
const _: ProtocolId = ProtocolId::from_static("openai-chat");
const _: TierId = TierId::from_static("fast");
const _: SurfaceId = SurfaceId::from_static("example-surface");

#[cfg(test)]
mod tests {
    use super::{
        AccountId, IdError, MemberId, MemberKey, PluginId, PluginKey, PluginKind, PresetId,
        ProtocolId, TierId,
    };

    #[test]
    fn plugin_keys_carry_kind_without_changing_the_id() {
        let key = PluginKey::new(PluginKind::Subscription, PluginId::from_static("shared"));
        assert_eq!(key.kind(), PluginKind::Subscription);
        assert_eq!(key.id().as_str(), "shared");
        assert_eq!(key.to_string(), "subscription:shared");
        assert_eq!(PluginKind::try_from("provider"), Ok(PluginKind::Provider));
        assert!(PluginKind::try_from("unknown").is_err());
    }

    #[test]
    fn kebab_ids_reject_shape_errors() {
        for bad in ["", "Acme", "a-", "-a", "a--b", "a_b", "a b", "API"] {
            assert!(PluginId::try_from(bad).is_err(), "{bad}");
            assert!(PresetId::try_from(bad).is_err(), "{bad}");
            assert!(AccountId::try_from(bad).is_err(), "{bad}");
        }
        let id = PluginId::try_from("acme-cloud").expect("id");
        assert_eq!(id.as_str(), "acme-cloud");
        assert_eq!(id, PluginId::from_static("acme-cloud"));
    }

    #[test]
    fn protocol_ids_are_the_five_codec_ids() {
        assert_eq!(ProtocolId::ALL.len(), 5);
        for id in ProtocolId::ALL {
            assert_eq!(ProtocolId::try_from(id.as_str()).expect("codec"), *id);
        }
        assert_eq!(
            ProtocolId::try_from("chat").unwrap_err(),
            IdError::new("unknown protocol id")
        );
    }

    #[test]
    fn tier_ids_are_not_kebab_case() {
        assert!(TierId::try_from("example-tier").is_err());
        assert!(TierId::try_from("").is_err());
        assert!(TierId::try_from("Opus").is_err());
        assert_eq!(TierId::try_from("fable").expect("tier").as_str(), "fable");
    }

    #[test]
    fn member_ids_compare_by_parts() {
        let preset = MemberId::from_preset(
            PresetId::from_static("acme"),
            MemberKey::from_static("primary"),
        );
        let account = MemberId::from_parts(
            PluginId::from_static("example-agent"),
            AccountId::from_static("work"),
        );
        assert_ne!(preset, account);
        assert_eq!(preset.preset().expect("preset").as_str(), "acme");
        assert_eq!(preset.member_key().expect("key").as_str(), "primary");
        assert!(preset.account().is_none());
        assert_eq!(
            account.subscription().expect("plugin").as_str(),
            "example-agent"
        );
        assert_eq!(account.account().expect("account").as_str(), "work");
        assert_eq!(account.to_string(), "account:example-agent:work");
        assert_eq!(
            account,
            MemberId::from_parts(
                PluginId::try_from("example-agent").expect("id"),
                AccountId::try_from("work").expect("id")
            )
        );
    }
}
