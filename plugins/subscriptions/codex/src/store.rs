//! Where Codex CLI keeps its ChatGPT sign-in: `auth.json`, macOS Keychain, or both.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hofund_plugin_api::{HostContext, PluginError};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::{Map, Value};

#[cfg(target_os = "macos")]
use crate::security_io::{
    SecurityExit, append_capped, classify_security_exit, decode_keychain_password,
};

pub(crate) const KEYCHAIN_SERVICE: &str = "Codex Auth";

const FILE_CAP: u64 = 1024 * 1024;
const KEYCHAIN_TTL: Duration = Duration::from_secs(10);
const KEYCHAIN_REFUSED: Duration = Duration::from_secs(10 * 60);

#[cfg(target_os = "macos")]
const SECURITY: &str = "/usr/bin/security";

/// Where `config.toml` says Codex keeps credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreSetting {
    pub mode: StoreMode,
    pub secret_auth_storage: bool,
}

/// Values Codex accepts for `cli_auth_credentials_store`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StoreMode {
    File,
    Keyring,
    Auto,
    Ephemeral,
}

impl StoreSetting {
    pub(crate) fn keychain_item(&self) -> bool {
        (self.mode == StoreMode::Keyring || self.mode == StoreMode::Auto)
            && !self.secret_auth_storage
            && keychain_available()
    }
}

/// Whether a sign-in was read from file or keychain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StoreLocation {
    pub home: PathBuf,
    pub keychain: bool,
}

impl StoreLocation {
    fn auth_path(&self) -> PathBuf {
        self.home.join("auth.json")
    }
}

/// Keychain access for tests and macOS production.
pub(crate) trait CodexKeychain: Send + Sync {
    fn find(&self, service: &str, account: &str) -> Result<Vec<u8>, KeychainError>;
    fn stat(&self, service: &str, account: &str) -> Result<String, KeychainError>;
    fn put(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), KeychainError>;
}

#[derive(Debug, Clone)]
pub(crate) enum KeychainError {
    Missing,
    #[allow(dead_code)] // macOS `security` exit 51/128
    Refused,
    Unreadable,
}

/// In-memory keychain for unit tests.
#[derive(Default)]
pub(crate) struct FakeKeychain {
    inner: Mutex<FakeKeychainState>,
}

#[derive(Default)]
struct FakeKeychainState {
    items: HashMap<String, Vec<u8>>,
    find_err: Option<KeychainError>,
    put_err: Option<KeychainError>,
}

impl FakeKeychain {
    #[allow(dead_code)]
    pub(crate) fn set(&self, account: &str, bytes: &[u8]) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.items.insert(account.to_owned(), bytes.to_vec());
    }
}

impl CodexKeychain for FakeKeychain {
    fn find(&self, service: &str, account: &str) -> Result<Vec<u8>, KeychainError> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if service != KEYCHAIN_SERVICE {
            return Err(KeychainError::Unreadable);
        }
        if let Some(err) = &guard.find_err {
            return Err(err.clone());
        }
        guard
            .items
            .get(account)
            .cloned()
            .ok_or(KeychainError::Missing)
    }

    fn stat(&self, service: &str, account: &str) -> Result<String, KeychainError> {
        let bytes = self.find(service, account)?;
        let sum = sha256(&bytes);
        Ok(hex8(&sum[..8]))
    }

    fn put(&self, service: &str, account: &str, secret: &[u8]) -> Result<(), KeychainError> {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if service != KEYCHAIN_SERVICE {
            return Err(KeychainError::Unreadable);
        }
        if let Some(err) = &guard.put_err {
            return Err(err.clone());
        }
        guard.items.insert(account.to_owned(), secret.to_vec());
        Ok(())
    }
}

struct KeychainCache {
    account: String,
    bytes: Option<Vec<u8>>,
    err: Option<KeychainError>,
    mdat: String,
    at: Instant,
    checked: Instant,
}

impl Default for KeychainCache {
    fn default() -> Self {
        Self {
            account: String::new(),
            bytes: None,
            err: None,
            mdat: String::new(),
            at: Instant::now(),
            checked: Instant::now(),
        }
    }
}

pub(crate) struct StoreReader<'a> {
    host: Option<&'a HostContext>,
    skip_keychain: bool,
    fake: Option<&'a FakeKeychain>,
    cache: Mutex<KeychainCache>,
}

impl<'a> StoreReader<'a> {
    pub(crate) fn new(
        host: Option<&'a HostContext>,
        skip_keychain: bool,
        fake: Option<&'a FakeKeychain>,
    ) -> Self {
        Self {
            host,
            skip_keychain,
            fake,
            cache: Mutex::new(KeychainCache::default()),
        }
    }

    fn uses_keychain(&self, setting: &StoreSetting) -> bool {
        if self.skip_keychain {
            return false;
        }
        if self.fake.is_some() {
            return matches!(setting.mode, StoreMode::Keyring | StoreMode::Auto)
                && !setting.secret_auth_storage;
        }
        setting.keychain_item()
    }

    pub(crate) async fn read_login(
        &self,
        home: &Path,
        fresh: bool,
    ) -> Result<(Vec<u8>, StoreLocation), StoreError> {
        let config = home.join("config.toml");
        let loc = StoreLocation {
            home: home.to_path_buf(),
            keychain: false,
        };
        let setting = match read_store_setting(home) {
            Ok(setting) => setting,
            Err(StoreError::ConfigParse(_)) => {
                return read_auth_file(&loc.auth_path()).map(|bytes| (bytes, loc));
            }
            Err(error) => return Err(error),
        };
        match setting.mode {
            StoreMode::Ephemeral => Err(StoreError::Unreadable {
                setting,
                config,
                cause: None,
            }),
            StoreMode::Keyring if self.uses_keychain(&setting) => {
                match self.find_keychain(home, fresh).await {
                    Ok(bytes) => Ok((
                        bytes,
                        StoreLocation {
                            keychain: true,
                            ..loc
                        },
                    )),
                    Err(KeychainError::Missing) => Err(StoreError::SignedOut),
                    Err(cause) => Err(StoreError::Unreadable {
                        setting,
                        config,
                        cause: Some(cause),
                    }),
                }
            }
            StoreMode::Auto if self.uses_keychain(&setting) => {
                match self.find_keychain(home, fresh).await {
                    Ok(bytes) => {
                        return Ok((
                            bytes,
                            StoreLocation {
                                keychain: true,
                                ..loc
                            },
                        ));
                    }
                    Err(KeychainError::Missing) => {}
                    Err(cause) if !fresh => match read_auth_file(&loc.auth_path()) {
                        Ok(bytes) => return Ok((bytes, loc)),
                        Err(StoreError::SignedOut) | Err(StoreError::InvalidJson) => {
                            return Err(StoreError::Unreadable {
                                setting,
                                config,
                                cause: Some(cause),
                            });
                        }
                        Err(error) => return Err(error),
                    },
                    Err(_) => {}
                }
                read_auth_file(&loc.auth_path()).map(|bytes| (bytes, loc))
            }
            StoreMode::Auto => read_auth_file(&loc.auth_path()).map(|bytes| (bytes, loc)),
            StoreMode::Keyring => Err(StoreError::Unreadable {
                setting,
                config,
                cause: None,
            }),
            StoreMode::File => read_auth_file(&loc.auth_path()).map(|bytes| (bytes, loc)),
        }
    }

    pub(crate) async fn save_login(&self, home: &Path, auth: &[u8]) -> Result<(), StoreError> {
        let _ = compact_json(auth)?;
        let setting = read_store_setting(home)?;
        let config = home.join("config.toml");
        if setting.mode == StoreMode::Ephemeral {
            return Err(StoreError::Unreadable {
                setting,
                config,
                cause: None,
            });
        }
        let mut loc = StoreLocation {
            home: home.to_path_buf(),
            keychain: false,
        };
        if self.uses_keychain(&setting) {
            let kerr = self.find_keychain(home, true).await;
            if let Err(cause) = &kerr
                && !matches!(cause, KeychainError::Missing)
            {
                return Err(StoreError::Unreadable {
                    setting,
                    config,
                    cause: Some(cause.clone()),
                });
            }
            loc.keychain = true;
            match self.write_location(&loc, auth).await {
                Ok(()) => return Ok(()),
                Err(error) if setting.mode == StoreMode::Keyring || kerr.is_ok() => {
                    return Err(error);
                }
                Err(_) => loc.keychain = false,
            }
        } else if setting.mode == StoreMode::Keyring
            || (setting.mode == StoreMode::Auto && !loc.auth_path().is_file())
        {
            return Err(StoreError::Unreadable {
                setting,
                config,
                cause: None,
            });
        }
        self.write_location(&loc, auth).await
    }

    async fn write_location(&self, loc: &StoreLocation, auth: &[u8]) -> Result<(), StoreError> {
        if loc.keychain {
            let account = keychain_account(&loc.home);
            let compact = compact_json(auth)?;
            if let Some(fake) = self.fake {
                fake.put(KEYCHAIN_SERVICE, &account, &compact)
                    .map_err(|_| StoreError::KeychainWrite)?;
            } else if let Some(host) = self.host {
                put_keychain(host, &account, &compact).await?;
            } else {
                return Err(StoreError::KeychainWrite);
            }
            return Ok(());
        }
        let mut body = auth.to_vec();
        if body.last() != Some(&b'\n') {
            body.push(b'\n');
        }
        write_atomic_private(&loc.auth_path(), &body).map_err(|_| StoreError::FileWrite)
    }

    async fn find_keychain(&self, home: &Path, fresh: bool) -> Result<Vec<u8>, KeychainError> {
        let account = keychain_account(home);
        if let Some(fake) = self.fake {
            return fake.find(KEYCHAIN_SERVICE, &account);
        }
        if self.skip_keychain || !keychain_available() {
            return Err(KeychainError::Missing);
        }
        let now = Instant::now();
        {
            let cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cache.account == account {
                if let Some(err) = &cache.err
                    && !fresh
                    && now.duration_since(cache.at) < KEYCHAIN_REFUSED
                {
                    return Err(err.clone());
                } else if let Some(bytes) = &cache.bytes
                    && !fresh
                    && now.duration_since(cache.checked) < KEYCHAIN_TTL
                {
                    return Ok(bytes.clone());
                }
            }
        }
        let bytes = self.read_keychain_live(&account).await?;
        let mdat = self.keychain_stat(&account).await.unwrap_or_default();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.account = account;
        cache.bytes = Some(bytes.clone());
        cache.err = None;
        cache.mdat = mdat;
        cache.at = now;
        cache.checked = now;
        Ok(bytes)
    }

    async fn read_keychain_live(&self, account: &str) -> Result<Vec<u8>, KeychainError> {
        let host = self.host.ok_or(KeychainError::Unreadable)?;
        let bytes = find_keychain(host, account).await?;
        validate_keychain_json(&bytes)?;
        Ok(bytes)
    }

    async fn keychain_stat(&self, account: &str) -> Result<String, KeychainError> {
        if let Some(fake) = self.fake {
            return fake.stat(KEYCHAIN_SERVICE, account);
        }
        let host = self.host.ok_or(KeychainError::Unreadable)?;
        stat_keychain(host, account).await
    }
}

#[derive(Debug)]
pub(crate) enum StoreError {
    SignedOut,
    #[allow(dead_code)]
    Unreadable {
        setting: StoreSetting,
        config: PathBuf,
        cause: Option<KeychainError>,
    },
    FileWrite,
    KeychainWrite,
    InvalidJson,
    ConfigParse(String),
}

pub(crate) fn read_store_setting(home: &Path) -> Result<StoreSetting, StoreError> {
    let path = home.join("config.toml");
    let default = StoreSetting {
        mode: StoreMode::File,
        secret_auth_storage: secret_auth_storage_default(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(default),
        Err(_) => return Err(StoreError::ConfigParse(path.display().to_string())),
    };
    parse_store_setting(&text, &path)
}

fn parse_store_setting(text: &str, path: &Path) -> Result<StoreSetting, StoreError> {
    let (top, tables) = parse_toml(text);
    let mut mode = top
        .get("cli_auth_credentials_store")
        .and_then(Value::as_str)
        .unwrap_or("");
    if mode.is_empty() {
        mode = "file";
    }
    let mode = match mode {
        "file" => StoreMode::File,
        "keyring" => StoreMode::Keyring,
        "auto" => StoreMode::Auto,
        "ephemeral" => StoreMode::Ephemeral,
        other => {
            return Err(StoreError::ConfigParse(format!(
                "cli_auth_credentials_store = {other:?} in {} isn't one Codex knows",
                path.display()
            )));
        }
    };
    let mut secret_auth_storage = secret_auth_storage_default();
    if let Some(features) = tables.get("features")
        && let Some(value) = features.get("secret_auth_storage").and_then(Value::as_str)
    {
        secret_auth_storage = matches!(value, "true" | "1");
    }
    Ok(StoreSetting {
        mode,
        secret_auth_storage,
    })
}

fn secret_auth_storage_default() -> bool {
    cfg!(target_os = "windows")
}

fn keychain_available() -> bool {
    cfg!(target_os = "macos")
}

pub(crate) fn keychain_account(home: &Path) -> String {
    let canonical = canonical_home(home);
    let sum = sha256(canonical.as_bytes());
    format!("cli|{}", hex16(&sum[..8]))
}

fn canonical_home(home: &Path) -> String {
    let abs = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    abs.to_string_lossy().into_owned()
}

fn hex16(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(16), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn hex8(bytes: &[u8]) -> String {
    hex16(bytes)
}

fn read_auth_file(path: &Path) -> Result<Vec<u8>, StoreError> {
    let meta = std::fs::metadata(path).map_err(|_| StoreError::SignedOut)?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return Err(StoreError::SignedOut);
    }
    let bytes = std::fs::read(path).map_err(|_| StoreError::SignedOut)?;
    if !is_valid_auth_json(&bytes) {
        return Err(StoreError::InvalidJson);
    }
    Ok(bytes)
}

pub(crate) fn is_valid_auth_json(bytes: &[u8]) -> bool {
    let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(bytes) else {
        return false;
    };
    value.is_object()
}

pub(crate) fn auth_mode_apikey(bytes: &[u8]) -> bool {
    let Ok(value) = hofund_plugin_sdk::json::from_slice::<Value>(bytes) else {
        return false;
    };
    value
        .get("auth_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode == "apikey")
}

fn validate_keychain_json(bytes: &[u8]) -> Result<(), KeychainError> {
    if !is_valid_auth_json(bytes) {
        Err(KeychainError::Unreadable)
    } else {
        Ok(())
    }
}

fn compact_json(auth: &[u8]) -> Result<Vec<u8>, StoreError> {
    let value: Value =
        hofund_plugin_sdk::json::from_slice(auth).map_err(|_| StoreError::InvalidJson)?;
    let text = value.to_string();
    Ok(text.into_bytes())
}

pub(crate) fn write_atomic_private(path: &Path, body: &[u8]) -> Result<(), PluginError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|_| PluginError::io("cannot create the Codex home directory"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).ok();
        }
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.json");
    let unique = hofund_plugin_sdk::uuid::v4()
        .map_err(|_| PluginError::io("cannot write Codex auth.json"))?;
    let temporary = path.with_file_name(format!("{file_name}.{unique}.tmp"));
    write_private_once(&temporary, body)?;
    std::fs::rename(&temporary, path).map_err(|_| PluginError::io("cannot write Codex auth.json"))
}

fn write_private_once(path: &Path, body: &[u8]) -> Result<(), PluginError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let failed = |_| PluginError::io("cannot write Codex auth.json");
    let mut file = options.open(path).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }
    file.write_all(body).map_err(failed)?;
    file.sync_all().map_err(failed)
}

pub(crate) fn merge_auth_tokens(raw: &[u8], patch: &AuthPatch) -> Result<Vec<u8>, PluginError> {
    let mut root: Map<String, Value> = hofund_plugin_sdk::json::from_slice(raw)
        .map_err(|_| PluginError::decode("Codex auth.json is not JSON"))?;
    let tokens = root
        .entry("tokens".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(tokens) = tokens.as_object_mut() else {
        return Err(PluginError::decode("Codex auth.json has no tokens object"));
    };
    if let Some(access) = &patch.access_token {
        tokens.insert("access_token".to_owned(), Value::String(access.clone()));
    }
    if let Some(refresh) = &patch.refresh_token {
        tokens.insert("refresh_token".to_owned(), Value::String(refresh.clone()));
    }
    if let Some(id_token) = &patch.id_token {
        tokens.insert("id_token".to_owned(), Value::String(id_token.clone()));
    }
    if patch.chatgpt_mode {
        root.insert("auth_mode".to_owned(), Value::String("chatgpt".to_owned()));
    }
    Ok(Value::Object(root).to_string().into_bytes())
}

pub(crate) struct AuthPatch {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub chatgpt_mode: bool,
}

pub(crate) fn parse_toml(text: &str) -> (Map<String, Value>, HashMap<String, Map<String, Value>>) {
    let mut top = Map::new();
    let mut tables: HashMap<String, Map<String, Value>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            current = Some(line.trim_matches(['[', ']']).trim().to_owned());
            tables.entry(current.clone().unwrap()).or_default();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = trim_quotes(key.trim());
        let value = trim_toml_value(value);
        if let Some(table) = &current {
            tables
                .entry(table.clone())
                .or_default()
                .insert(key.to_owned(), Value::String(value.to_owned()));
        } else {
            top.insert(key.to_owned(), Value::String(value.to_owned()));
        }
    }
    (top, tables)
}

fn trim_toml_value(raw: &str) -> &str {
    let raw = raw.trim();
    if raw.starts_with('"') {
        let mut end = 1;
        while end < raw.len() {
            if raw.as_bytes()[end] == b'"' && raw.as_bytes()[end - 1] != b'\\' {
                return trim_quotes(&raw[..=end]);
            }
            end += 1;
        }
        return trim_quotes(raw);
    }
    if let Some(rest) = raw.strip_prefix('\'')
        && let Some(end) = rest.find('\'')
    {
        return trim_quotes(&raw[..=end + 1]);
    }
    if let Some(idx) = raw.find(" #") {
        return raw[..idx].trim();
    }
    raw
}

fn trim_quotes(raw: &str) -> &str {
    let raw = raw.trim();
    if raw.len() >= 2 && (raw.starts_with('"') || raw.starts_with('\'')) {
        let quote = raw.as_bytes()[0] as char;
        if raw.ends_with(quote) {
            return &raw[1..raw.len() - 1];
        }
    }
    raw
}

#[cfg(target_os = "macos")]
async fn find_keychain(host: &HostContext, account: &str) -> Result<Vec<u8>, KeychainError> {
    let output = spawn_security(
        host,
        &[
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
            "-w",
        ],
    )
    .await?;
    if output.is_empty() {
        return Err(KeychainError::Unreadable);
    }
    Ok(output)
}

#[cfg(not(target_os = "macos"))]
async fn find_keychain(_host: &HostContext, _account: &str) -> Result<Vec<u8>, KeychainError> {
    Err(KeychainError::Missing)
}

#[cfg(target_os = "macos")]
async fn stat_keychain(host: &HostContext, account: &str) -> Result<String, KeychainError> {
    let output = spawn_security(
        host,
        &[
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
        ],
    )
    .await?;
    keychain_mdat(&output).ok_or(KeychainError::Unreadable)
}

#[cfg(not(target_os = "macos"))]
async fn stat_keychain(_host: &HostContext, _account: &str) -> Result<String, KeychainError> {
    Err(KeychainError::Missing)
}

#[cfg(target_os = "macos")]
async fn put_keychain(host: &HostContext, account: &str, secret: &[u8]) -> Result<(), StoreError> {
    let text = std::str::from_utf8(secret).map_err(|_| StoreError::KeychainWrite)?;
    spawn_security(
        host,
        &[
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            account,
            "-w",
            text,
        ],
    )
    .await
    .map(|_| ())
    .map_err(|_| StoreError::KeychainWrite)
}

#[cfg(not(target_os = "macos"))]
async fn put_keychain(
    _host: &HostContext,
    _account: &str,
    _secret: &[u8],
) -> Result<(), StoreError> {
    Err(StoreError::KeychainWrite)
}

#[cfg(target_os = "macos")]
async fn spawn_security(host: &HostContext, args: &[&str]) -> Result<Vec<u8>, KeychainError> {
    use hofund_plugin_api::SpawnRequest;
    let request = SpawnRequest::new(
        PathBuf::from(SECURITY),
        args.iter()
            .map(|arg| std::ffi::OsString::from(*arg))
            .collect(),
        Vec::new(),
        std::env::temp_dir(),
    )
    .map_err(|_| KeychainError::Unreadable)?;
    let child = host
        .spawn_process()
        .spawn_piped(&request)
        .await
        .map_err(|_| KeychainError::Unreadable)?;
    let stdout = read_piped_stdout(host, child).await?;
    Ok(decode_keychain_password(&stdout))
}

#[cfg(target_os = "macos")]
async fn read_piped_stdout(
    host: &HostContext,
    child: hofund_plugin_api::PipedChild,
) -> Result<Vec<u8>, KeychainError> {
    let mut stream = child.stdout().map_err(|_| KeychainError::Unreadable)?;
    let mut out = Vec::new();
    while let Some(chunk) = stream.next_chunk().await {
        let chunk = chunk.map_err(|_| KeychainError::Unreadable)?;
        append_capped(&mut out, &chunk).map_err(|_| KeychainError::Unreadable)?;
    }
    let code = piped_exit_code(child).await?;
    match classify_security_exit(code) {
        SecurityExit::Ok => Ok(out),
        SecurityExit::Missing => Err(KeychainError::Missing),
        SecurityExit::Refused => Err(KeychainError::Refused),
        SecurityExit::Failed => Err(KeychainError::Unreadable),
    }
}

/// Waits for exit after stdout EOF via [`PipedChild::exit_code`] (kernel sup/r-8a).
#[cfg(target_os = "macos")]
async fn piped_exit_code(child: hofund_plugin_api::PipedChild) -> Result<i32, KeychainError> {
    child
        .exit_code()
        .await
        .map_err(|_| KeychainError::Unreadable)
}

#[cfg(target_os = "macos")]
fn keychain_mdat(output: &[u8]) -> Option<String> {
    for line in output.split(|byte| *byte == b'\n') {
        let line = std::str::from_utf8(line).ok()?;
        if let Some(rest) = line.strip_prefix("\"mdat\"<timedate>=") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::json::Value;

    use super::*;

    #[test]
    fn r47b_r1_toml_inline_comment_after_quoted_value() {
        let (top, _) = parse_toml(r#"cli_auth_credentials_store = "keyring" # comment"#);
        assert_eq!(
            top.get("cli_auth_credentials_store")
                .and_then(Value::as_str),
            Some("keyring")
        );
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\" # comment\n",
        )
        .expect("config");
        let setting = read_store_setting(&home).expect("parse");
        assert!(matches!(setting.mode, StoreMode::Keyring));
    }

    #[test]
    fn r47b_keychain_account_matches_codex_store_key() {
        let dir = tempfile::tempdir().expect("dir");
        let account = keychain_account(dir.path());
        assert!(account.starts_with("cli|"));
        assert_eq!(account.len(), "cli|".len() + 16);
    }

    #[test]
    fn r47b_non_macos_keychain_read_is_noop() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .expect("config");
        let reader = StoreReader::new(None, false, None);
        let error = hofund_kit_subscription::block_on(reader.read_login(&home, false))
            .expect_err("keyring off-platform");
        assert!(matches!(error, StoreError::Unreadable { cause: None, .. }));
    }

    #[test]
    fn r47b_fake_keychain_round_trip() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .expect("config");
        let fake = FakeKeychain::default();
        let auth = br#"{"auth_mode":"chatgpt","tokens":{"access_token":"a","refresh_token":"r","id_token":"i"}}"#;
        fake.set(&keychain_account(&home), auth);
        let reader = StoreReader::new(None, false, Some(&fake));
        let (read, loc) =
            hofund_kit_subscription::block_on(reader.read_login(&home, false)).expect("read");
        assert!(loc.keychain);
        assert_eq!(read, auth);
    }

    #[test]
    fn r47b_auto_falls_back_to_auth_json() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"auto\"\n",
        )
        .expect("config");
        let auth = br#"{"tokens":{"access_token":"a","refresh_token":"r","id_token":"i"}}"#;
        write_atomic_private(&home.join("auth.json"), auth).expect("write");
        let reader = StoreReader::new(None, true, None);
        let (read, loc) =
            hofund_kit_subscription::block_on(reader.read_login(&home, false)).expect("read");
        assert!(!loc.keychain);
        assert_eq!(read, auth);
    }

    #[test]
    fn r47b_atomic_write_leaves_valid_json() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("auth.json");
        write_atomic_private(&path, br#"{"tokens":{"access_token":"x"}}"#).expect("write");
        let read = std::fs::read(&path).expect("read");
        assert!(is_valid_auth_json(&read));
    }
}
