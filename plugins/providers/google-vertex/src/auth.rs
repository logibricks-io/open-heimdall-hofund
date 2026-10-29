//! Google access tokens from Application Default Credentials, a service
//! account key, or an impersonated service account.
//!
//! References:
//! <https://google.aip.dev/auth/4110> (where Application Default Credentials live),
//! <https://developers.google.com/identity/protocols/oauth2/service-account> (the signed JWT),
//! <https://cloud.google.com/iam/docs/reference/credentials/rest/v1/projects.serviceAccounts/generateAccessToken>.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::jwt::{KeyError, RsaSigner};
use hofund_plugin_sdk::oauth::{TokenCache, account_id_from, parse_iam_access_token};
use hofund_plugin_sdk::transport::parse_retry_after_headers;
use hofund_plugin_sdk::wire::path_segment;
use hofund_plugin_sdk::{Call, form_body, parse_access_token, refresh_form};

/// Google's OAuth token endpoint.
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// The IAM Credentials API root that trades a token for a service account's.
pub(crate) const IAM_ROOT: &str = "https://iamcredentials.googleapis.com/v1";
/// The one scope Vertex AI needs.
pub(crate) const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// A token this close to its expiry is minted again.
pub(crate) const REFRESH_LEAD: Duration = Duration::from_secs(5 * 60);
/// The total time one mint may take, every request included.
pub(crate) const MINT_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest credentials file or token response read.
pub(crate) const MAX_CREDENTIALS_BYTES: u64 = 1024 * 1024;

/// Mints and caches one provider's Google access token.
///
/// The cache key is the credentials file's path, size and modification time,
/// so a rewritten file is read again. A failed mint is replayed for the
/// SDK's negative TTL so a burst of requests does not repeat a refused sign-in.
pub(crate) struct Minter {
    credentials: String,
    impersonate: String,
    token_url: String,
    iam_root: String,
    cache: TokenCache<SecretString>,
    last: Mutex<Option<AccountId>>,
}

impl Minter {
    pub(crate) fn new(
        credentials: String,
        impersonate: String,
        token_url: String,
        iam_root: String,
    ) -> Self {
        Self {
            credentials,
            impersonate,
            token_url: if token_url.is_empty() {
                TOKEN_URL.to_owned()
            } else {
                token_url
            },
            iam_root: if iam_root.is_empty() {
                IAM_ROOT.to_owned()
            } else {
                iam_root
            },
            cache: TokenCache::new(REFRESH_LEAD),
            last: Mutex::new(None),
        }
    }

    /// A fresh access token for the configured credentials.
    pub(crate) async fn access_token(
        &self,
        host: &HostContext,
    ) -> Result<SecretString, PluginError> {
        let file = credentials_file(&self.credentials)?;
        let meta = match std::fs::metadata(&file) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(missing_credentials(&file, &self.credentials));
            }
            Err(error) => {
                return Err(PluginError::decode(format!("Google credentials: {error}")));
            }
        };
        if meta.len() > MAX_CREDENTIALS_BYTES {
            return Err(PluginError::decode(
                "Google credentials file is larger than 1 MiB",
            ));
        }
        let account = account_id_from(&[&file.to_string_lossy(), &file_stamp(&meta)]);
        self.forget_others(&account);
        let clock = host.clock();
        self.cache
            .get(clock, &account, || async {
                let mint = mint_file(self, host, &file);
                match hofund_plugin_sdk::timeout(MINT_TIMEOUT, mint).await {
                    Some(Ok((token, expiry))) => Ok((token, Some(expiry))),
                    Some(Err(error)) => Err(error),
                    None => Err(PluginError::timeout(
                        "Google token mint exceeded its total deadline",
                    )),
                }
            })
            .await
    }

    /// Drops the entry of an older stamp of the file, so one provider holds
    /// one token.
    fn forget_others(&self, account: &AccountId) {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if last.as_ref() != Some(account)
            && let Some(old) = last.replace(account.clone())
        {
            self.cache.remove(&old);
        }
    }
}

fn file_stamp(meta: &std::fs::Metadata) -> String {
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    format!("{} {modified}", meta.len())
}

fn missing_credentials(file: &Path, given: &str) -> PluginError {
    let file = file.display();
    if !given.is_empty() {
        return PluginError::decode(format!(
            "no Google credentials at {file}: correct the provider's credentials file, or clear it to use Application Default Credentials"
        ));
    }
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .ok()
        .is_some_and(|value| !value.is_empty())
    {
        return PluginError::decode(format!(
            "GOOGLE_APPLICATION_CREDENTIALS names {file}, which isn't there: correct or unset it, or give the provider a credentials file"
        ));
    }
    PluginError::decode(format!(
        "no Google credentials at {file}: sign in with `gcloud auth application-default login`, or give the provider a credentials file"
    ))
}

fn credentials_file(given: &str) -> Result<PathBuf, PluginError> {
    if !given.is_empty() {
        return expand_home(given);
    }
    if let Some(path) = env_path("GOOGLE_APPLICATION_CREDENTIALS")? {
        return Ok(path);
    }
    Ok(gcloud_dir()?.join("application_default_credentials.json"))
}

fn gcloud_dir() -> Result<PathBuf, PluginError> {
    if let Some(dir) = env_path("CLOUDSDK_CONFIG")? {
        return Ok(dir);
    }
    if cfg!(windows) {
        return match std::env::var("APPDATA") {
            Ok(appdata) if !appdata.is_empty() => Ok(PathBuf::from(appdata).join("gcloud")),
            _ => Err(PluginError::decode(
                "APPDATA is unset, so gcloud's credentials folder cannot be found",
            )),
        };
    }
    Ok(home_dir()?.join(".config").join("gcloud"))
}

fn env_path(name: &str) -> Result<Option<PathBuf>, PluginError> {
    let Ok(value) = std::env::var(name) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let expanded = expand_home(&value)?;
    if !expanded.is_absolute() {
        return Err(PluginError::decode(format!(
            "{name} is {value:?}, not a full path: give the whole path, unset it, or give the provider a credentials file"
        )));
    }
    Ok(Some(expanded))
}

/// `path` with a leading `~` replaced by the home folder.
pub(crate) fn expand_home(path: &str) -> Result<PathBuf, PluginError> {
    let rest = if path == "~" {
        ""
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        rest
    } else {
        return Ok(PathBuf::from(path));
    };
    let home = home_dir()?;
    if rest.is_empty() {
        Ok(home)
    } else {
        Ok(home.join(rest))
    }
}

fn home_dir() -> Result<PathBuf, PluginError> {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => Err(PluginError::decode(format!(
            "{name} is unset, so a credentials path starting with ~ cannot be expanded"
        ))),
    }
}

/// The fields of a Google credentials file this plugin reads. Missing or
/// non-string fields are empty.
#[derive(Default)]
struct GoogleCreds {
    kind: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    client_email: String,
    private_key: String,
    private_key_id: String,
    service_account_impersonation_url: String,
    delegates: Vec<String>,
    source_credentials: Option<Box<GoogleCreds>>,
    universe_domain: String,
}

impl GoogleCreds {
    fn from_value(value: &Value) -> Result<Self, String> {
        let Value::Object(object) = value else {
            return Err("expected a JSON object".to_owned());
        };
        let text = |key: &str| -> Result<String, String> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(String::new()),
                Some(Value::String(text)) => Ok(text.clone()),
                Some(_) => Err(format!("`{key}` is not a string")),
            }
        };
        let delegates = match object.get("delegates") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "`delegates` holds a value that is not a string".to_owned())
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("`delegates` is not a list".to_owned()),
        };
        let source_credentials = match object.get("source_credentials") {
            None | Some(Value::Null) => None,
            Some(source) => Some(Box::new(Self::from_value(source)?)),
        };
        Ok(Self {
            kind: text("type")?,
            client_id: text("client_id")?,
            client_secret: text("client_secret")?,
            refresh_token: text("refresh_token")?,
            client_email: text("client_email")?,
            private_key: text("private_key")?,
            private_key_id: text("private_key_id")?,
            service_account_impersonation_url: text("service_account_impersonation_url")?,
            delegates,
            source_credentials,
            universe_domain: text("universe_domain")?,
        })
    }

    fn impersonated_email(&self) -> Result<String, PluginError> {
        if self.kind != "impersonated_service_account" {
            return Ok(String::new());
        }
        Ok(impersonated_account(&self.service_account_impersonation_url)?.unwrap_or_default())
    }
}

/// A failed mint. A refresh token refused by the token endpoint (a 4xx other
/// than 408 and 429) needs the person to sign in again.
enum MintFail {
    Person(PluginError),
    Other(PluginError),
}

impl MintFail {
    fn message(&self) -> &str {
        match self {
            Self::Person(error) | Self::Other(error) => error.message(),
        }
    }
}

/// What one mint needs: the host's HTTP client and clock, and the endpoints.
struct Mint<'a> {
    host: &'a HostContext,
    token_url: &'a str,
    iam_root: &'a str,
}

type Minted = (SecretString, SystemTime);

async fn mint_file(
    minter: &Minter,
    host: &HostContext,
    file: &Path,
) -> Result<Minted, PluginError> {
    let bytes = std::fs::read(file)
        .map_err(|error| PluginError::decode(format!("Google credentials: {error}")))?;
    let creds = hofund_plugin_sdk::json::from_slice::<Value>(&bytes)
        .map_err(|error| error.to_string())
        .and_then(|value| GoogleCreds::from_value(&value))
        .map_err(|error| {
            PluginError::decode(format!(
                "{} holds no Google credentials: {error}",
                file.display()
            ))
        })?;
    let mint = Mint {
        host,
        token_url: &minter.token_url,
        iam_root: &minter.iam_root,
    };
    let account = creds.impersonated_email()?;
    let minted = match mint_creds(&mint, &creds, true).await {
        Ok(minted) => minted,
        Err(error) => {
            let hint = if matches!(error, MintFail::Person(_)) {
                format!("; {}", sign_in_again(file, &account))
            } else {
                String::new()
            };
            return Err(PluginError::decode(format!(
                "the Google credentials in {}: {}{hint}",
                file.display(),
                error.message()
            )));
        }
    };
    if minter.impersonate.is_empty() {
        return Ok(minted);
    }
    impersonate_for(&mint, &minted.0, &minter.impersonate, &[])
        .await
        .map_err(|error| {
            PluginError::decode(format!(
                "the Google credentials in {}: {}",
                file.display(),
                error.message()
            ))
        })
}

type MintFuture<'a> = Pin<Box<dyn Future<Output = Result<Minted, MintFail>> + Send + 'a>>;

fn mint_creds<'a>(mint: &'a Mint<'a>, creds: &'a GoogleCreds, top: bool) -> MintFuture<'a> {
    Box::pin(mint_creds_inner(mint, creds, top))
}

async fn mint_creds_inner(
    mint: &Mint<'_>,
    creds: &GoogleCreds,
    top: bool,
) -> Result<Minted, MintFail> {
    if !creds.universe_domain.is_empty() && creds.universe_domain != "googleapis.com" {
        return Err(MintFail::Other(PluginError::decode(format!(
            "they are for {}; Vertex AI is asked at googleapis.com",
            creds.universe_domain
        ))));
    }
    match creds.kind.as_str() {
        "authorized_user" => {
            if creds.refresh_token.is_empty() || creds.client_id.is_empty() {
                return Err(MintFail::Other(PluginError::decode(
                    "they have no refresh token",
                )));
            }
            let body = refresh_form(&creds.client_id, &creds.client_secret, &creds.refresh_token);
            token_grant(mint, body).await.map_err(classify_refresh)
        }
        "service_account" => signed_jwt(mint, creds).await,
        "impersonated_service_account" => {
            if !top {
                return Err(MintFail::Other(PluginError::decode(
                    "an impersonation's source is another impersonation",
                )));
            }
            let account = creds.impersonated_email().map_err(MintFail::Other)?;
            if account.is_empty() {
                return Err(MintFail::Other(PluginError::decode(
                    "their service_account_impersonation_url names no service account",
                )));
            }
            let source = creds
                .source_credentials
                .as_deref()
                .filter(|source| !source.kind.is_empty())
                .ok_or_else(|| {
                    MintFail::Other(PluginError::decode(
                        "their source_credentials can't be read",
                    ))
                })?;
            let (token, _) = mint_creds(mint, source, false).await?;
            impersonate_for(mint, &token, &account, &creds.delegates)
                .await
                .map_err(MintFail::Other)
        }
        "external_account" | "external_account_authorized_user" => {
            Err(MintFail::Other(PluginError::decode(format!(
                "workload identity federation ({}) isn't read: use a person's credentials, a service account key, or impersonation",
                creds.kind
            ))))
        }
        other => Err(MintFail::Other(PluginError::decode(format!(
            "they are of type {other:?}; authorized_user, service_account and impersonated_service_account are read"
        )))),
    }
}

fn classify_refresh(error: PluginError) -> MintFail {
    match error.status() {
        Some(status) if status < 500 && status != 408 && status != 429 => MintFail::Person(error),
        _ => MintFail::Other(error),
    }
}

/// The service account a `generateAccessToken` URL names, percent-decoded.
pub(crate) fn impersonated_account(url: &str) -> Result<Option<String>, PluginError> {
    let marker = "/serviceAccounts/";
    let Some(marker_start) = url.rfind(marker) else {
        return Ok(None);
    };
    let start = marker_start + marker.len();
    let Some(email) = url[start..].strip_suffix(":generateAccessToken") else {
        return Ok(None);
    };
    if email.is_empty() || email.contains('/') || email.contains(':') {
        return Ok(None);
    }
    percent_decode(email).map(Some)
}

pub(crate) fn percent_decode(text: &str) -> Result<String, PluginError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            if index + 2 >= bytes.len() {
                return Err(PluginError::decode(
                    "service account impersonation URL has a truncated percent escape",
                ));
            }
            let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_digit),
                bytes.get(index + 2).copied().and_then(hex_digit),
            ) else {
                return Err(PluginError::decode(
                    "service account impersonation URL has a non-hex percent escape",
                ));
            };
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(byte);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| {
        PluginError::decode("percent-decoded service account email is not valid UTF-8")
    })
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// How to fix a refused refresh token, by where the file is.
fn sign_in_again(file: &Path, account: &str) -> String {
    let relative = gcloud_dir()
        .ok()
        .and_then(|dir| file.strip_prefix(dir).ok().map(Path::to_path_buf))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    if relative == "application_default_credentials.json" && !account.is_empty() {
        return format!(
            "sign in again with `gcloud auth application-default login --impersonate-service-account={account}`"
        );
    }
    if relative == "application_default_credentials.json" {
        return "sign in again with `gcloud auth application-default login`".to_owned();
    }
    if let Some(rest) = relative.strip_prefix("legacy_credentials/")
        && let Some(name) = rest.strip_suffix("/adc.json")
        && !name.is_empty()
        && !name.contains('/')
    {
        return format!("sign in again with `gcloud auth login {name}`");
    }
    "sign in again and write that file anew, or give the provider another credentials file"
        .to_owned()
}

async fn signed_jwt(mint: &Mint<'_>, creds: &GoogleCreds) -> Result<Minted, MintFail> {
    if creds.client_email.is_empty() {
        return Err(MintFail::Other(PluginError::decode(
            "their client_email is missing",
        )));
    }
    let now = mint.host.clock().now();
    let assertion = service_assertion(creds, mint.token_url, now).map_err(MintFail::Other)?;
    let body = form_body(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
        ("assertion", &assertion),
    ]);
    token_grant(mint, body).await.map_err(MintFail::Other)
}

/// The signed JWT a service account trades for an access token.
fn service_assertion(
    creds: &GoogleCreds,
    token_url: &str,
    now: SystemTime,
) -> Result<String, PluginError> {
    let signer = RsaSigner::from_pem(&creds.private_key).map_err(key_error)?;
    let now = now
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PluginError::decode("the system clock is before the unix epoch"))?;
    let iat = now.as_secs().saturating_sub(10);
    let exp = iat.saturating_add(60 * 60);
    let mut header = json!({"alg": "RS256", "typ": "JWT"});
    if !creds.private_key_id.is_empty() {
        header["kid"] = Value::String(creds.private_key_id.to_owned());
    }
    let claims = json!({
        "iss": creds.client_email,
        "scope": SCOPE,
        "aud": token_url,
        "iat": iat,
        "exp": exp,
    });
    signer.sign(&header, &claims).map_err(key_error)
}

fn key_error(error: KeyError) -> PluginError {
    PluginError::decode(match error {
        KeyError::NotPem => "their private_key is no PEM key",
        KeyError::Encrypted => "their private_key is encrypted; a service account key is not",
        KeyError::Sign => "their private_key could not sign",
        _ => "their private_key is no RSA key",
    })
}

async fn token_grant(mint: &Mint<'_>, body: String) -> Result<Minted, PluginError> {
    let call = Call::post(mint.token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .bytes(body);
    let bytes = post_token(mint.host, call).await?;
    let token = parse_access_token(&bytes).map_err(|error| PluginError::decode(error.message()))?;
    let expiry = mint.host.clock().now() + token.expires_in();
    Ok((SecretString::from(token.expose_secret()), expiry))
}

async fn impersonate_for(
    mint: &Mint<'_>,
    token: &SecretString,
    account: &str,
    delegates: &[String],
) -> Result<Minted, PluginError> {
    let mut body = json!({
        "scope": [SCOPE],
        "lifetime": "3600s",
    });
    if !delegates.is_empty() {
        body["delegates"] = Value::Array(
            delegates
                .iter()
                .map(|delegate| Value::String(delegate_name(delegate)))
                .collect(),
        );
    }
    let url = format!(
        "{}/projects/-/serviceAccounts/{}:generateAccessToken",
        mint.iam_root.trim_end_matches('/'),
        path_segment(account)
    );
    let call = Call::post(url).json(body.to_string()).bearer(token)?;
    let bytes = match post_token(mint.host, call).await {
        Ok(bytes) => bytes,
        Err(error) if error.status() == Some(403) => {
            return Err(PluginError::decode(format!(
                "impersonating {account}: {}; the account signed in needs the Service Account Token Creator role (roles/iam.serviceAccountTokenCreator) on it",
                error.message()
            )));
        }
        Err(error) => {
            return Err(PluginError::decode(format!(
                "impersonating {account}: {}",
                error.message()
            )));
        }
    };
    let iam = parse_iam_access_token(&bytes)
        .map_err(|error| PluginError::decode(format!("impersonating {account}: {error}")))?;
    let access = iam.access_token();
    let expiry = iam
        .expire_time()
        .and_then(parse_rfc3339_utc)
        .unwrap_or_else(|| mint.host.clock().now() + access.expires_in());
    Ok((SecretString::from(access.expose_secret()), expiry))
}

fn delegate_name(value: &str) -> String {
    if value.starts_with("projects/") {
        value.to_owned()
    } else {
        format!("projects/-/serviceAccounts/{value}")
    }
}

/// Sends one token request and reads at most 1 MiB of its answer.
///
/// A refusal is a status error that names the status and, when the body has
/// a short `error` code, that code. The body itself is never repeated.
async fn post_token(host: &HostContext, call: Call) -> Result<Vec<u8>, PluginError> {
    let mut response = call
        .header("accept", "application/json")
        .open_raw(host.http(), 1)
        .await?;
    let status = response.status().as_u16();
    let ok = (200..300).contains(&status);
    let retry_after = parse_retry_after_headers(response.headers(), SystemTime::now());
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body_mut().next_chunk().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) if !ok => break,
            Err(error) => return Err(error),
        };
        if u64::try_from(bytes.len().saturating_add(chunk.len()))
            .is_ok_and(|len| len > MAX_CREDENTIALS_BYTES)
        {
            return Err(PluginError::decode(
                "the token response is larger than 1 MiB",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if !ok {
        let message = token_error_code(&bytes).map_or_else(
            || format!("the sign-in was refused ({status})"),
            |code| format!("the sign-in was refused ({status}): {code}"),
        );
        let error = PluginError::status_error(status, message);
        return Err(match retry_after {
            Some(after) => error.with_retry_after(after),
            None => error,
        });
    }
    Ok(bytes)
}

fn token_error_code(bytes: &[u8]) -> Option<String> {
    let value = hofund_plugin_sdk::json::from_slice::<Value>(bytes).ok()?;
    let code = value.get("error")?.as_str()?;
    let valid = (1..=64).contains(&code.len())
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
    valid.then(|| code.to_owned())
}

/// An RFC 3339 time in UTC (`Z`), the form IAM's `expireTime` takes.
pub(crate) fn parse_rfc3339_utc(text: &str) -> Option<SystemTime> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut dates = date.split('-');
    let year: i32 = dates.next()?.parse().ok()?;
    let month: u32 = dates.next()?.parse().ok()?;
    let day: u32 = dates.next()?.parse().ok()?;
    if dates.next().is_some() {
        return None;
    }
    let time = time.split('.').next()?;
    let mut times = time.split(':');
    let hour: u32 = times.next()?.parse().ok()?;
    let minute: u32 = times.next()?.parse().ok()?;
    let second: u32 = times.next()?.parse().ok()?;
    if times.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let seconds =
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second);
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Days since the Unix epoch for a civil date. Howard Hinnant's algorithm.
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = i64::from(year.rem_euclid(400));
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = i64::from((153 * month_prime + 2) / 5 + day - 1);
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(i64::from(era) * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests;
