//! Reading the CLI's auth.json and building account secrets.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, unverified_claims};

/// How long before expiry the CLI should renew the token.
pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

const FILE_CAP: u64 = 1024 * 1024;
const TORN_TRIES: usize = 6;
const TORN_WAIT: Duration = Duration::from_millis(100);

/// One sign-in read from auth.json.
#[derive(Clone, Debug)]
pub(crate) struct Credential {
    /// Bearer token the backend accepts.
    pub key: SecretString,
    /// Account email when present.
    pub email: String,
    /// Stable account label: email, else user id, else token subject.
    pub who: String,
    /// When the token expires, in milliseconds since the epoch.
    pub expires_ms: u64,
}

/// A stored sign-in with its CLI home directory.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub home: String,
    pub credential: Credential,
}

impl Stored {
    /// Reads the account secret and parses it.
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Grok Build sign-in is stored for this account",
            ));
        };
        parse_secret(&secret)
    }

    /// Whether the token is still good past the refresh margin.
    pub(crate) fn fresh_at(&self, now: SystemTime) -> bool {
        expires_after(now, self.credential.expires_ms, REFRESH_MARGIN)
    }
}

/// Parses a stored secret blob.
pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let home = blob
        .text("home")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| PluginError::auth("the stored Grok sign-in has no CLI home"))?
        .to_owned();
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Grok sign-in has no access token"))?;
    let expires_ms = blob.number("expires_at").unwrap_or(0);
    let email = blob.text("email").unwrap_or("").to_owned();
    let who = blob
        .text("account_key")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| email.clone());
    Ok(Stored {
        home,
        credential: Credential {
            key: access,
            email,
            who,
            expires_ms,
        },
    })
}

/// Builds the secret written at sign-in or import.
pub(crate) fn secret_blob(home: &Path, credential: &Credential) -> SecretObject {
    let account_key = account_key(credential);
    SecretObject::new()
        .with_number("v", 1)
        .with_text("home", &path_text(home))
        .with_secret("access_token", &credential.key)
        .with_number("expires_at", credential.expires_ms)
        .with_text("email", &credential.email)
        .with_text("account_key", &account_key)
}

pub(crate) fn account_id(credential: &Credential) -> AccountId {
    account_id_from(&["grok", &account_key(credential)])
}

fn account_key(credential: &Credential) -> String {
    if !credential.who.is_empty() {
        return credential.who.clone();
    }
    credential.email.clone()
}

fn path_text(home: &Path) -> String {
    home.to_string_lossy().into_owned()
}

/// Reads the first credential with a token from `home/auth.json`.
pub(crate) fn read_sync(home: &Path) -> Result<Credential, ReadError> {
    let mut last = read_once(home);
    for _ in 1..TORN_TRIES {
        if !last.torn {
            break;
        }
        std::thread::sleep(TORN_WAIT);
        last = read_once(home);
    }
    if last.torn {
        return Err(ReadError::Torn(last.reason));
    }
    last.credential.ok_or(ReadError::Missing(last.reason))
}

/// Reads auth.json once.
struct ReadOnce {
    credential: Option<Credential>,
    torn: bool,
    reason: String,
}

fn read_once(home: &Path) -> ReadOnce {
    let path = home.join("auth.json");
    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(error) => {
            let reason = if error.kind() == std::io::ErrorKind::NotFound {
                format!("there is no {}", path.display())
            } else {
                format!("{} can't be read ({error})", path.display())
            };
            return ReadOnce {
                credential: None,
                torn: false,
                reason,
            };
        }
    };
    if !meta.is_file() || meta.len() > FILE_CAP {
        return ReadOnce {
            credential: None,
            torn: false,
            reason: format!("{} is not a readable sign-in file", path.display()),
        };
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return ReadOnce {
                credential: None,
                torn: false,
                reason: format!("{} can't be read ({error})", path.display()),
            };
        }
    };
    if text.trim().is_empty() {
        return ReadOnce {
            credential: None,
            torn: true,
            reason: format!("{} is empty", path.display()),
        };
    }
    let root: Value = match hofund_plugin_sdk::json::from_str(&text) {
        Ok(root) => root,
        Err(_) => {
            return ReadOnce {
                credential: None,
                torn: true,
                reason: format!("{} isn't JSON", path.display()),
            };
        }
    };
    let Some(map) = root.as_object() else {
        return ReadOnce {
            credential: None,
            torn: false,
            reason: format!("{} holds no sign-in", path.display()),
        };
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for key in keys {
        let Some(entry) = map.get(key).and_then(Value::as_object) else {
            continue;
        };
        let Some(token) = entry
            .get("key")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
        else {
            continue;
        };
        let email = entry
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let user_id = entry
            .get("user_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let sub = token_subject(token).unwrap_or_default();
        let who = if !email.is_empty() {
            email.clone()
        } else if !user_id.is_empty() {
            user_id
        } else {
            sub
        };
        let expires_ms = entry
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
            .unwrap_or(0);
        return ReadOnce {
            credential: Some(Credential {
                key: SecretString::from(token.to_owned()),
                email,
                who,
                expires_ms,
            }),
            torn: false,
            reason: String::new(),
        };
    }
    ReadOnce {
        credential: None,
        torn: false,
        reason: if map.is_empty() {
            format!("{} holds no sign-in", path.display())
        } else {
            format!(
                "{} has {}, none with a token",
                path.display(),
                map.keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    }
}

/// Why auth.json could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// The file was missing or had no token entry.
    Missing(String),
    /// The file looked mid-write.
    Torn(String),
}

impl ReadError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Missing(reason) | Self::Torn(reason) => reason,
        }
    }
}

pub(crate) fn expires_after(now: SystemTime, expires_ms: u64, margin: Duration) -> bool {
    let Ok(now_ms) = now.duration_since(UNIX_EPOCH) else {
        return false;
    };
    let now_ms = now_ms.as_millis() as u64;
    expires_ms.saturating_sub(now_ms) > margin.as_millis() as u64
}

fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    let parsed = hofund_plugin_sdk::time::parse_rfc3339(text)?;
    parsed
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

fn token_subject(token: &str) -> Option<String> {
    let claims = unverified_claims(&SecretString::from(token.to_owned())).ok()?;
    claims.get("sub").and_then(Value::as_str).map(str::to_owned)
}

/// Writes auth.json for agent sign-in switching.
pub(crate) fn write_auth(home: &Path, stored: &Stored) -> Result<(), PluginError> {
    let scope = "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828";
    let mut entry = Map::new();
    entry.insert(
        "key".to_owned(),
        Value::String(hofund_plugin_sdk::oauth::secret_text(&stored.credential.key).to_owned()),
    );
    if !stored.credential.email.is_empty() {
        entry.insert(
            "email".to_owned(),
            Value::String(stored.credential.email.clone()),
        );
    }
    entry.insert(
        "expires_at".to_owned(),
        Value::String(expires_rfc3339(stored.credential.expires_ms)),
    );
    let mut root = Map::new();
    root.insert(scope.to_owned(), Value::Object(entry));
    let text = Value::Object(root).to_string();
    std::fs::create_dir_all(home)
        .map_err(|_| PluginError::io("cannot create the Grok home directory"))?;
    write_private(&home.join("auth.json"), text.as_bytes())
}

fn expires_rfc3339(expires_ms: u64) -> String {
    let duration = Duration::from_millis(expires_ms);
    let time = UNIX_EPOCH + duration;
    hofund_plugin_sdk::time::format_rfc3339(time)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), PluginError> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|_| PluginError::io("cannot write auth.json"))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| PluginError::io("cannot restrict auth.json"))?;
        file.write_all(bytes)
            .map_err(|_| PluginError::io("cannot write auth.json"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).map_err(|_| PluginError::io("cannot write auth.json"))?;
    }
    Ok(())
}
