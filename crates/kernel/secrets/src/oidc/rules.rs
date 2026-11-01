//! Persisted OIDC trust rules beside the gateway-key store.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use getrandom::getrandom;
use serde::{Deserialize, Serialize};

use crate::keys::{KeyStore, MAX_TTL, MIN_TTL};
use crate::{SecretError, TokenFile, write_atomic};

const STORE_NAME: &str = "oidc-rules.json";
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Default HTTPS issuer for common CI workflow OIDC when a rule names none.
pub const DEFAULT_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Default lifetime when a rule sets no TTL.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Claim names that pin a workflow to one repository or owner (no wildcards).
const WORKFLOW_SCOPE_CLAIMS: &[&str] = &[
    "repository",
    "repository_owner",
    "repository_id",
    "repository_owner_id",
];

/// One way an OIDC token may be traded for a short-lived gateway key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// Stable rule id.
    pub id: String,
    /// Id of the gateway key keys are issued under; it must be allowed to issue.
    pub key: String,
    /// HTTPS issuer URL, normalized without a trailing slash.
    pub issuer: String,
    /// Required token audience; default `hofund-<id>` at creation.
    pub audience: String,
    /// Claim names and patterns [`claim_pattern_matches`] must accept.
    pub claims: std::collections::BTreeMap<String, String>,
    /// Maximum issued key lifetime in seconds.
    pub ttl: i64,
}

impl Rule {
    /// The rule's TTL as a duration.
    #[must_use]
    pub fn lifetime(&self) -> Duration {
        Duration::from_secs(u64::try_from(self.ttl).unwrap_or(0))
    }
}

/// File-backed OIDC rule store beside the legacy token file.
#[derive(Clone, Debug)]
pub struct RuleStore {
    path: PathBuf,
}

impl RuleStore {
    /// Uses `oidc-rules.json` beside `token_path`.
    #[must_use]
    pub fn beside_token(token_path: impl Into<PathBuf>) -> Self {
        let token_file = TokenFile::new(token_path);
        let path = token_file
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(STORE_NAME);
        Self { path }
    }

    /// The rules file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lists every rule.
    pub fn list(&self) -> Result<Vec<Rule>, SecretError> {
        let _guard = lock()?;
        load(&self.path)
    }

    /// Validates and stores a new rule, assigning id and defaults.
    pub fn add(&self, mut rule: RuleDraft, keys: &KeyStore) -> Result<Rule, SecretError> {
        rule.normalize()?;
        rule.validate(keys)?;
        let mut id = [0_u8; 6];
        getrandom(&mut id).map_err(|error| SecretError::Random(error.to_string()))?;
        let id: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
        let audience = if rule.audience.trim().is_empty() {
            format!("hofund-{id}")
        } else {
            rule.audience.trim().to_owned()
        };
        let stored = Rule {
            id,
            key: rule.key,
            issuer: rule.issuer,
            audience,
            claims: rule.claims,
            ttl: rule.ttl,
        };
        let _guard = lock()?;
        let mut rules = load(&self.path)?;
        rules.push(stored.clone());
        save(&self.path, &rules)?;
        Ok(stored)
    }

    /// Removes the rule with `id`.
    pub fn remove(&self, id: &str) -> Result<(), SecretError> {
        let _guard = lock()?;
        let mut rules = load(&self.path)?;
        let index = rules.iter().position(|rule| rule.id == id).ok_or_else(|| {
            SecretError::Key(format!(
                "no rule has the id {id:?} (hofund gateway-key oidc list)"
            ))
        })?;
        rules.remove(index);
        save(&self.path, &rules)
    }
}

/// Input for [`RuleStore::add`]; id and audience may be filled in by the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleDraft {
    /// Issuer gateway key id.
    pub key: String,
    /// HTTPS issuer URL; blank selects [`DEFAULT_ISSUER`].
    pub issuer: String,
    /// Required audience; blank selects `hofund-<new id>`.
    pub audience: String,
    /// Claim patterns.
    pub claims: std::collections::BTreeMap<String, String>,
    /// TTL in seconds; zero selects [`DEFAULT_TTL`].
    pub ttl: i64,
}

impl RuleDraft {
    fn normalize(&mut self) -> Result<(), SecretError> {
        self.issuer = self.issuer.trim().trim_end_matches('/').to_owned();
        if self.issuer.is_empty() {
            self.issuer = DEFAULT_ISSUER.to_owned();
        }
        if !valid_https_issuer(&self.issuer) {
            return Err(SecretError::Key(format!(
                "the issuer is an https URL, such as {DEFAULT_ISSUER}"
            )));
        }
        if self.claims.is_empty() {
            return Err(SecretError::Key(
                "a rule names at least one claim the token must have, such as repository=owner/name"
                    .to_owned(),
            ));
        }
        for (name, value) in &self.claims {
            if name.trim() != name.as_str() || name.is_empty() || value.is_empty() {
                return Err(SecretError::Key(format!(
                    "a claim is name=value, both given: {name:?}={value:?}"
                )));
            }
            if claim_pattern_valid(value).is_err() {
                return Err(SecretError::Key(format!(
                    "the value of {name} isn't a pattern claim matching reads: {value:?}"
                )));
            }
        }
        if self.issuer == DEFAULT_ISSUER && !workflow_scope_pinned(&self.claims) {
            return Err(SecretError::Key(
                "a CI workflow issuer rule names the repository (repository=owner/name) or its owner (repository_owner=owner) without a wildcard: otherwise any repository's workflow could get a key"
                    .to_owned(),
            ));
        }
        if self.ttl == 0 {
            self.ttl = i64::try_from(DEFAULT_TTL.as_secs()).unwrap_or(3600);
        }
        let lifetime = Duration::from_secs(u64::try_from(self.ttl).unwrap_or(0));
        if lifetime < MIN_TTL || lifetime > MAX_TTL {
            return Err(SecretError::Key(format!(
                "a key lives from {} to {} days",
                format_duration(MIN_TTL),
                MAX_TTL.as_secs() / (24 * 3600)
            )));
        }
        Ok(())
    }

    fn validate(&self, keys: &KeyStore) -> Result<(), SecretError> {
        let listed = keys.list()?;
        let Some(parent) = listed.iter().find(|key| key.id == self.key) else {
            return Err(SecretError::Key(format!(
                "no gateway key has the id {:?} (hofund gateway-key list)",
                self.key
            )));
        };
        if !parent.issues || !parent.issuer.is_empty() {
            return Err(SecretError::Key(format!(
                "the gateway key {} can't issue keys: let it with hofund gateway-key issuer {} on",
                parent.name, self.key
            )));
        }
        Ok(())
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() >= 60 {
        format!("{}m", duration.as_secs() / 60)
    } else {
        format!("{}s", duration.as_secs())
    }
}

fn workflow_scope_pinned(claims: &std::collections::BTreeMap<String, String>) -> bool {
    WORKFLOW_SCOPE_CLAIMS.iter().any(|name| {
        claims.get(*name).is_some_and(|value| {
            !value.contains('*') && !value.contains('?') && !value.contains('[')
        })
    })
}

fn valid_https_issuer(issuer: &str) -> bool {
    let Some(rest) = issuer.strip_prefix("https://") else {
        return false;
    };
    if rest.is_empty() || rest.contains('?') || rest.contains('#') {
        return false;
    }
    rest.split('/').next().is_some_and(|host| !host.is_empty())
}

fn lock() -> Result<StoreGuard, SecretError> {
    let guard = STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(StoreGuard { guard })
}

struct StoreGuard {
    #[allow(dead_code)]
    guard: std::sync::MutexGuard<'static, ()>,
}

fn load(path: &Path) -> Result<Vec<Rule>, SecretError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            SecretError::File(format!("{} can't be read: {error}", path.display()))
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(SecretError::File(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

fn save(path: &Path, rules: &[Rule]) -> Result<(), SecretError> {
    let bytes = serde_json::to_vec_pretty(rules)
        .map_err(|error| SecretError::File(format!("cannot encode {}: {error}", path.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|error| SecretError::File(format!("cannot encode {}: {error}", path.display())))?;
    write_atomic(path, &text)
}

/// Whether `pattern` matches `value` using the reference's claim glob rules.
pub fn claim_pattern_matches(pattern: &str, value: &str) -> bool {
    path_match_inner(pattern.as_bytes(), value.as_bytes()).unwrap_or_default()
}

fn claim_pattern_valid(pattern: &str) -> Result<(), ()> {
    path_match_inner(pattern.as_bytes(), b"\x00").map(|_| ())
}

fn path_match_inner(pattern: &[u8], name: &[u8]) -> Result<bool, ()> {
    if pattern.is_empty() {
        return Ok(name.is_empty());
    }
    if pattern[0] == b'*' {
        if pattern.len() == 1 {
            return Ok(true);
        }
        let rest = &pattern[1..];
        for index in 0..=name.len() {
            if path_match_inner(rest, &name[index..])? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if name.is_empty() {
        return Ok(false);
    }
    if pattern[0] == b'?' {
        return path_match_inner(&pattern[1..], &name[1..]);
    }
    if pattern[0] == b'[' {
        let end = pattern[1..]
            .iter()
            .position(|&byte| byte == b']')
            .ok_or(())?;
        let class = &pattern[1..=end];
        let mut matched = false;
        let mut index = 0;
        if class.len() > 1 && class[1] == b'^' {
            index = 1;
        }
        while index < class.len() {
            if index + 2 < class.len() && class[index + 1] == b'-' {
                let (start, end) = (class[index], class[index + 2]);
                if start <= name[0] && name[0] <= end {
                    matched = true;
                }
                index += 3;
            } else if class[index] == name[0] {
                matched = true;
                index += 1;
            } else {
                index += 1;
            }
        }
        if class.len() > 1 && class[1] == b'^' {
            matched = !matched;
        }
        if !matched {
            return Ok(false);
        }
        return path_match_inner(&pattern[end + 2..], &name[1..]);
    }
    if pattern[0] != name[0] {
        return Ok(false);
    }
    path_match_inner(&pattern[1..], &name[1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyStore;

    fn store() -> (tempfile::TempDir, RuleStore, KeyStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let token = dir.path().join("hofund.token");
        let keys = KeyStore::beside_token(&token);
        keys.list().expect("migrate");
        let rules = RuleStore::beside_token(&token);
        (dir, rules, keys)
    }

    fn issuer_key(keys: &KeyStore) -> String {
        let parent = keys.add("CI", None).expect("add");
        keys.set_issuer(&parent.key.id, true).expect("issuer");
        parent.key.id
    }

    #[test]
    fn d28_rules_add_refuses_invalid_drafts() {
        let (_dir, rules, keys) = store();
        let key = issuer_key(&keys);
        let plain = keys.add("plain", None).expect("plain").key.id;
        let cases = [
            (
                "workflow issuer without repository scope",
                RuleDraft {
                    key: key.clone(),
                    claims: std::collections::BTreeMap::from([(
                        "ref".to_owned(),
                        "refs/heads/main".to_owned(),
                    )]),
                    ..RuleDraft::default()
                },
            ),
            (
                "wildcard owner",
                RuleDraft {
                    key: key.clone(),
                    issuer: DEFAULT_ISSUER.to_owned(),
                    claims: std::collections::BTreeMap::from([(
                        "repository_owner".to_owned(),
                        "*".to_owned(),
                    )]),
                    ..RuleDraft::default()
                },
            ),
            (
                "no claims",
                RuleDraft {
                    key: key.clone(),
                    issuer: "https://idp.example".to_owned(),
                    ..RuleDraft::default()
                },
            ),
            (
                "http issuer",
                RuleDraft {
                    key: key.clone(),
                    issuer: "http://idp.example".to_owned(),
                    claims: std::collections::BTreeMap::from([("sub".to_owned(), "x".to_owned())]),
                    ..RuleDraft::default()
                },
            ),
            (
                "key not allowed to issue",
                RuleDraft {
                    key: plain,
                    claims: std::collections::BTreeMap::from([(
                        "repository".to_owned(),
                        "acme/app".to_owned(),
                    )]),
                    ..RuleDraft::default()
                },
            ),
            (
                "ttl too long",
                RuleDraft {
                    key: key.clone(),
                    ttl: i64::try_from(MAX_TTL.as_secs() + 1).unwrap_or(i64::MAX),
                    claims: std::collections::BTreeMap::from([(
                        "repository".to_owned(),
                        "acme/app".to_owned(),
                    )]),
                    ..RuleDraft::default()
                },
            ),
            (
                "invalid pattern",
                RuleDraft {
                    key: key.clone(),
                    claims: std::collections::BTreeMap::from([
                        ("repository".to_owned(), "acme/app".to_owned()),
                        ("ref".to_owned(), "[".to_owned()),
                    ]),
                    ..RuleDraft::default()
                },
            ),
        ];
        for (name, draft) in cases {
            assert!(rules.add(draft, &keys).is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn d28_rules_add_list_remove_round_trip() {
        let (_dir, rules, keys) = store();
        let key = issuer_key(&keys);
        let stored = rules
            .add(
                RuleDraft {
                    key: key.clone(),
                    issuer: DEFAULT_ISSUER.to_owned(),
                    claims: std::collections::BTreeMap::from([(
                        "repository_owner".to_owned(),
                        "acme".to_owned(),
                    )]),
                    ..RuleDraft::default()
                },
                &keys,
            )
            .expect("add");
        assert!(stored.audience.starts_with("hofund-"));
        assert_eq!(stored.lifetime(), DEFAULT_TTL);
        let listed = rules.list().expect("list");
        assert_eq!(listed.len(), 1);
        rules.remove(&stored.id).expect("remove");
        assert!(rules.remove(&stored.id).is_err());
    }
}
