//! Resolves a pin, an id, or a user to one account of a provider.
//!
//! A later routing-group member can call [`resolve_one`] with the account
//! fragment. This module does not parse a group member's model syntax.
//! The request header that carries a pin is [`ACCOUNT_HEADER`].

use crate::Hop;

/// Canonical request header that pins one account or key.
pub const ACCOUNT_HEADER: &str = "x-hofund-account";

/// Header name written in refusal text.
pub const ACCOUNT_HEADER_NAME: &str = "X-Hofund-Account";

/// Whether a listed row is a signed-in account or a provider key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountKind {
    /// A signed-in account.
    Account,
    /// A provider API key, named by its fingerprint.
    Key,
}

impl AccountKind {
    /// Wire name: `account` or `key`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Key => "key",
        }
    }
}

/// One account or key a caller can list or pin.
///
/// `id` is a login id or a key fingerprint. It is never a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountFace {
    /// Provider or subscription id.
    pub provider: String,
    /// Display name of the provider. Empty when unknown.
    pub provider_name: String,
    /// Stable id: a login id, or a key fingerprint.
    pub id: String,
    /// Account or key.
    pub kind: AccountKind,
    /// Name shown for the row. Empty when the row has none.
    pub label: String,
    /// Email or login of a signed-in account. Empty for a key.
    pub user: String,
    /// Plan name when one is known. Empty when it is not.
    pub plan: String,
    /// Whether requests may be sent to it.
    pub on: bool,
    /// Models the plan lists.
    ///
    /// `None` means the plan does not restrict models. `Some` means only
    /// those models, compared without regard to case.
    pub models: Option<Vec<String>>,
    /// Router member id when this row is a live hop. Empty when it is not.
    pub member_id: String,
}

impl AccountFace {
    /// A signed-in account.
    #[must_use]
    pub fn account(
        provider: impl Into<String>,
        id: impl Into<String>,
        user: impl Into<String>,
        plan: impl Into<String>,
        on: bool,
    ) -> Self {
        let id = id.into();
        let user = user.into();
        Self {
            provider: provider.into(),
            provider_name: String::new(),
            id,
            kind: AccountKind::Account,
            label: user.clone(),
            user,
            plan: plan.into(),
            on,
            models: None,
            member_id: String::new(),
        }
    }

    /// A provider key named by its fingerprint.
    #[must_use]
    pub fn key(
        provider: impl Into<String>,
        id: impl Into<String>,
        label: impl Into<String>,
        plan: impl Into<String>,
        on: bool,
    ) -> Self {
        Self {
            provider: provider.into(),
            provider_name: String::new(),
            id: id.into(),
            kind: AccountKind::Key,
            label: label.into(),
            user: String::new(),
            plan: plan.into(),
            on,
            models: None,
            member_id: String::new(),
        }
    }

    /// Sets the provider's display name.
    #[must_use]
    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = name.into();
        self
    }

    /// Sets the label without changing [`Self::user`].
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Restricts the plan to `models`.
    #[must_use]
    pub fn with_models(mut self, models: Vec<String>) -> Self {
        self.models = Some(models);
        self
    }

    /// Records the router member id.
    #[must_use]
    pub fn with_member(mut self, member_id: impl Into<String>) -> Self {
        self.member_id = member_id.into();
        self
    }

    /// `provider/id`, the pin a request header accepts.
    #[must_use]
    pub fn pin(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Name used in a refusal: the user, else the label, else the id.
    #[must_use]
    pub fn who(&self) -> &str {
        if !self.user.is_empty() {
            &self.user
        } else if !self.label.is_empty() {
            &self.label
        } else {
            &self.id
        }
    }
}

/// Header text after trimming and percent-decoding.
///
/// A value that is not ASCII is percent-encoded on the wire. A value that
/// is not a valid encoding is kept as written.
#[must_use]
pub fn pinned_name(raw: &str) -> String {
    let raw = raw.trim();
    if !raw.contains('%') {
        return raw.to_owned();
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let encoded = bytes[index] == b'%' && index + 2 < bytes.len();
        if encoded
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push((high << 4) | low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out)
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| raw.to_owned())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Accounts and keys `raw` names.
///
/// `raw` may be `provider/id`, a bare id, or a user. Matching ignores case.
/// A pin that includes a provider only searches that provider. The order is
/// the order of `accounts`.
#[must_use]
pub fn resolve<'a>(raw: &str, accounts: &'a [AccountFace]) -> Vec<&'a AccountFace> {
    let want = pinned_name(raw);
    if want.is_empty() {
        return Vec::new();
    }
    let (provider, id) = match want.split_once('/') {
        Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
            (Some(provider), id.to_owned())
        }
        _ => (None, want),
    };
    accounts
        .iter()
        .filter(|face| provider.is_none_or(|provider| same(face.provider.as_str(), provider)))
        .filter(|face| names(face, &id) || face.pin().eq_ignore_ascii_case(raw.trim()))
        .collect()
}

/// The account of `provider` that `raw` names.
///
/// When several rows of that provider match, the first in `accounts` is
/// returned. A pin for a different provider does not match.
#[must_use]
pub fn resolve_one<'a>(
    raw: &str,
    provider: &str,
    accounts: &'a [AccountFace],
) -> Option<&'a AccountFace> {
    resolve(raw, accounts)
        .into_iter()
        .find(|face| same(face.provider.as_str(), provider))
}

fn same(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn names(face: &AccountFace, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    same(&face.id, token)
        || (!face.user.is_empty() && same(&face.user, token))
        || (!face.label.is_empty() && same(&face.label, token))
        || (!face.member_id.is_empty() && face.member_id == token)
}

/// Why a pin cannot be asked, or the hops that can.
pub enum PinChoice {
    /// These hops, in route order, and no others.
    Ask(Vec<Hop>),
    /// Answer this status and message. No hop is asked.
    Refuse {
        /// HTTP status.
        status: u16,
        /// Message for the caller. It names no secret.
        message: String,
    },
}

/// Facts the router knows about one hop while a pin is applied.
pub struct PinHop<'a> {
    /// The hop.
    pub hop: &'a Hop,
    /// Whether the caller's account scope allows it.
    pub allowed: bool,
    /// Whether a spend cap has removed it.
    pub capped: bool,
    /// Rest cause when the hop is cooling. Empty when it is ready.
    pub resting: Option<String>,
}

/// Narrows `planned` to the account `raw` names.
///
/// Ready matches are returned in their existing order. A resting or capped
/// match is 429, a match the gateway key may not use is 403, a match whose
/// plan does not list `model` is 400, and any other miss is 404. No other
/// account is substituted.
#[must_use]
pub fn select(
    raw: &str,
    model: &str,
    planned: &[PinHop<'_>],
    catalog: &[AccountFace],
) -> PinChoice {
    let faces = merged(planned, catalog);
    let matches = resolve(raw, &faces);
    if matches.is_empty() {
        return PinChoice::Refuse {
            status: 404,
            message: missing(raw, &faces),
        };
    }
    let mut ready = Vec::new();
    let mut rests = Vec::new();
    let mut capped = Vec::new();
    let mut held = None;
    let mut plan = None;
    for face in matches {
        let hop = planned.iter().find(|row| hop_is(row.hop, face));
        if face_excludes(face, model, hop.map(|row| row.hop)) {
            plan = Some(face.who().to_owned());
            continue;
        }
        let Some(row) = hop else {
            continue;
        };
        if !row.allowed {
            held = Some(face.who().to_owned());
            continue;
        }
        if row.capped {
            capped.push(face.who().to_owned());
            continue;
        }
        if let Some(why) = &row.resting {
            rests.push(format!("{} rests ({why})", face.who()));
            continue;
        }
        ready.push(row.hop.clone());
    }
    if !ready.is_empty() {
        return PinChoice::Ask(ready);
    }
    if !rests.is_empty() {
        return PinChoice::Refuse {
            status: 429,
            message: format!(
                "{ACCOUNT_HEADER_NAME}: {}; no other account is tried in its place",
                rests.join("; ")
            ),
        };
    }
    if !capped.is_empty() {
        return PinChoice::Refuse {
            status: 429,
            message: format!(
                "{ACCOUNT_HEADER_NAME}: {} is at its spend cap; no other account is tried in its place",
                capped.join(", ")
            ),
        };
    }
    if let Some(who) = held {
        return PinChoice::Refuse {
            status: 403,
            message: format!("{ACCOUNT_HEADER_NAME}: the gateway key may not use {who}'s account"),
        };
    }
    if let Some(who) = plan {
        return PinChoice::Refuse {
            status: 400,
            message: format!("{who}'s plan doesn't list {model}"),
        };
    }
    PinChoice::Refuse {
        status: 404,
        message: missing(raw, &faces),
    }
}

fn face_excludes(face: &AccountFace, model: &str, hop: Option<&Hop>) -> bool {
    let Some(models) = &face.models else {
        return false;
    };
    let bare = model
        .rsplit_once('/')
        .map(|(_, bare)| bare)
        .unwrap_or(model);
    !models.iter().any(|listed| {
        same(listed, model)
            || same(listed, bare)
            || hop.is_some_and(|hop| {
                hop.wire_model
                    .as_deref()
                    .is_some_and(|wire| same(listed, wire))
            })
    })
}

fn merged(planned: &[PinHop<'_>], catalog: &[AccountFace]) -> Vec<AccountFace> {
    let mut providers = Vec::new();
    for row in planned {
        let provider = row.hop.member.scope_provider().to_ascii_lowercase();
        if !providers.iter().any(|known: &String| known == &provider) {
            providers.push(provider);
        }
    }
    let mut faces: Vec<AccountFace> = catalog
        .iter()
        .filter(|face| {
            providers
                .iter()
                .any(|provider| provider == &face.provider.to_ascii_lowercase())
        })
        .cloned()
        .collect();
    for row in planned {
        let derived = derived_face(row.hop);
        if let Some(existing) = faces.iter_mut().find(|face| identity_eq(face, &derived)) {
            if existing.member_id.is_empty() {
                existing.member_id = derived.member_id;
            }
        } else {
            faces.push(derived);
        }
    }
    faces
}

fn derived_face(hop: &Hop) -> AccountFace {
    let provider = hop.member.scope_provider().to_owned();
    let member_id = hop.member.id().to_string();
    if let Some(account) = hop.member.id().account() {
        return AccountFace::account(provider, account.to_string(), account.to_string(), "", true)
            .with_member(member_id);
    }
    let key = hop.member.provider_key_id();
    let id = if key.is_empty() {
        member_id.clone()
    } else {
        key.to_owned()
    };
    AccountFace::key(provider, id, "", "", true).with_member(member_id)
}

fn identity_eq(left: &AccountFace, right: &AccountFace) -> bool {
    same(&left.provider, &right.provider)
        && (same(&left.id, &right.id)
            || (!left.member_id.is_empty() && left.member_id == right.member_id))
}

fn hop_is(hop: &Hop, face: &AccountFace) -> bool {
    let derived = derived_face(hop);
    identity_eq(face, &derived)
}

fn missing(raw: &str, faces: &[AccountFace]) -> String {
    let want = pinned_name(raw);
    let mut have = Vec::new();
    for face in faces {
        if !face.on {
            continue;
        }
        let who = face.who().to_owned();
        if !who.is_empty() && !have.iter().any(|known: &String| known == &who) {
            have.push(who);
        }
    }
    let mut message = format!("{ACCOUNT_HEADER_NAME}: no account \"{want}\" serves this model");
    if !have.is_empty() {
        message.push_str("; its accounts are ");
        message.push_str(&have.join(", "));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> AccountFace {
        AccountFace::key("keyed", "aaa111", "work", "api", true).with_provider_name("Keyed")
    }

    fn spare() -> AccountFace {
        AccountFace::account("acct", "login-2", "spare@example.com", "plus", true)
    }

    #[test]
    fn d243_resolve_matches_pin_id_and_user() {
        let accounts = vec![work(), spare()];
        assert_eq!(resolve("keyed/aaa111", &accounts)[0].label, "work");
        assert_eq!(resolve("AAA111", &accounts)[0].id, "aaa111");
        assert_eq!(resolve("spare@example.com", &accounts)[0].id, "login-2");
        assert_eq!(
            resolve("acct/login-2", &accounts)[0].user,
            "spare@example.com"
        );
        assert!(resolve("other/aaa111", &accounts).is_empty());
        assert_eq!(
            resolve_one("aaa111", "keyed", &accounts).map(|face| face.kind),
            Some(AccountKind::Key)
        );
        assert!(resolve_one("keyed/aaa111", "acct", &accounts).is_none());
        assert_eq!(pinned_name("spare%40example.com"), "spare@example.com");
        assert_eq!(resolve("spare%40example.com", &accounts)[0].id, "login-2");
    }
}
