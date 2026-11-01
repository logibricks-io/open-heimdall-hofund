//! Short-lived gateway keys, issuer folding, and expiry cleanup.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::*;

/// Shortest lifetime accepted for an issued key.
pub const MIN_TTL: Duration = Duration::from_secs(60);
/// Longest lifetime accepted for an issued key: thirty days.
pub const MAX_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// How long an expired key remains so its caller gets an expiry message.
pub const EXPIRED_KEPT: Duration = Duration::from_secs(60 * 60);

/// Input for issuing a short-lived key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IssueRequest {
    /// Display name; blank selects a timestamped name.
    pub name: String,
    /// Requested lifetime, from [`MIN_TTL`] through [`MAX_TTL`].
    pub ttl: Duration,
    /// Model patterns; blank inherits the issuer's effective scope.
    pub models: Vec<String>,
    /// Account patterns; blank inherits the issuer's effective scope.
    pub accounts: Vec<String>,
    /// Known canonical accounts and optional tab-separated aliases.
    pub known_accounts: Vec<String>,
    /// Whole-lifetime token cap; zero means no token cap.
    pub tokens: u64,
    /// Whole-lifetime cost cap; zero means no cost cap.
    pub cost: f64,
    /// Whether the issued key is read-only.
    pub read_only: bool,
    /// Issuer key id; blank means the local administrator.
    pub issuer: String,
}

/// A newly issued key. Its secret is returned only here, never by a listing.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssuedKey {
    /// Stable key id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Secret to give the client once.
    pub key: String,
    /// RFC 3339 expiry in UTC.
    pub expires: String,
    /// Issuer id, when issued under another key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub issuer: String,
    /// Stored model patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Stored account patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<String>,
    /// Whole-lifetime spending limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<KeyLimit>,
    /// Read-only access flag.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
}

impl std::fmt::Debug for IssuedKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedKey")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("key", &"<redacted>")
            .field("expires", &self.expires)
            .field("issuer", &self.issuer)
            .field("models", &self.models)
            .field("accounts", &self.accounts)
            .field("limit", &self.limit)
            .field("read_only", &self.read_only)
            .finish()
    }
}

/// Detailed authentication result, including a known key's refusal reason.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyAuthentication {
    /// An enabled key and its effective identity.
    Enabled(Identity),
    /// A known key that is expired or whose issuer no longer permits it.
    Refused(String),
    /// No stored key has this secret, or the key is disabled.
    Unknown,
}

impl KeyStore {
    /// Authenticates at the current time, retaining expiry and issuer refusals.
    pub fn check(&self, secret: &str) -> Result<KeyAuthentication, SecretError> {
        self.check_at(secret, unix_now())
    }

    /// Authenticates at `now`, exposed for deterministic clocks and future issuers.
    pub fn check_at(&self, secret: &str, now: i64) -> Result<KeyAuthentication, SecretError> {
        if secret.is_empty() {
            return Ok(KeyAuthentication::Unknown);
        }
        let _guard = self.lock()?;
        let keys = self.load_migrated()?;
        let mut matched = None;
        for (index, key) in keys.iter().enumerate() {
            if !key.secret.is_empty() && constant_time_eq(secret.as_bytes(), key.secret.as_bytes())
            {
                matched = Some(index);
            }
        }
        let Some(index) = matched else {
            return Ok(KeyAuthentication::Unknown);
        };
        let key = &keys[index];
        if key.off || key.removed {
            return Ok(KeyAuthentication::Unknown);
        }
        if expired(key, now) {
            return Ok(KeyAuthentication::Refused(format!(
                "the gateway key {:?} expired at {}: ask for a new one",
                key.name,
                expiry_text(key.expires.expect("expired key has an expiry"))
            )));
        }
        identity_of(key, &keys, now)
            .map(KeyAuthentication::Enabled)
            .or_else(|message| Ok(KeyAuthentication::Refused(message)))
    }

    /// Issues a short-lived key at the current time.
    pub fn issue(&self, request: IssueRequest) -> Result<IssuedKey, SecretError> {
        self.issue_at(request, unix_now())
    }

    /// Issues at `now`. This entry point is reusable by a later token exchange.
    pub fn issue_at(&self, request: IssueRequest, now: i64) -> Result<IssuedKey, SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        keys = pruned(keys, now).0;
        let name = if request.name.trim().is_empty() {
            Local
                .timestamp_opt(now, 0)
                .single()
                .map(|time| format!("Issued {}", time.format("%Y-%m-%d %H:%M")))
                .unwrap_or_else(|| "Issued".to_owned())
        } else {
            clean_name(&request.name)?
        };
        if !(MIN_TTL..=MAX_TTL).contains(&request.ttl) {
            return Err(key_error(
                "An issued key lives from 1m to 30 days: give a ttl such as 1h",
            ));
        }
        let ttl = i64::try_from(request.ttl.as_secs()).unwrap_or(i64::MAX);
        let mut expires = now.saturating_add(ttl);
        let models = clean_models(&request.models)?;
        let mut read_only = request.read_only;
        let mut known = request.known_accounts.clone();
        if known.is_empty() && !request.issuer.is_empty() {
            known = keys
                .iter()
                .find(|key| key.id == request.issuer && !key.removed)
                .map(|key| key.accounts.clone())
                .unwrap_or_default();
        }
        let accounts = clean_issued_accounts(&request.accounts, &known)?;
        if !request.issuer.is_empty() {
            let issuer = keys
                .iter()
                .find(|key| key.id == request.issuer && !key.removed)
                .ok_or_else(|| key_error("The key issuing is no longer an enabled gateway key"))?;
            if issuer.off || issuer.secret.is_empty() || expired(issuer, now) {
                return Err(key_error(
                    "The key issuing is no longer an enabled gateway key",
                ));
            }
            if !issuer.issues || !issuer.issuer.is_empty() {
                return Err(key_error(
                    "This gateway key cannot issue keys: enable it with hofund gateway-key issuer <id> on",
                ));
            }
            if issuer.read_only {
                return Err(key_error("A read-only gateway key cannot issue keys"));
            }
            if let Some(parent_expiry) = issuer.expires {
                expires = expires.min(parent_expiry);
            }
            let held = held_models(&issuer.models, &issuer.accounts);
            for model in &models {
                if !model_allowed(&held, model) {
                    return Err(key_error(format!(
                        "The key issuing may not use {model}, so the key it issues cannot either"
                    )));
                }
            }
            for account in &accounts {
                let Some((provider, reference)) = account.split_once('/') else {
                    return Err(key_error(format!(
                        "A key's account is <provider>/<account> or <provider>/<key id>: {account}"
                    )));
                };
                if !allows_account(&issuer.accounts, provider, reference) {
                    return Err(key_error(format!(
                        "The key issuing may not use the account {account}, so the key it issues cannot either"
                    )));
                }
            }
            read_only |= issuer.read_only;
        }
        if !request.cost.is_finite() || request.cost < 0.0 {
            return Err(key_error("A limit can't be negative"));
        }
        let hours = u32::try_from(
            expires
                .saturating_sub(now)
                .saturating_add(3_599)
                .saturating_div(3_600)
                .max(1),
        )
        .unwrap_or(u32::MAX);
        let limit = KeyLimit {
            period: "hours".to_owned(),
            hours,
            tokens: request.tokens,
            cost: request.cost,
            since: now,
            ..KeyLimit::default()
        }
        .validated()?;
        let key = Key {
            id: random_hex(12)?,
            name,
            secret: generated_secret()?,
            models,
            accounts,
            read_only,
            expires: Some(expires),
            issuer: request.issuer,
            limits: limit.iter().cloned().collect(),
            ..Key::default()
        };
        keys.push(key.clone());
        self.save(&keys)?;
        Ok(IssuedKey {
            id: key.id,
            name: key.name,
            key: key.secret,
            expires: expiry_text(expires),
            issuer: key.issuer,
            models: key.models,
            accounts: key.accounts,
            limit,
            read_only: key.read_only,
        })
    }

    /// Lists expiring keys issued by `issuer`, without secrets.
    pub fn issued_by(&self, issuer: &str) -> Result<Vec<Key>, SecretError> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|key| key.expires.is_some() && key.issuer == issuer)
            .collect())
    }

    /// Revokes an expiring key. An issuer can revoke only its own children;
    /// the local administrator can revoke any issued key.
    pub fn revoke_issued(&self, id: &str, issuer: &str) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let index = keys
            .iter()
            .position(|key| {
                key.id == id
                    && !key.removed
                    && key.expires.is_some()
                    && (issuer.is_empty() || key.issuer == issuer)
            })
            .ok_or_else(|| key_error("Key not found"))?;
        keys.remove(index);
        self.save(&keys)
    }

    /// Removes keys expired for over an hour and children of missing issuers.
    pub fn prune(&self) -> Result<usize, SecretError> {
        self.prune_at(unix_now())
    }

    /// Prunes at `now`, returning the number of records removed.
    pub fn prune_at(&self, now: i64) -> Result<usize, SecretError> {
        let _guard = self.lock()?;
        let keys = self.load_migrated()?;
        let (kept, removed) = pruned(keys, now);
        if removed > 0 {
            self.save(&kept)?;
        }
        Ok(removed)
    }

    /// Enables or disables a key by id or by one exact, unambiguous name.
    pub fn set_enabled_by_name(&self, id_or_name: &str, enabled: bool) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let index = match find(&keys, id_or_name) {
            Ok(index) => index,
            Err(_) => {
                let mut matches = keys
                    .iter()
                    .enumerate()
                    .filter(|(_, key)| !key.removed && key.name == id_or_name);
                let Some((index, _)) = matches.next() else {
                    return Err(key_error("Key not found"));
                };
                if matches.next().is_some() {
                    return Err(key_error(
                        "More than one gateway key has this name; use its id",
                    ));
                }
                index
            }
        };
        keys[index].off = !enabled;
        self.save(&keys)
    }
}

/// Parses a duration such as `30m`, `1h`, or `1h30m`.
pub fn parse_ttl(input: &str) -> Result<Duration, SecretError> {
    let input = input.trim();
    let mut rest = input;
    let mut seconds = 0.0_f64;
    let mut terms = 0_u32;
    while !rest.is_empty() {
        let number_end = rest
            .char_indices()
            .take_while(|(_, character)| character.is_ascii_digit() || *character == '.')
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0);
        if number_end == 0 {
            return Err(bad_ttl());
        }
        let number: f64 = rest[..number_end].parse().map_err(|_| bad_ttl())?;
        rest = &rest[number_end..];
        let (unit, multiplier) = if let Some(tail) = rest.strip_prefix("ms") {
            (tail, 0.001)
        } else if let Some(tail) = rest.strip_prefix("us") {
            (tail, 0.000_001)
        } else if let Some(tail) = rest.strip_prefix("ns") {
            (tail, 0.000_000_001)
        } else if let Some(tail) = rest.strip_prefix('h') {
            (tail, 3_600.0)
        } else if let Some(tail) = rest.strip_prefix('m') {
            (tail, 60.0)
        } else if let Some(tail) = rest.strip_prefix('s') {
            (tail, 1.0)
        } else {
            return Err(bad_ttl());
        };
        seconds += number * multiplier;
        rest = unit;
        terms += 1;
    }
    if terms == 0 || !seconds.is_finite() || seconds <= 0.0 {
        return Err(bad_ttl());
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// Formats a stored Unix expiry as UTC RFC 3339.
#[must_use]
pub fn expiry_text(expires: i64) -> String {
    DateTime::<Utc>::from_timestamp(expires, 0)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| expires.to_string())
}

pub(super) fn identity_of(key: &Key, keys: &[Key], now: i64) -> Result<Identity, String> {
    let mut who = identity(key);
    if key.issuer.is_empty() {
        return Ok(who);
    }
    let Some(issuer) = keys
        .iter()
        .find(|parent| parent.id == key.issuer && !parent.removed)
    else {
        return Err(issuer_refusal(key));
    };
    if issuer.off || issuer.secret.is_empty() || expired(issuer, now) {
        return Err(issuer_refusal(key));
    }
    let parent_models = held_models(&issuer.models, &issuer.accounts);
    if !parent_models.is_empty() {
        who.models = fold_models(&who.models, &parent_models);
        if who.models.is_empty() {
            return Err(format!(
                "the gateway key {:?} may use none of its models now: the key it was issued under no longer lets it",
                key.name
            ));
        }
    }
    who.accounts = fold_accounts(&who.accounts, &issuer.accounts);
    who.read_only |= issuer.read_only;
    who.quotas &= issuer.quotas;
    if let Some(parent_expiry) = issuer.expires {
        who.expires = Some(
            who.expires
                .map_or(parent_expiry, |own| own.min(parent_expiry)),
        );
    }
    who.issuer_name.clone_from(&issuer.name);
    who.issuer_limits.clone_from(&issuer.limits);
    Ok(who)
}

fn issuer_refusal(key: &Key) -> String {
    format!(
        "the gateway key {:?} was issued under a key that is no longer enabled, so it no longer works",
        key.name
    )
}

fn fold_models(child: &[String], issuer: &[String]) -> Vec<String> {
    if child.is_empty() {
        return issuer.to_vec();
    }
    let mut out = Vec::new();
    for pattern in child
        .iter()
        .filter(|pattern| model_allowed(issuer, pattern))
        .chain(
            issuer
                .iter()
                .filter(|pattern| model_allowed(child, pattern)),
        )
    {
        if !out
            .iter()
            .any(|kept: &String| kept.eq_ignore_ascii_case(pattern))
        {
            out.push(pattern.clone());
        }
    }
    out
}

fn fold_accounts(child: &[String], issuer: &[String]) -> Vec<String> {
    if issuer.is_empty() {
        return child.to_vec();
    }
    let by_provider = |accounts: &[String]| {
        let mut grouped = HashMap::<String, Vec<String>>::new();
        for account in accounts {
            if let Some((provider, _)) = account.trim().to_lowercase().split_once('/') {
                grouped
                    .entry(provider.to_owned())
                    .or_default()
                    .push(account.clone());
            }
        }
        grouped
    };
    let child_by_provider = by_provider(child);
    let issuer_by_provider = by_provider(issuer);
    let mut out = Vec::new();
    for account in child {
        let provider = account
            .split_once('/')
            .map(|(provider, _)| provider.trim().to_lowercase())
            .unwrap_or_default();
        if !issuer_by_provider.contains_key(&provider)
            || issuer_by_provider[&provider]
                .iter()
                .any(|parent| parent.eq_ignore_ascii_case(account))
        {
            out.push(account.clone());
        }
    }
    for account in issuer {
        let provider = account
            .split_once('/')
            .map(|(provider, _)| provider.trim().to_lowercase())
            .unwrap_or_default();
        let child_has_shared = child_by_provider.get(&provider).is_some_and(|children| {
            children.iter().any(|child| {
                issuer_by_provider[&provider]
                    .iter()
                    .any(|parent| parent.eq_ignore_ascii_case(child))
            })
        });
        if !child_has_shared {
            out.push(account.clone());
        }
    }
    out
}

fn clean_issued_accounts(input: &[String], known: &[String]) -> Result<Vec<String>, SecretError> {
    if !known.is_empty() {
        return clean_accounts(input, known);
    }
    let mut out = Vec::new();
    for account in input {
        let account = account.trim();
        let valid = account
            .split_once('/')
            .is_some_and(|(provider, reference)| !provider.is_empty() && !reference.is_empty());
        if !valid || account.len() > 300 {
            return Err(key_error(format!(
                "A key's account is <provider>/<account> or <provider>/<key id>: {account}"
            )));
        }
        if !out
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(account))
        {
            out.push(account.to_owned());
        }
    }
    if out.len() > 500 {
        return Err(key_error("A key can list at most 500 accounts"));
    }
    Ok(out)
}

pub(super) fn expired(key: &Key, now: i64) -> bool {
    key.expires.is_some_and(|expires| now >= expires)
}

fn pruned(keys: Vec<Key>, now: i64) -> (Vec<Key>, usize) {
    let ids: HashSet<String> = keys
        .iter()
        .filter(|key| !key.removed)
        .map(|key| key.id.clone())
        .collect();
    let cutoff = now.saturating_sub(EXPIRED_KEPT.as_secs() as i64);
    let before = keys.len();
    let kept: Vec<Key> = keys
        .into_iter()
        .filter(|key| {
            !key.expires.is_some_and(|expires| expires <= cutoff)
                && (key.issuer.is_empty() || ids.contains(key.issuer.as_str()))
        })
        .collect();
    let removed = before - kept.len();
    (kept, removed)
}

fn bad_ttl() -> SecretError {
    key_error("ttl is a duration such as 30m, 1h or 24h")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, KeyStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = KeyStore::beside_token(dir.path().join("hofund.token"));
        (dir, store)
    }

    fn request(ttl: Duration) -> IssueRequest {
        IssueRequest {
            ttl,
            ..IssueRequest::default()
        }
    }

    #[test]
    fn s1_ttl_bounds_lifetime_limit_and_expiry_refusal() {
        let (_dir, store) = scratch();
        assert_eq!(
            parse_ttl("1h30m").expect("duration"),
            Duration::from_secs(5_400)
        );
        assert!(parse_ttl("soon").is_err());
        assert!(store.issue(request(Duration::from_secs(59))).is_err());
        assert!(
            store
                .issue(request(MAX_TTL + Duration::from_secs(1)))
                .is_err()
        );
        let issued = store
            .issue_at(
                IssueRequest {
                    name: "Sandbox".to_owned(),
                    tokens: 100,
                    cost: 2.0,
                    ..request(Duration::from_secs(5_400))
                },
                1_000_000,
            )
            .expect("issue");
        assert_eq!(issued.expires, "1970-01-12T15:16:40+00:00");
        assert_eq!(issued.limit.as_ref().expect("limit").hours, 2);
        assert_eq!(issued.limit.as_ref().expect("limit").since, 1_000_000);
        assert!(matches!(
            store.check_at(&issued.key, 1_005_399).expect("before"),
            KeyAuthentication::Enabled(_)
        ));
        let KeyAuthentication::Refused(message) =
            store.check_at(&issued.key, 1_005_400).expect("expired")
        else {
            panic!("expired key was enabled");
        };
        assert!(message.contains("expired at 1970-01-12T15:16:40+00:00: ask for a new one"));
        assert!(!format!("{issued:?}").contains(&issued.key));
    }

    #[test]
    fn s3_store_refuses_unauthorized_issued_expiring_and_read_only_issuers() {
        let (_dir, store) = scratch();
        let parent = store.add("Parent", None).expect("parent");
        let mut under_parent = request(Duration::from_secs(3_600));
        under_parent.issuer = parent.key.id.clone();
        assert!(store.issue(under_parent.clone()).is_err());
        store.set_issuer(&parent.key.id, true).expect("issuer");
        store
            .set_read_only(&parent.key.id, true)
            .expect("read only");
        assert!(store.issue(under_parent.clone()).is_err());
        store.set_read_only(&parent.key.id, false).expect("full");
        let child = store.issue(under_parent).expect("child");
        assert!(store.set_issuer(&child.id, true).is_err());
        assert!(store.set_issuer(&child.id, false).is_ok());
    }

    #[test]
    fn s5_issued_scope_is_clamped_to_its_issuer() {
        let (_dir, store) = scratch();
        let parent = store.add("Parent", None).expect("parent");
        store
            .set_models(&parent.key.id, &["primary/*".to_owned()])
            .expect("models");
        store
            .set_accounts(
                &parent.key.id,
                &["primary/a".to_owned()],
                &["primary/a".to_owned(), "primary/b".to_owned()],
            )
            .expect("accounts");
        store.set_issuer(&parent.key.id, true).expect("issuer");
        let mut input = IssueRequest {
            issuer: parent.key.id.clone(),
            models: vec!["other/model".to_owned()],
            ..request(Duration::from_secs(3_600))
        };
        assert!(store.issue(input.clone()).is_err());
        input.models = vec!["primary/model".to_owned()];
        input.accounts = vec!["primary/b".to_owned()];
        assert!(store.issue(input.clone()).is_err());
        input.accounts = vec!["primary/Person".to_owned()];
        input.known_accounts = vec!["primary/a\tPerson".to_owned(), "primary/b".to_owned()];
        let child = store.issue(input).expect("allowed child");
        assert_eq!(child.models, ["primary/model"]);
        assert_eq!(child.accounts, ["primary/a"]);

        let mut stored = store.export().expect("export");
        let parent_record = stored
            .iter_mut()
            .find(|key| key.id == parent.key.id)
            .expect("parent record");
        parent_record.expires = Some(2_000_000);
        store.restore(&stored).expect("restore");
        let clamped = store
            .issue_at(
                IssueRequest {
                    issuer: parent.key.id,
                    ..request(Duration::from_secs(3_600))
                },
                1_999_000,
            )
            .expect("clamped");
        assert_eq!(clamped.expires, expiry_text(2_000_000));
    }

    #[test]
    fn s6_every_authentication_folds_the_current_issuer() {
        let (_dir, store) = scratch();
        let parent = store.add("Parent", None).expect("parent");
        store
            .set_models(&parent.key.id, &["primary/*".to_owned()])
            .expect("models");
        store.set_quotas(&parent.key.id, true).expect("quotas");
        store.set_issuer(&parent.key.id, true).expect("issuer");
        let child = store
            .issue(IssueRequest {
                issuer: parent.key.id.clone(),
                models: vec!["primary/model".to_owned()],
                ..request(Duration::from_secs(3_600))
            })
            .expect("child");
        let KeyAuthentication::Enabled(mut who) = store.check(&child.key).expect("identity") else {
            panic!("child not enabled");
        };
        assert_eq!(who.models, ["primary/model"]);
        assert_eq!(who.issuer, parent.key.id);
        assert_eq!(who.issuer_name, "Parent");
        assert!(!who.issues);
        store
            .set_models(
                &child.id,
                &["other/*".to_owned(), "primary/model".to_owned()],
            )
            .expect("widen child on disk");
        who = match store.check(&child.key).expect("folded") {
            KeyAuthentication::Enabled(who) => who,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(who.models, ["primary/model"]);
        store
            .set_models(&parent.key.id, &["primary/other".to_owned()])
            .expect("narrow parent");
        assert!(matches!(
            store.check(&child.key).expect("no overlap"),
            KeyAuthentication::Refused(_)
        ));
        store
            .set_models(&parent.key.id, &["primary/*".to_owned()])
            .expect("restore models");
        store.set_quotas(&child.id, true).expect("child quotas");
        store
            .set_quotas(&parent.key.id, false)
            .expect("parent quotas off");
        who = match store.check(&child.key).expect("quota fold") {
            KeyAuthentication::Enabled(who) => who,
            other => panic!("unexpected {other:?}"),
        };
        assert!(!who.quotas);
        store
            .set_read_only(&parent.key.id, true)
            .expect("parent read only");
        who = match store.check(&child.key).expect("access fold") {
            KeyAuthentication::Enabled(who) => who,
            other => panic!("unexpected {other:?}"),
        };
        assert!(who.read_only);
        store
            .set_enabled(&parent.key.id, false)
            .expect("parent off");
        assert!(matches!(
            store.check(&child.key).expect("parent off"),
            KeyAuthentication::Refused(_)
        ));
        store.set_enabled(&parent.key.id, true).expect("parent on");
        store.remove(&parent.key.id).expect("parent removed");
        assert!(matches!(
            store.check(&child.key).expect("parent removed"),
            KeyAuthentication::Refused(_)
        ));
    }

    #[test]
    fn s8_prune_keeps_recent_expiry_and_removes_old_and_orphaned_keys() {
        let (_dir, store) = scratch();
        let parent = store.add("Parent", None).expect("parent");
        store.set_issuer(&parent.key.id, true).expect("issuer");
        let _child = store
            .issue_at(
                IssueRequest {
                    issuer: parent.key.id.clone(),
                    ..request(Duration::from_secs(60))
                },
                1_000_000,
            )
            .expect("child");
        assert_eq!(store.prune_at(1_003_600).expect("kept"), 0);
        assert_eq!(store.prune_at(1_003_661).expect("old"), 1);
        let child = store
            .issue_at(
                IssueRequest {
                    issuer: parent.key.id.clone(),
                    ..request(Duration::from_secs(3_600))
                },
                1_000_000,
            )
            .expect("second child");
        store.remove(&parent.key.id).expect("remove parent");
        assert_eq!(store.prune_at(1_000_001).expect("orphan"), 1);
        assert!(matches!(
            store.check_at(&child.key, 1_000_001).expect("unknown"),
            KeyAuthentication::Unknown
        ));
    }

    #[test]
    fn s11_on_off_accepts_an_unambiguous_name_and_preserves_user_order() {
        let (_dir, store) = scratch();
        let first = store.add("First", None).expect("first");
        let second = store.add("Second", None).expect("second");
        store.set_enabled_by_name("First", false).expect("off");
        assert!(!store.on(&first.key.id).expect("off"));
        store.set_enabled_by_name("First", true).expect("on");
        assert!(store.on(&first.key.id).expect("on"));
        store.move_key(&second.key.id, 1).expect("move");
        assert_eq!(store.list().expect("list")[0].id, second.key.id);
        store.add("First", None).expect("duplicate name");
        assert!(store.set_enabled_by_name("First", false).is_err());
    }
}
