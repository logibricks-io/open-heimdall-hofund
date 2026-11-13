//! Kiro IDE / CLI shaped token files and stored account secrets.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, AgentSignIn, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, same_secret, secret_text};
use hofund_plugin_sdk::time::parse_rfc3339;

fn hash_hex(data: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    data.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) const REFRESH_MARGIN: Duration = Duration::from_secs(120);
const FILE_CAP: u64 = 64 * 1024;
pub(crate) const TOKEN_FILE: &str = "kiro-auth-token.json";

/// Profile ARN the reference uses for AWS Builder ID sign-ins without their own profile.
pub(crate) const BUILDER_ID_PROFILE: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

/// Agent names and home declaration shared with the subscription spec and tests.
pub(crate) const AGENT_SIGN_IN: AgentSignIn =
    AgentSignIn::new(&["kiro"], "KIRO_IDE_CACHE", ".aws/sso/cache");

#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires_at: Option<std::time::SystemTime>,
    pub region: String,
    pub profile_arn: String,
    pub label: String,
    pub method: String,
    pub provider: String,
    pub client_id: String,
    pub client_secret: SecretString,
    pub client_id_hash: String,
    pub token_url: String,
    pub ide_token_path: String,
    pub db_key: String,
    pub cli_db_path: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
}

impl Stored {
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no Kiro sign-in is stored for this account",
            ));
        };
        parse_secret(&secret)
    }

    pub(crate) fn fresh_at(&self, now: std::time::SystemTime) -> bool {
        match self.credential.expires_at {
            None => true,
            Some(at) => at > now + REFRESH_MARGIN,
        }
    }
}

impl Credential {
    pub(crate) fn fresh_at(&self, now: std::time::SystemTime) -> bool {
        match self.expires_at {
            None => true,
            Some(at) => at > now + REFRESH_MARGIN,
        }
    }
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored Kiro sign-in has no access token"))?;
    let refresh = blob
        .secret("refresh_token")
        .unwrap_or_else(|| SecretString::from(String::new()));
    let region = blob
        .text("region")
        .filter(|value| !value.is_empty())
        .unwrap_or("us-east-1")
        .to_owned();
    let profile_arn = blob.text("profile_arn").unwrap_or("").to_owned();
    let label = blob
        .text("label")
        .filter(|value| !value.is_empty())
        .unwrap_or("Kiro")
        .to_owned();
    let method = blob.text("method").unwrap_or("social").to_owned();
    let provider = blob.text("provider").unwrap_or("").to_owned();
    let client_id = blob.text("client_id").unwrap_or("").to_owned();
    let client_secret = blob
        .secret("client_secret")
        .unwrap_or_else(|| SecretString::from(String::new()));
    let client_id_hash = blob.text("client_id_hash").unwrap_or("").to_owned();
    let token_url = blob.text("token_url").unwrap_or("").to_owned();
    let ide_token_path = blob.text("ide_token_path").unwrap_or("").to_owned();
    let db_key = blob.text("db_key").unwrap_or("").to_owned();
    let cli_db_path = blob.text("cli_db_path").unwrap_or("").to_owned();
    let expires_at = blob.number("expires_at").and_then(|secs| {
        if secs == 0 {
            None
        } else {
            UNIX_EPOCH.checked_add(Duration::from_secs(secs))
        }
    });
    Ok(Stored {
        credential: Credential {
            access,
            refresh,
            expires_at,
            region,
            profile_arn,
            label,
            method,
            provider,
            client_id,
            client_secret,
            client_id_hash,
            token_url,
            ide_token_path,
            db_key,
            cli_db_path,
        },
    })
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    let mut blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("access_token", &credential.access)
        .with_secret("refresh_token", &credential.refresh)
        .with_text("region", &credential.region)
        .with_text("profile_arn", &credential.profile_arn)
        .with_text("label", &credential.label)
        .with_text("method", &credential.method)
        .with_text("provider", &credential.provider)
        .with_text("client_id", &credential.client_id)
        .with_secret("client_secret", &credential.client_secret)
        .with_text("client_id_hash", &credential.client_id_hash)
        .with_text("token_url", &credential.token_url);
    if !credential.ide_token_path.is_empty() {
        blob = blob.with_text("ide_token_path", &credential.ide_token_path);
    }
    if !credential.db_key.is_empty() {
        blob = blob
            .with_text("db_key", &credential.db_key)
            .with_text("cli_db_path", &credential.cli_db_path);
    }
    if let Some(at) = credential
        .expires_at
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    {
        blob = blob.with_number("expires_at", at.as_secs());
    }
    blob
}

pub(crate) fn agent_account_id(home: &Path) -> AccountId {
    account_id_from(&[
        "kiro",
        "agent",
        &hash_hex(home.to_string_lossy().as_bytes()),
    ])
}

pub(crate) fn account_id_for_home(home: &Path, credential: &Credential) -> AccountId {
    let _ = credential;
    agent_account_id(home)
}

pub(crate) fn browser_account_id(credential: &Credential) -> AccountId {
    account_id_from(&[
        "kiro",
        "browser",
        &credential.label,
        &credential.provider.to_ascii_lowercase(),
    ])
}

/// Resolves the agent home directory the same way the daemon does for [`AgentSignIn`].
pub(crate) fn resolve_agent_home(decl: &AgentSignIn) -> Result<PathBuf, PluginError> {
    let home_env = std::env::var(decl.home_env()).ok();
    let home = std::env::var("HOME").ok();
    resolve_agent_home_from(home_env.as_deref(), home.as_deref(), decl)
}

pub(crate) fn resolve_agent_home_from(
    home_env: Option<&str>,
    home: Option<&str>,
    decl: &AgentSignIn,
) -> Result<PathBuf, PluginError> {
    if let Some(value) = home_env.filter(|text| !text.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let home = home
        .ok_or_else(|| PluginError::io("HOME is not set; set KIRO_IDE_CACHE to the IDE cache"))?;
    Ok(PathBuf::from(home).join(decl.home_dir()))
}

pub(crate) fn default_cache_dir() -> PathBuf {
    resolve_agent_home(&AGENT_SIGN_IN).unwrap_or_else(|_| PathBuf::from(".aws/sso/cache"))
}

pub(crate) fn discover_token_path() -> PathBuf {
    default_cache_dir().join(TOKEN_FILE)
}

pub(crate) fn token_path_in_home(home: &Path) -> PathBuf {
    home.join(TOKEN_FILE)
}

pub(crate) fn read_token_file(path: &Path) -> Result<Credential, PluginError> {
    let meta = std::fs::metadata(path)
        .map_err(|_| PluginError::auth("Kiro has no token file at that path"))?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return Err(PluginError::auth("Kiro token file is missing or too large"));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| PluginError::auth("Kiro token file could not be read"))?;
    let cache_dir = path.parent().unwrap_or_else(|| Path::new("."));
    parse_token_json(&text, cache_dir)
}

pub(crate) fn parse_token_json(text: &str, cache_dir: &Path) -> Result<Credential, PluginError> {
    let root: Value =
        json::from_str(text).map_err(|_| PluginError::decode("Kiro token file is not JSON"))?;
    let access = field_secret(&root, &["accessToken", "access_token"])?;
    let refresh = field_secret(&root, &["refreshToken", "refresh_token"])
        .unwrap_or_else(|_| SecretString::from(String::new()));
    let region = root
        .get("region")
        .and_then(Value::as_str)
        .unwrap_or("us-east-1")
        .to_owned();
    let profile_arn = root
        .get("profileArn")
        .or_else(|| root.get("profile_arn"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let label = root
        .get("email")
        .or_else(|| root.get("userEmail"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "Kiro".to_owned());
    let auth_method = root.get("authMethod").and_then(Value::as_str).unwrap_or("");
    let provider = root
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let client_id_hash = root
        .get("clientIdHash")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut client_id = root
        .get("clientId")
        .or_else(|| root.get("client_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut client_secret = root
        .get("clientSecret")
        .or_else(|| root.get("client_secret"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| SecretString::from(text.to_owned()))
        .unwrap_or_else(|| SecretString::from(String::new()));
    if client_id.is_empty()
        && !client_id_hash.is_empty()
        && let Some((id, secret)) = read_registration(cache_dir, &client_id_hash)?
    {
        client_id = id;
        client_secret = secret;
    }
    let method = if auth_method.eq_ignore_ascii_case("social") || client_id.is_empty() {
        "social".to_owned()
    } else if auth_method.eq_ignore_ascii_case("idc") {
        "idc".to_owned()
    } else if auth_method.eq_ignore_ascii_case("external-idp") {
        "external-idp".to_owned()
    } else if auth_method.eq_ignore_ascii_case("apikey") {
        "apikey".to_owned()
    } else {
        "social".to_owned()
    };
    let token_url = root
        .get("tokenUrl")
        .or_else(|| root.get("token_url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let expires_at = root
        .get("expiresAt")
        .or_else(|| root.get("expires_at"))
        .and_then(Value::as_str)
        .and_then(parse_rfc3339);
    Ok(Credential {
        access,
        refresh,
        expires_at,
        region,
        profile_arn,
        label,
        method,
        provider,
        client_id,
        client_secret,
        client_id_hash,
        token_url,
        ide_token_path: String::new(),
        db_key: String::new(),
        cli_db_path: String::new(),
    })
}

/// Profile ARN sent on Kiro runtime and usage calls.
#[must_use]
pub(crate) fn effective_profile_arn(credential: &Credential) -> &str {
    if !credential.profile_arn.is_empty() {
        return credential.profile_arn.as_str();
    }
    if credential.provider.eq_ignore_ascii_case("BuilderId") {
        return BUILDER_ID_PROFILE;
    }
    ""
}

fn read_registration(
    cache_dir: &Path,
    hash: &str,
) -> Result<Option<(String, SecretString)>, PluginError> {
    let path = cache_dir.join(format!("{hash}.json"));
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let root: Value = json::from_str(&text)
        .map_err(|_| PluginError::decode("Kiro client registration is not JSON"))?;
    let id = root
        .get("clientId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let secret = root
        .get("clientSecret")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::from(value.to_owned()));
    Ok(match (id, secret) {
        (Some(id), Some(secret)) => Some((id, secret)),
        _ => None,
    })
}

fn field_secret(root: &Value, keys: &[&str]) -> Result<SecretString, PluginError> {
    for key in keys {
        if let Some(text) = root
            .get(*key)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            return Ok(SecretString::from(text.to_owned()));
        }
    }
    Err(PluginError::auth("Kiro token file has no access token"))
}

pub(crate) fn kiro_region(profile_arn: &str, signed_in_region: &str) -> String {
    let parts: Vec<&str> = profile_arn.split(':').collect();
    if parts.len() > 4 && !parts[3].is_empty() {
        return parts[3].to_owned();
    }
    if signed_in_region.starts_with("eu-") {
        return "eu-central-1".to_owned();
    }
    "us-east-1".to_owned()
}

pub(crate) fn percent_encode_query(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn write_token_file(path: &Path, credential: &Credential) -> Result<(), PluginError> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let expires = credential
        .expires_at
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|d| hofund_plugin_sdk::time::format_rfc3339(UNIX_EPOCH + d))
        .unwrap_or_default();
    let mut object = read_existing_object(path);
    if let Value::Object(map) = &mut object {
        map.insert(
            "accessToken".to_owned(),
            Value::String(secret_text(&credential.access).to_owned()),
        );
        map.insert(
            "refreshToken".to_owned(),
            Value::String(secret_text(&credential.refresh).to_owned()),
        );
        map.insert("expiresAt".to_owned(), Value::String(expires));
        map.insert(
            "region".to_owned(),
            Value::String(credential.region.clone()),
        );
        map.insert(
            "profileArn".to_owned(),
            Value::String(credential.profile_arn.clone()),
        );
        if !credential.auth_method_field().is_empty() {
            map.insert(
                "authMethod".to_owned(),
                Value::String(credential.auth_method_field()),
            );
        }
        if !credential.provider.is_empty() {
            map.insert(
                "provider".to_owned(),
                Value::String(credential.provider.clone()),
            );
        }
        if !credential.client_id_hash.is_empty() {
            map.insert(
                "clientIdHash".to_owned(),
                Value::String(credential.client_id_hash.clone()),
            );
        }
        if !credential.client_id.is_empty() {
            map.insert(
                "clientId".to_owned(),
                Value::String(credential.client_id.clone()),
            );
        }
        if !secret_text(&credential.client_secret).is_empty() {
            map.insert(
                "clientSecret".to_owned(),
                Value::String(secret_text(&credential.client_secret).to_owned()),
            );
        }
        if credential.label != "Kiro" {
            map.insert("email".to_owned(), Value::String(credential.label.clone()));
        }
    }
    let body =
        json::to_vec(&object).map_err(|_| PluginError::io("cannot encode the Kiro token file"))?;
    write_atomic(path, &body)
}

/// Writes only access, refresh, and expiry back to the IDE token file when it
/// still holds the refresh token that was used for refresh (compare-and-swap).
pub(crate) fn write_token_refresh_fields(
    path: &Path,
    access: &SecretString,
    refresh: &SecretString,
    expires_at: Option<std::time::SystemTime>,
    refresh_used: &SecretString,
) -> Result<(), PluginError> {
    let Some(on_disk) = refresh_token_in_object(path) else {
        return Ok(());
    };
    if !same_secret(&on_disk, refresh_used) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let expires = expires_at
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|d| hofund_plugin_sdk::time::format_rfc3339(UNIX_EPOCH + d))
        .unwrap_or_default();
    let mut object = read_existing_object(path);
    if let Value::Object(map) = &mut object {
        map.insert(
            "accessToken".to_owned(),
            Value::String(secret_text(access).to_owned()),
        );
        map.insert(
            "refreshToken".to_owned(),
            Value::String(secret_text(refresh).to_owned()),
        );
        map.insert("expiresAt".to_owned(), Value::String(expires));
    }
    let body =
        json::to_vec(&object).map_err(|_| PluginError::io("cannot encode the Kiro token file"))?;
    write_atomic(path, &body)
}

fn refresh_token_in_object(path: &Path) -> Option<SecretString> {
    let object = read_existing_object(path);
    object
        .get("refreshToken")
        .or_else(|| object.get("refresh_token"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| SecretString::from(text.to_owned()))
}

impl Credential {
    fn auth_method_field(&self) -> String {
        match self.method.as_str() {
            "idc" => "IdC".to_owned(),
            "external-idp" => "external-idp".to_owned(),
            "apikey" => "apikey".to_owned(),
            _ => "social".to_owned(),
        }
    }
}

fn read_existing_object(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| json::from_str(&text).ok())
        .unwrap_or_else(|| Value::Object(json::Map::new()))
}

fn private_dir(dir: &Path) -> Result<(), PluginError> {
    std::fs::create_dir_all(dir)
        .map_err(|_| PluginError::io("cannot create the Kiro token directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| PluginError::io("cannot restrict the Kiro token directory"))?;
    }
    Ok(())
}

fn write_atomic(path: &Path, text: &[u8]) -> Result<(), PluginError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        private_dir(parent)?;
    }
    let mut temporary_name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("kiro.tmp"), ToOwned::to_owned);
    temporary_name.push(".tmp");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(temporary_name);
    let failed = |_| PluginError::io("cannot write the Kiro token file");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(failed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(failed)?;
        }
        file.write_all(text).map_err(failed)?;
        file.sync_all().map_err(failed)?;
    }
    std::fs::rename(&temporary, path).map_err(failed)
}
