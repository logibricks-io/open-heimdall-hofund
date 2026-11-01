//! Named gateway keys, their credentials, and their scopes.
//!
//! The store keeps each secret readable, as the reference does, so an
//! administrator can copy a key again after it was made; the file is
//! written atomically with owner-only permissions, and listings carry only
//! a masked form.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{SecretError, TokenFile, write_atomic};

/// Prefix generated for a named gateway key.
pub const KEY_PREFIX: &str = "hofund_key_";
const OWN_PREFIX: &str = "hofund_";
const STORE_NAME: &str = "caller-keys.json";
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// A stored spending-limit description.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyLimit {
    /// Window name: day, week, month, days, or hours.
    pub period: String,
    /// Number of days in a repeating day window.
    pub days: u32,
    /// Number of hours in a repeating hour window.
    pub hours: u32,
    /// Token cap; zero means no token cap.
    pub tokens: u64,
    /// Cost cap in US dollars; zero means no cost cap.
    pub cost: f64,
    /// Whether cache reads count toward the token cap.
    pub cache_reads: bool,
    /// Unix seconds at which a repeating cycle began.
    pub since: i64,
}

/// One named gateway key. The secret persists readable in the owner-only
/// store file; [`KeyStore::list`] clears it and fills `masked` instead.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Key {
    /// Stable id, retained when the secret rotates.
    pub id: String,
    /// Display name, from 1 to 120 Unicode characters.
    pub name: String,
    /// Whether authentication is disabled.
    pub off: bool,
    /// Whether this is the default key mirrored by the legacy token file.
    #[serde(rename = "default")]
    pub is_default: bool,
    /// The secret a client sends; empty in a listing.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub secret: String,
    /// Display-only prefix and suffix of the secret, computed for a listing
    /// and never stored.
    #[serde(skip_serializing)]
    pub masked: String,
    /// Model patterns allowed for this key.
    pub models: Vec<String>,
    /// Account or provider-key patterns allowed for this key.
    pub accounts: Vec<String>,
    /// Whether scoped quota reads are allowed when that API is implemented.
    pub quotas: bool,
    /// Stored for the read-only enforcement phase.
    pub read_only: bool,
    /// Optional Unix-second expiry, stored for the short-lived-key phase.
    pub expires: Option<i64>,
    /// Id of the issuing key, when this key was issued by another key.
    pub issuer: String,
    /// Whether this key may issue short-lived keys.
    pub issues: bool,
    /// Spending limits enforced by the gateway.
    pub limits: Vec<KeyLimit>,
    /// A removed default remains as a tombstone so migration cannot resurrect it.
    #[serde(skip_serializing_if = "is_false")]
    pub removed: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl std::fmt::Debug for Key {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Key")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("off", &self.off)
            .field("is_default", &self.is_default)
            .field(
                "secret",
                &if self.secret.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("masked", &self.masked)
            .field("models", &self.models)
            .field("accounts", &self.accounts)
            .field("quotas", &self.quotas)
            .field("read_only", &self.read_only)
            .field("expires", &self.expires)
            .field("issuer", &self.issuer)
            .field("issues", &self.issues)
            .field("limits", &self.limits)
            .field("removed", &self.removed)
            .finish()
    }
}

impl Key {
    /// The record as a listing shows it: the secret cleared, its mask set.
    #[must_use]
    pub fn shown(mut self) -> Self {
        self.masked = mask(&self.secret);
        self.secret.clear();
        self
    }
}

/// The authenticated caller and the scopes later request phases also consume.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Identity {
    /// Authenticated key id.
    pub key_id: String,
    /// Authenticated key name.
    pub key_name: String,
    /// Effective model patterns, including the accounts-only restriction.
    pub models: Vec<String>,
    /// Allowed accounts or provider-key fingerprints.
    pub accounts: Vec<String>,
    /// Whether scoped quota reads are allowed.
    pub quotas: bool,
    /// Stored read-only flag.
    pub read_only: bool,
    /// Optional Unix-second expiry.
    pub expires: Option<i64>,
    /// Issuing key id.
    pub issuer: String,
    /// Whether the key may issue other keys.
    pub issues: bool,
    /// Whether this is the default sharing key.
    pub is_default: bool,
    /// Stored limit descriptions.
    pub limits: Vec<KeyLimit>,
}

impl Identity {
    /// Whether at least one candidate model id is allowed.
    #[must_use]
    pub fn allows(&self, candidates: &[String]) -> bool {
        self.models.is_empty()
            || candidates
                .iter()
                .any(|id| !id.is_empty() && model_allowed(&self.models, id))
    }

    /// Whether `provider/ref` is allowed by the account scope.
    #[must_use]
    pub fn allows_account(&self, provider: &str, reference: &str) -> bool {
        allows_account(&self.accounts, provider, reference)
    }
}

/// A key and its secret, returned by add and rotate.
#[derive(Clone, PartialEq)]
pub struct KeyWithSecret {
    /// The key as a listing shows it.
    pub key: Key,
    /// The secret to hand the caller.
    pub secret: String,
}

impl std::fmt::Debug for KeyWithSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KeyWithSecret")
            .field("key", &self.key)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// File-backed named-key store beside the legacy token file.
#[derive(Clone, Debug)]
pub struct KeyStore {
    path: PathBuf,
    token_file: TokenFile,
}

impl KeyStore {
    /// Uses `caller-keys.json` beside `token_path`.
    #[must_use]
    pub fn beside_token(token_path: impl Into<PathBuf>) -> Self {
        let token_file = TokenFile::new(token_path);
        let path = token_file
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(STORE_NAME);
        Self { path, token_file }
    }

    /// The key-store path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The legacy token file mirrored by the default key.
    #[must_use]
    pub fn token_file(&self) -> &TokenFile {
        &self.token_file
    }

    /// Lists keys in their stored order, migrating the legacy token first.
    /// A listing never carries a secret, only its mask; [`Self::copy`] is
    /// the administrator's way to read one again.
    pub fn list(&self) -> Result<Vec<Key>, SecretError> {
        let _guard = self.lock()?;
        let keys = self.load_migrated()?;
        Ok(keys
            .into_iter()
            .filter(|key| !key.removed)
            .map(Key::shown)
            .collect())
    }

    /// The secret of the key `id`, for copying it again after it was made
    /// (`reference: internal/access/access.go` copy-key).
    pub fn copy(&self, id: &str) -> Result<String, SecretError> {
        let _guard = self.lock()?;
        let keys = self.load_migrated()?;
        let index = find(&keys, id)?;
        Ok(keys[index].secret.clone())
    }

    /// Authenticates against every stored secret in constant time.
    pub fn authenticate(&self, secret: &str) -> Result<Option<Identity>, SecretError> {
        if secret.is_empty() {
            return Ok(None);
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
        Ok(matched.and_then(|index| {
            let key = &keys[index];
            (!key.off && !key.removed).then(|| identity(key))
        }))
    }

    /// Whether the secret belongs to any stored key, enabled or not.
    pub fn contains(&self, secret: &str) -> Result<bool, SecretError> {
        if secret.is_empty() {
            return Ok(false);
        }
        let _guard = self.lock()?;
        let keys = self.load_migrated()?;
        Ok(keys.iter().any(|key| {
            !key.secret.is_empty() && constant_time_eq(secret.as_bytes(), key.secret.as_bytes())
        }))
    }

    /// Whether `id` names an enabled key.
    pub fn on(&self, id: &str) -> Result<bool, SecretError> {
        let _guard = self.lock()?;
        let keys = self.load_migrated()?;
        Ok(keys
            .iter()
            .any(|key| key.id == id && !key.off && !key.removed && !key.secret.is_empty()))
    }

    /// Adds a key with a generated secret or a caller-supplied one.
    pub fn add(&self, name: &str, own: Option<&str>) -> Result<KeyWithSecret, SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let name = clean_name(name)?;
        let secret = match own {
            Some(secret) => own_secret(secret, &keys)?,
            None => generated_secret()?,
        };
        let key = Key {
            id: random_hex(12)?,
            name,
            secret: secret.clone(),
            ..Key::default()
        };
        keys.push(key.clone());
        self.save(&keys)?;
        Ok(KeyWithSecret {
            key: key.shown(),
            secret,
        })
    }

    /// Rotates a key while retaining its id, name, scopes, and enabled state.
    pub fn rotate(&self, id: &str) -> Result<KeyWithSecret, SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let index = find(&keys, id)?;
        let secret = generated_secret()?;
        keys[index].secret.clone_from(&secret);
        let key = keys[index].clone().shown();
        self.save(&keys)?;
        if key.is_default {
            write_atomic(self.token_file.path(), &secret)?;
        }
        Ok(KeyWithSecret { key, secret })
    }

    /// Rotates the default key, preserving the `token rotate` contract.
    pub fn rotate_default(&self) -> Result<KeyWithSecret, SecretError> {
        let _guard = self.lock()?;
        let keys = self.load_migrated()?;
        let id = keys
            .iter()
            .find(|key| key.is_default && !key.removed)
            .map(|key| key.id.clone())
            .ok_or_else(|| key_error("The default gateway key was removed"))?;
        drop(_guard);
        self.rotate(&id)
    }

    /// Renames a key.
    pub fn rename(&self, id: &str, name: &str) -> Result<(), SecretError> {
        self.change(id, |key| {
            key.name = clean_name(name)?;
            Ok(())
        })
    }

    /// Enables or disables a key.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), SecretError> {
        self.change(id, |key| {
            key.off = !enabled;
            Ok(())
        })
    }

    /// Removes a key. A removed default leaves a migration tombstone.
    pub fn remove(&self, id: &str) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let index = find(&keys, id)?;
        let is_default = keys[index].is_default;
        if is_default {
            keys[index].off = true;
            keys[index].removed = true;
            keys[index].secret.clear();
        } else {
            keys.remove(index);
        }
        self.save(&keys)?;
        if is_default {
            let revoked = super::TokenFile::new(self.token_file.path()).rotate()?;
            let _ = revoked;
        }
        Ok(())
    }

    /// Moves a key to a one-based place in the list without changing the default mirror.
    pub fn move_key(&self, id: &str, place: usize) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let index = find(&keys, id)?;
        if place == 0 || place > keys.len() {
            return Err(key_error("There is no such place in the list of keys"));
        }
        let key = keys.remove(index);
        keys.insert(place - 1, key);
        self.save(&keys)
    }

    /// Replaces a key's model patterns.
    pub fn set_models(&self, id: &str, models: &[String]) -> Result<(), SecretError> {
        let models = clean_models(models)?;
        self.change(id, |key| {
            key.models = models;
            Ok(())
        })
    }

    /// Replaces a key's account patterns after checking them against known canonical ids.
    pub fn set_accounts(
        &self,
        id: &str,
        accounts: &[String],
        known: &[String],
    ) -> Result<(), SecretError> {
        let accounts = clean_accounts(accounts, known)?;
        self.change(id, |key| {
            key.accounts = accounts;
            Ok(())
        })
    }

    /// Sets the scoped-quota flag.
    pub fn set_quotas(&self, id: &str, quotas: bool) -> Result<(), SecretError> {
        self.change(id, |key| {
            key.quotas = quotas;
            Ok(())
        })
    }

    /// Sets full or read-only access. The default key must retain full access.
    pub fn set_read_only(&self, id: &str, read_only: bool) -> Result<(), SecretError> {
        self.change(id, |key| {
            if read_only && key.is_default {
                return Err(key_error(
                    "The default gateway key gives connected agents model access, so it cannot be read-only",
                ));
            }
            key.read_only = read_only;
            Ok(())
        })
    }

    /// Sets whether a key may issue short-lived keys.
    pub fn set_issuer(&self, id: &str, issues: bool) -> Result<(), SecretError> {
        self.change(id, |key| {
            if issues && (!key.issuer.is_empty() || key.expires.is_some()) {
                return Err(key_error(
                    "An issued key cannot issue keys of its own; use a key you created",
                ));
            }
            key.issues = issues;
            Ok(())
        })
    }

    /// Sets, appends, or removes one limit. An index of zero replaces the
    /// first; an index at the current length appends. No index replaces all.
    pub fn set_limit(
        &self,
        id: &str,
        limit: Option<KeyLimit>,
        index: Option<usize>,
        now: i64,
    ) -> Result<(), SecretError> {
        self.change(id, |key| super::limits::set_limit(key, limit, index, now))
    }

    /// Resets one limit, or every limit when `index` is `None`, from `now`.
    pub fn reset_limits(
        &self,
        id: &str,
        index: Option<usize>,
        now: i64,
    ) -> Result<(), SecretError> {
        self.change(id, |key| super::limits::reset_limits(key, index, now))
    }

    /// Rewrites an account reference after an account id or provider-key fingerprint changes.
    pub fn replace_account(
        &self,
        provider: &str,
        was: &str,
        now: &str,
    ) -> Result<usize, SecretError> {
        if was == now {
            return Ok(0);
        }
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let old = format!("{provider}/{was}").to_lowercase();
        let mut moved = 0;
        for key in &mut keys {
            for account in &mut key.accounts {
                if account.trim().to_lowercase() == old {
                    *account = format!("{provider}/{now}");
                    moved += 1;
                }
            }
        }
        if moved > 0 {
            self.save(&keys)?;
        }
        Ok(moved)
    }

    /// Rewrites exact model ids after a model move; wildcard patterns stay unchanged.
    pub fn move_model(&self, from: &str, to: &str) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let mut changed = false;
        for key in &mut keys {
            let mut moved = false;
            let mut models = Vec::new();
            for model in &key.models {
                let model = if model.eq_ignore_ascii_case(from) {
                    moved = true;
                    to
                } else {
                    model
                };
                if !models
                    .iter()
                    .any(|existing: &&str| existing.eq_ignore_ascii_case(model))
                {
                    models.push(model);
                }
            }
            if moved {
                key.models = models.into_iter().map(str::to_owned).collect();
                changed = true;
            }
        }
        if changed {
            self.save(&keys)?;
        }
        Ok(())
    }

    /// Exports the complete key records, secrets included, for a backup.
    pub fn export(&self) -> Result<Vec<Key>, SecretError> {
        let _guard = self.lock()?;
        self.load_migrated()
    }

    /// Restores key records atomically.
    pub fn restore(&self, keys: &[Key]) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        self.save(keys)
    }

    fn change(
        &self,
        id: &str,
        change: impl FnOnce(&mut Key) -> Result<(), SecretError>,
    ) -> Result<(), SecretError> {
        let _guard = self.lock()?;
        let mut keys = self.load_migrated()?;
        let index = find(&keys, id)?;
        change(&mut keys[index])?;
        self.save(&keys)
    }

    fn lock(&self) -> Result<StoreGuard, SecretError> {
        let thread = STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = lock_path(&self.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| SecretError::File(error.to_string()))?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| SecretError::File(error.to_string()))?;
        file.lock()
            .map_err(|error| SecretError::File(error.to_string()))?;
        Ok(StoreGuard {
            _file: file,
            _thread: thread,
        })
    }

    fn load_migrated(&self) -> Result<Vec<Key>, SecretError> {
        let mut keys = self.load()?;
        let secret = self.token_file.load_or_create()?;
        if keys.iter().any(|key| key.is_default) {
            return Ok(keys);
        }
        if let Some(index) = keys
            .iter()
            .position(|key| constant_time_eq(secret.as_bytes(), key.secret.as_bytes()))
        {
            keys[index].is_default = true;
        } else {
            // The id is derived from the token so a retried migration
            // names the same key (`reference: internal/access/access.go`
            // legacyLANKey).
            let suffix = secret_digest(&secret)[..24].to_owned();
            keys.push(Key {
                id: format!("default-{suffix}"),
                name: "Sharing".to_owned(),
                is_default: true,
                secret,
                ..Key::default()
            });
        }
        self.save(&keys)?;
        Ok(keys)
    }

    fn load(&self) -> Result<Vec<Key>, SecretError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SecretError::File(error.to_string())),
        };
        serde_json::from_slice(&bytes)
            .map_err(|error| SecretError::File(format!("invalid caller key store: {error}")))
    }

    fn save(&self, keys: &[Key]) -> Result<(), SecretError> {
        let text = serde_json::to_string_pretty(keys)
            .map_err(|error| SecretError::File(error.to_string()))?;
        write_atomic(&self.path, &text)
    }
}

/// Holds the in-process lock and the cross-process lock on `<store>.lock`
/// for one read-modify-write; the CLI and the daemon both write the store.
struct StoreGuard {
    _file: fs::File,
    _thread: std::sync::MutexGuard<'static, ()>,
}

fn lock_path(store: &Path) -> PathBuf {
    let mut name = store.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn find(keys: &[Key], id: &str) -> Result<usize, SecretError> {
    keys.iter()
        .position(|key| key.id == id && !key.removed)
        .ok_or_else(|| key_error("Key not found"))
}

fn identity(key: &Key) -> Identity {
    Identity {
        key_id: key.id.clone(),
        key_name: key.name.clone(),
        models: held_models(&key.models, &key.accounts),
        accounts: key.accounts.clone(),
        quotas: key.quotas,
        read_only: key.read_only,
        expires: key.expires,
        issuer: key.issuer.clone(),
        issues: key.issues,
        is_default: key.is_default,
        limits: key.limits.clone(),
    }
}

/// Effective models: an accounts-only key is held to those accounts' providers.
#[must_use]
pub fn held_models(models: &[String], accounts: &[String]) -> Vec<String> {
    if !models.is_empty() || accounts.is_empty() {
        return models.to_vec();
    }
    let mut held = Vec::new();
    for account in accounts {
        let Some((provider, _)) = account.trim().split_once('/') else {
            continue;
        };
        if provider.is_empty() {
            continue;
        }
        let pattern = format!("{provider}/*");
        if !held
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&pattern))
        {
            held.push(pattern);
        }
    }
    if held.is_empty() {
        held.push("/".to_owned());
    }
    held
}

/// Cleans and validates model patterns.
pub fn clean_models(input: &[String]) -> Result<Vec<String>, SecretError> {
    let mut out: Vec<String> = Vec::new();
    for model in input {
        let model = model.trim();
        if model.is_empty()
            || out
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(model))
        {
            continue;
        }
        if !model.contains('/') || model.starts_with('/') || model.ends_with('/') {
            return Err(key_error(format!(
                "A key's model is <provider>/<model> or <provider>/*, like primary/model-a or primary/*: {model}"
            )));
        }
        if model.len() > 300 {
            return Err(key_error("A key's model is at most 300 characters"));
        }
        out.push(model.to_owned());
    }
    if out.len() > 500 {
        return Err(key_error("A key can list at most 500 models"));
    }
    Ok(out)
}

/// Case-insensitive model matching; `*` matches any byte run, including `/`.
#[must_use]
pub fn model_allowed(patterns: &[String], id: &str) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            pattern.eq_ignore_ascii_case(id) || (pattern.contains('*') && glob_matches(pattern, id))
        })
}

fn glob_matches(pattern: &str, id: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let id = id.to_lowercase();
    let (p, s) = (pattern.as_bytes(), id.as_bytes());
    let mut previous = vec![false; s.len() + 1];
    previous[0] = true;
    for &token in p {
        let mut current = vec![false; s.len() + 1];
        if token == b'*' {
            current[0] = previous[0];
            for index in 1..=s.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=s.len() {
                current[index] = previous[index - 1] && token == s[index - 1];
            }
        }
        previous = current;
    }
    previous[s.len()]
}

/// Cleans account patterns and resolves aliases against `known` canonical ids.
pub fn clean_accounts(input: &[String], known: &[String]) -> Result<Vec<String>, SecretError> {
    let mut out: Vec<String> = Vec::new();
    for account in input {
        let account = account.trim();
        if account.is_empty() {
            continue;
        }
        let Some((provider, reference)) = account.split_once('/') else {
            return Err(key_error(format!(
                "A key's account is <provider>/<account> or <provider>/<key id>: {account}"
            )));
        };
        if provider.is_empty() || reference.is_empty() {
            return Err(key_error(format!(
                "A key's account is <provider>/<account> or <provider>/<key id>: {account}"
            )));
        }
        if account.len() > 300 {
            return Err(key_error("A key's account is at most 300 characters"));
        }
        let kept = known
            .iter()
            .find_map(|candidate| {
                let (canonical, alias) = candidate.split_once('\t').unwrap_or((candidate, ""));
                (canonical.eq_ignore_ascii_case(account)
                    || (alias.eq_ignore_ascii_case(reference)
                        && canonical
                            .split_once('/')
                            .is_some_and(|(p, _)| p.eq_ignore_ascii_case(provider))))
                .then(|| canonical.to_owned())
            })
            .ok_or_else(|| key_error(format!("{provider} has no account or key {reference:?}")))?;
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&kept))
        {
            out.push(kept);
        }
    }
    if out.len() > 500 {
        return Err(key_error("A key can list at most 500 accounts"));
    }
    Ok(out)
}

/// Account scope: providers absent from the list retain their ordinary access.
#[must_use]
pub fn allows_account(accounts: &[String], provider: &str, reference: &str) -> bool {
    if accounts.is_empty() {
        return true;
    }
    let provider = provider.trim().to_lowercase();
    let key = format!("{provider}/{}", reference.trim().to_lowercase());
    let mut named = false;
    for account in accounts {
        let account = account.trim().to_lowercase();
        if account == key {
            return true;
        }
        if account.split_once('/').is_some_and(|(p, _)| p == provider) {
            named = true;
        }
    }
    !named
}

fn clean_name(name: &str) -> Result<String, SecretError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(key_error("Use a name between 1 and 120 characters"));
    }
    Ok(name.to_owned())
}

fn own_secret(secret: &str, keys: &[Key]) -> Result<String, SecretError> {
    let secret = secret.trim();
    if !(8..=256).contains(&secret.len()) {
        return Err(key_error("Use a key between 8 and 256 characters"));
    }
    if !secret.bytes().all(|byte| byte > b' ' && byte <= b'~') {
        return Err(key_error(
            "A key can have only letters, digits and ASCII symbols, no spaces",
        ));
    }
    if secret.starts_with(OWN_PREFIX) {
        return Err(key_error(
            "A key of your own cannot start with hofund_; leave it empty to generate one",
        ));
    }
    if keys
        .iter()
        .any(|key| constant_time_eq(secret.as_bytes(), key.secret.as_bytes()))
    {
        return Err(key_error("Another gateway key already has this key"));
    }
    Ok(secret.to_owned())
}

fn generated_secret() -> Result<String, SecretError> {
    Ok(format!("{KEY_PREFIX}{}", random_hex(24)?))
}

fn random_hex(length: usize) -> Result<String, SecretError> {
    let mut bytes = vec![0_u8; length];
    getrandom::getrandom(&mut bytes).map_err(|error| SecretError::Random(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn secret_digest(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn mask(secret: &str) -> String {
    if secret.is_empty() {
        return String::new();
    }
    if secret.starts_with(OWN_PREFIX) {
        let prefix = if secret.starts_with(KEY_PREFIX) {
            KEY_PREFIX
        } else {
            "hofund_"
        };
        return format!("{prefix}…{}", &secret[secret.len().saturating_sub(6)..]);
    }
    if secret.len() >= 12 {
        format!("…{}", &secret[secret.len() - 4..])
    } else {
        "…".to_owned()
    }
}

fn key_error(message: impl Into<String>) -> SecretError {
    SecretError::Key(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, KeyStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = KeyStore::beside_token(dir.path().join("hofund.token"));
        (dir, store)
    }

    #[test]
    fn k1_record_fields_round_trip_and_unknown_fields_are_tolerated() {
        let (_dir, store) = scratch();
        let added = store.add("Laptop", Some("client-secret-123")).expect("add");
        assert_eq!(added.key.masked, "…-123");
        assert!(
            added.key.secret.is_empty(),
            "a returned record carries no secret"
        );
        let path = store.path();
        let text = fs::read_to_string(path).expect("store");
        let persisted: Vec<serde_json::Value> = serde_json::from_str(&text).expect("json");
        let laptop = persisted
            .iter()
            .find(|key| key["name"] == "Laptop")
            .expect("persisted");
        assert_eq!(laptop["secret"], "client-secret-123", "stored readable");
        assert!(laptop.get("masked").is_none(), "the mask is never stored");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).expect("loosen");
            store.rename(&added.key.id, "Laptop").expect("rewrite");
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                0o600,
                "a write leaves the store owner-only again"
            );
        }
        let with_future = text.replace("\"id\"", "\"future\": true, \"id\"");
        fs::write(path, with_future).expect("future field");
        let loaded = store.list().expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].name, "Laptop");
        assert!(loaded.iter().all(|key| key.secret.is_empty()));
        let exported = store.export().expect("export");
        assert_eq!(exported[1].secret, "client-secret-123");
        store.restore(&exported).expect("restore");
        assert_eq!(store.export().expect("export"), exported);
    }

    #[test]
    fn k2_single_token_migrates_to_the_default_key_without_changing_it() {
        let (_dir, store) = scratch();
        let token = store.token_file().load_or_create().expect("token");
        let keys = store.list().expect("migrate");
        assert_eq!(keys.len(), 1);
        assert!(keys[0].is_default);
        assert!(keys[0].secret.is_empty());
        assert_eq!(store.copy(&keys[0].id).expect("copy"), token);
        assert_eq!(store.token_file().read().expect("token"), token);
        let identity = store.authenticate(&token).expect("auth").expect("key");
        assert!(identity.is_default);
        assert_eq!(store.list().expect("stable").len(), 1);
    }

    #[test]
    fn k3_every_action_preserves_identity_and_validates_changes() {
        let (_dir, store) = scratch();
        let generated = store.add("Generated", None).expect("generated");
        assert!(generated.secret.starts_with(KEY_PREFIX));
        assert!(store.add("Prefix", Some("hofund_own_secret")).is_err());
        let own = store.add("Own", Some("my-own-secret-123")).expect("own");
        assert!(store.add("Duplicate", Some("my-own-secret-123")).is_err());
        let id = own.key.id.clone();
        let rotated = store.rotate(&id).expect("rotate");
        assert_eq!(rotated.key.id, id);
        assert!(
            store
                .authenticate("my-own-secret-123")
                .expect("old")
                .is_none()
        );
        store.rename(&id, "Renamed").expect("rename");
        assert!(store.rename(&id, "").is_err());
        assert!(store.rename(&id, &"x".repeat(121)).is_err());
        store.set_enabled(&id, false).expect("off");
        assert!(!store.on(&id).expect("on"));
        store.set_enabled(&id, true).expect("on");
        let default_secret = store.token_file().read().expect("default token");
        store.move_key(&id, 1).expect("move");
        assert_eq!(store.list().expect("list")[0].id, id);
        assert_eq!(
            store.token_file().read().expect("default token"),
            default_secret
        );
        store
            .set_models(&id, &["primary/*".to_owned(), "group/work".to_owned()])
            .expect("models");
        store
            .set_accounts(
                &id,
                &["primary/account-a".to_owned()],
                &["primary/account-a".to_owned()],
            )
            .expect("accounts");
        store.set_quotas(&id, true).expect("quotas");
        store.set_read_only(&id, true).expect("read only");
        store.set_issuer(&id, true).expect("issuer");
        let key = store.list().expect("list").remove(0);
        assert!(key.quotas && key.read_only && key.issues);
        let default_id = store
            .list()
            .expect("list")
            .into_iter()
            .find(|key| key.is_default)
            .expect("default")
            .id;
        assert!(store.set_read_only(&default_id, true).is_err());
        store.remove(&id).expect("remove");
        assert!(
            store
                .authenticate(&rotated.secret)
                .expect("removed")
                .is_none()
        );
    }

    #[test]
    fn k6_model_patterns_globs_groups_and_held_models_match() {
        assert!(model_allowed(
            &["primary/model-a".to_owned()],
            "PRIMARY/MODEL-A"
        ));
        assert!(model_allowed(&["primary/*".to_owned()], "primary/a/b"));
        assert!(model_allowed(&["group/work".to_owned()], "GROUP/WORK"));
        assert!(model_allowed(&["*/model-*".to_owned()], "other/model-a"));
        assert!(!model_allowed(&["primary/a".to_owned()], "other/a"));
        assert_eq!(
            held_models(&[], &["primary/a".to_owned(), "other/b".to_owned()]),
            ["primary/*".to_owned(), "other/*".to_owned()]
        );
    }

    #[test]
    fn k7_account_validation_rename_and_model_move_rewrite_scopes() {
        let (_dir, store) = scratch();
        let added = store.add("Scoped", None).expect("add");
        let id = added.key.id;
        store
            .set_accounts(
                &id,
                &["primary/a".to_owned()],
                &["primary/a".to_owned(), "primary/b".to_owned()],
            )
            .expect("accounts");
        assert!(
            store
                .set_accounts(
                    &id,
                    &["primary/missing".to_owned()],
                    &["primary/a".to_owned()]
                )
                .is_err()
        );
        assert_eq!(
            clean_accounts(
                &["primary/Person".to_owned()],
                &["primary/b\tPerson".to_owned()]
            )
            .expect("account label"),
            ["primary/b"]
        );
        store
            .set_models(&id, &["primary/old".to_owned(), "primary/*-old".to_owned()])
            .expect("models");
        assert_eq!(
            store.replace_account("primary", "a", "b").expect("rename"),
            1
        );
        store
            .move_model("primary/old", "primary/new")
            .expect("move");
        let key = store.export().expect("export").remove(1);
        assert_eq!(key.accounts, ["primary/b"]);
        assert_eq!(key.models, ["primary/new", "primary/*-old"]);
        assert!(allows_account(&key.accounts, "primary", "b"));
        assert!(!allows_account(&key.accounts, "primary", "a"));
        assert!(allows_account(&key.accounts, "other", "a"));
    }

    #[test]
    fn k10_quotas_flag_is_stored_and_exposed_on_identity() {
        let (_dir, store) = scratch();
        let added = store.add("Quota", Some("quota-secret-123")).expect("add");
        store.set_quotas(&added.key.id, true).expect("set");
        let identity = store
            .authenticate("quota-secret-123")
            .expect("auth")
            .expect("identity");
        assert!(identity.quotas);
        store.set_quotas(&added.key.id, false).expect("clear");
        assert!(
            !store
                .authenticate("quota-secret-123")
                .expect("auth")
                .expect("identity")
                .quotas
        );
    }

    #[test]
    fn k13_export_restore_replaces_the_whole_store() {
        let (_dir, store) = scratch();
        let added = store.add("Backup", Some("backup-secret-123")).expect("add");
        let exported = store.export().expect("export");
        store.remove(&added.key.id).expect("remove");
        assert_eq!(store.list().expect("list").len(), 1);
        store.restore(&exported).expect("restore");
        assert_eq!(store.export().expect("export"), exported);
        assert_eq!(store.list().expect("list").len(), 2);
        assert!(
            store
                .authenticate("backup-secret-123")
                .expect("auth")
                .is_some()
        );
    }

    #[test]
    fn k15_lock_file_serialises_writers_across_processes() {
        let (_dir, store) = scratch();
        store.list().expect("migrate");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path(store.path()))
            .expect("lock file");
        assert_eq!(
            lock_path(store.path())
                .file_name()
                .and_then(|name| name.to_str()),
            Some("caller-keys.json.lock")
        );
        lock.lock().expect("hold the lock as another process would");
        let writer = KeyStore::beside_token(store.token_file().path().to_path_buf());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let added = writer.add("Second", Some("second-secret-123"));
            done_tx.send(()).ok();
            added
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "a writer waits while another process holds the lock"
        );
        store_raw_add(store.path(), "held-by-other");
        lock.unlock().expect("release");
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("writer proceeds once the lock is released");
        handle.join().expect("writer thread").expect("add");
        let names: Vec<String> = store
            .list()
            .expect("list")
            .into_iter()
            .map(|key| key.name)
            .collect();
        assert!(names.contains(&"held-by-other".to_owned()), "{names:?}");
        assert!(names.contains(&"Second".to_owned()), "{names:?}");
    }

    #[test]
    fn k16_a_key_is_copied_again_and_never_listed_or_logged() {
        let (_dir, store) = scratch();
        let generated = store.add("Copyable", None).expect("add");
        let id = generated.key.id.clone();
        assert_eq!(store.copy(&id).expect("copy"), generated.secret);
        assert!(
            generated.key.masked.starts_with(KEY_PREFIX)
                && generated
                    .key
                    .masked
                    .ends_with(&generated.secret[generated.secret.len() - 6..])
        );
        assert_eq!(
            generated.key.masked,
            format!(
                "{KEY_PREFIX}…{}",
                &generated.secret[generated.secret.len() - 6..]
            )
        );
        let own = store.add("Own", Some("my-own-key-1234")).expect("own");
        assert_eq!(own.key.masked, "…1234");
        store.set_enabled(&id, false).expect("off");
        assert_eq!(store.copy(&id).expect("copy while off"), generated.secret);
        let rotated = store.rotate(&id).expect("rotate");
        assert_eq!(store.copy(&id).expect("copy rotated"), rotated.secret);
        assert_ne!(rotated.secret, generated.secret);
        assert!(store.copy("missing").is_err());
        let listed = store.list().expect("list");
        assert!(
            listed
                .iter()
                .all(|key| key.secret.is_empty() && !key.masked.is_empty())
        );
        let full = store.export().expect("export");
        let debug = format!("{full:?} {rotated:?}");
        assert!(!debug.contains(&rotated.secret), "{debug}");
        assert!(debug.contains("<redacted>"));
        let default_id = full
            .iter()
            .find(|key| key.is_default)
            .expect("default")
            .id
            .clone();
        let token = store.token_file().read().expect("token");
        store.remove(&default_id).expect("remove default");
        assert!(store.copy(&default_id).is_err());
        let text = fs::read_to_string(store.path()).expect("store");
        assert!(!text.contains(&token), "a removed default keeps no secret");
        assert!(!store.contains(&token).expect("contains"));
    }

    /// Appends a record the way another process would while holding the lock.
    fn store_raw_add(path: &Path, name: &str) {
        let mut keys: Vec<Key> =
            serde_json::from_slice(&fs::read(path).expect("store")).expect("keys");
        keys.push(Key {
            id: format!("raw-{name}"),
            name: name.to_owned(),
            secret: format!("raw-secret-{name}"),
            ..Key::default()
        });
        fs::write(path, serde_json::to_string_pretty(&keys).expect("json")).expect("write");
    }
}
