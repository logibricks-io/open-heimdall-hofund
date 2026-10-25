//! Adapter for Google Vertex AI's publisher `generateContent` API.
//!
//! The model is a path segment under the caller's project:
//! `{root}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent`.
//! A stream uses `:streamGenerateContent?alt=sse`. The call is authorized with
//! an OAuth access token minted from Application Default Credentials. No API
//! key is sent, and no service-account key is stored in the gateway config.
//!
//! Endpoint shape, checked 2026-10-25:
//! <https://cloud.google.com/vertex-ai/generative-ai/docs/model-reference/inference>.
//! Location hosts:
//! <https://cloud.google.com/vertex-ai/generative-ai/docs/learn/locations>.
//! Application Default Credentials:
//! <https://cloud.google.com/docs/authentication/application-default-credentials>.
//! A service-account key is traded as a signed JWT:
//! <https://developers.google.com/identity/protocols/oauth2/service-account>.
//! Impersonation uses IAM Credentials `generateAccessToken`:
//! <https://cloud.google.com/iam/docs/reference/credentials/rest/v1/projects.serviceAccounts/generateAccessToken>.
//!
//! Not read: the metadata server, workload identity federation
//! (`external_account`), and express-mode API keys.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hofund_codec::GenerativeLanguage;
use hofund_ir::Request;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::Value;

use crate::error::ProviderError;
use crate::generative::{self, classify_count};
use crate::http::{self, Endpoint, Upstream};
use crate::{Completion, CountOutcome, Provider};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const IAM_ROOT: &str = "https://iamcredentials.googleapis.com/v1";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const REFRESH_LEAD: Duration = Duration::from_secs(5 * 60);
const MINT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CREDENTIALS_BYTES: u64 = 1 << 20;

const US_EU_MODELS: &[&str] = &[
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
];

const REGION_MODELS: &[&str] = &[
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "gemini-2.5-flash-lite",
];

/// One Vertex AI upstream.
///
/// The access token is cached on this value until five minutes before it
/// expires, or until the credentials file's size or modification time
/// changes. Overlapping calls can each mint a token; a later call reuses
/// whichever finished last.
pub struct Vertex {
    upstream: Upstream,
    base: String,
    credentials: String,
    impersonate: String,
    token_url: String,
    iam_root: String,
    token_client: reqwest::Client,
    cache: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    stamp: String,
    token: String,
    expiry: SystemTime,
}

impl std::fmt::Debug for Vertex {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Vertex")
            .field("id", &self.upstream.id)
            .field("base", &self.base)
            .field("credentials", &self.credentials)
            .field("impersonate", &self.impersonate)
            .finish()
    }
}

impl Vertex {
    /// Builds a client that mints tokens at Google's own endpoints.
    ///
    /// `endpoint.base_url` is the root returned by [`base_url`]: the project
    /// and location are already in it. `credentials` is empty to use
    /// Application Default Credentials, or an absolute path (or `~/...`) to
    /// a credentials file. `impersonate` is empty, or the email of a service
    /// account the minted token is traded for.
    ///
    /// `endpoint.api_key` must be empty. A key is not a Vertex credential.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the id, base URL, credentials path, or
    /// impersonation email is unusable.
    pub fn new(
        endpoint: Endpoint,
        credentials: impl Into<String>,
        impersonate: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Self::open(
            endpoint,
            credentials.into(),
            impersonate.into(),
            TOKEN_URL.to_owned(),
            IAM_ROOT.to_owned(),
        )
    }

    /// [`Vertex::new`] with the token endpoint and the IAM Credentials root
    /// replaced. Tests point both at a local server. `iam_root` is the prefix
    /// before `/projects/-/serviceAccounts/...`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] in the same cases as [`Vertex::new`].
    pub fn with_token_endpoints(
        endpoint: Endpoint,
        credentials: impl Into<String>,
        impersonate: impl Into<String>,
        token_url: impl Into<String>,
        iam_root: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        Self::open(
            endpoint,
            credentials.into(),
            impersonate.into(),
            token_url.into(),
            iam_root.into(),
        )
    }

    fn open(
        endpoint: Endpoint,
        credentials: String,
        impersonate: String,
        token_url: String,
        iam_root: String,
    ) -> Result<Self, ProviderError> {
        if !endpoint.api_key.is_empty() {
            return Err(ProviderError::decode(
                "Google Vertex AI uses Google credentials, not an API key",
            ));
        }
        let credentials = credentials.trim().to_owned();
        let impersonate = impersonate.trim().to_ascii_lowercase();
        check_settings(&credentials, &impersonate)?;
        let base = endpoint.base_url.trim().trim_end_matches('/').to_owned();
        if base.is_empty() {
            return Err(ProviderError::decode("provider base URL is empty"));
        }
        reqwest::Url::parse(&base)
            .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
        let token_client = reqwest::Client::builder()
            .timeout(MINT_TIMEOUT)
            .build()
            .map_err(|error| ProviderError::connect(error.to_string()))?;
        let upstream = Upstream::open(endpoint, "", reqwest::header::HeaderMap::new())?;
        Ok(Self {
            upstream,
            base,
            credentials,
            impersonate,
            token_url,
            iam_root,
            token_client,
            cache: Mutex::new(None),
        })
    }

    /// Static model ids a location serves.
    ///
    /// `global` is the catalog list. `us` and `eu` serve the Gemini 3 Flash
    /// ids. Any other location, including a region such as `us-central1`,
    /// serves the Gemini 2.5 ids. This is not a live list.
    #[must_use]
    pub fn models_for(location: &str) -> Vec<String> {
        let location = location.trim().to_ascii_lowercase();
        match location.as_str() {
            "" | "global" => crate::presets::preset_models("google-vertex")
                .map(<[String]>::to_vec)
                .unwrap_or_default(),
            "us" | "eu" => owned(US_EU_MODELS),
            _ => owned(REGION_MODELS),
        }
    }
}

fn owned(models: &[&str]) -> Vec<String> {
    models.iter().map(|model| (*model).to_owned()).collect()
}

/// The API root for `project` at `location`.
///
/// An empty location is `global`. An empty `root_override` uses the host for
/// that location (`aiplatform.googleapis.com`, `aiplatform.<us|eu>.rep.googleapis.com`,
/// or `<region>-aiplatform.googleapis.com`). A non-empty override replaces
/// that host, which is how a test server or a relay is addressed. A pasted
/// `:generateContent` URL is cut back to the project root.
///
/// # Errors
///
/// Returns [`ProviderError`] when the project id or location is unusable, or
/// the resulting URL does not parse.
pub(crate) fn base_url(
    project: &str,
    location: &str,
    root_override: &str,
) -> Result<String, ProviderError> {
    let project = project.trim().to_ascii_lowercase();
    let mut location = location.trim().to_ascii_lowercase();
    if project.is_empty() {
        return Err(ProviderError::decode(
            "Google Vertex AI needs the id of your Google Cloud project",
        ));
    }
    if !project_ok(&project) {
        return Err(ProviderError::decode(format!(
            "{project:?} is no Google Cloud project id"
        )));
    }
    if location.is_empty() {
        location = "global".to_owned();
    }
    if !location_ok(&location) {
        return Err(ProviderError::decode(format!(
            "{location:?} is no Vertex AI location: global, us, eu or a region such as us-central1"
        )));
    }
    let suffix = format!("/v1/projects/{project}/locations/{location}");
    let root = if root_override.trim().is_empty() {
        format!("https://{}", host(&location))
    } else {
        let mut raw = root_override.trim().trim_end_matches('/').to_owned();
        if !raw.contains("://") {
            raw = format!("https://{raw}");
        }
        if let Some(index) = raw.find("/publishers/google/models") {
            raw.truncate(index);
            raw = raw.trim_end_matches('/').to_owned();
        }
        if raw.ends_with(&suffix) {
            return parse_base(&raw);
        }
        raw
    };
    parse_base(&format!("{root}{suffix}"))
}

fn parse_base(url: &str) -> Result<String, ProviderError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
    if parsed.host_str().is_none() {
        return Err(ProviderError::decode("provider URL has no host"));
    }
    Ok(url.trim_end_matches('/').to_owned())
}

/// The API host for a location that has already been checked.
pub(crate) fn host(location: &str) -> String {
    match location {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{location}.rep.googleapis.com"),
        other => format!("{other}-aiplatform.googleapis.com"),
    }
}

fn project_ok(project: &str) -> bool {
    let bytes = project.as_bytes();
    let [first, .., last] = bytes else {
        return false;
    };
    project_char(*first) && project_char(*last) && bytes.iter().all(|byte| project_byte(*byte))
}

fn project_char(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn project_byte(byte: u8) -> bool {
    project_char(byte) || matches!(byte, b'.' | b':' | b'-')
}

fn location_ok(location: &str) -> bool {
    let mut parts = location.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty() || !first.bytes().all(|byte| byte.is_ascii_lowercase()) {
        return false;
    }
    parts.all(|part| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

pub(crate) fn check_settings(credentials: &str, impersonate: &str) -> Result<(), ProviderError> {
    if !credentials.is_empty() && !credentials_path_ok(credentials) {
        return Err(ProviderError::decode(format!(
            "give the credentials file's full path, not {credentials:?}"
        )));
    }
    if !impersonate.is_empty() && !account_ok(impersonate) {
        return Err(ProviderError::decode(format!(
            "{impersonate:?} is no service account's email"
        )));
    }
    Ok(())
}

fn credentials_path_ok(path: &str) -> bool {
    path == "~"
        || path.starts_with("~/")
        || path.starts_with("~\\")
        || Path::new(path).is_absolute()
}

fn account_ok(email: &str) -> bool {
    let Some((local, host)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !host.is_empty()
        && !local.contains([' ', '/'])
        && !host.contains([' ', '/'])
        && !email.contains(char::is_whitespace)
}

#[async_trait]
impl Provider for Vertex {
    fn id(&self) -> &str {
        http::id_of(&self.upstream)
    }

    async fn count_input(
        &self,
        mut request: Request,
        inbound_protocol: &str,
    ) -> Result<CountOutcome, ProviderError> {
        request.stream = false;
        let token = self.access_token().await?;
        let url = join_url(&self.base, &count_path(&request.model)?)?;
        let body = http::prepare_body(
            &self.upstream,
            &GenerativeLanguage,
            request,
            Some(GenerativeLanguage::prepare_upstream_body),
            inbound_protocol,
        )?;
        let response = http::post_bearer(&self.upstream, &url, body, &token).await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.map_err(http::map_transport)?;
        classify_count(status, &bytes)
    }

    async fn execute(
        &self,
        request: Request,
        inbound_protocol: &str,
    ) -> Result<Completion, ProviderError> {
        let token = self.access_token().await?;
        let url = join_url(&self.base, &method_path(&request.model, request.stream)?)?;
        http::execute_bearer(
            &self.upstream,
            &url,
            &GenerativeLanguage,
            request,
            Some(GenerativeLanguage::prepare_upstream_body),
            inbound_protocol,
            &token,
        )
        .await
    }
}

fn method_path(model: &str, stream: bool) -> Result<String, ProviderError> {
    let model = generative::escape_model(model)?;
    if stream {
        Ok(format!(
            "/publishers/google/models/{model}:streamGenerateContent?alt=sse"
        ))
    } else {
        Ok(format!("/publishers/google/models/{model}:generateContent"))
    }
}

fn count_path(model: &str) -> Result<String, ProviderError> {
    Ok(format!(
        "/publishers/google/models/{}:countTokens",
        generative::escape_model(model)?
    ))
}

fn join_url(base: &str, path: &str) -> Result<String, ProviderError> {
    let url = format!("{base}{path}");
    reqwest::Url::parse(&url)
        .map_err(|error| ProviderError::decode(format!("provider URL is invalid: {error}")))?;
    Ok(url)
}

impl Vertex {
    async fn access_token(&self) -> Result<String, ProviderError> {
        let file = credentials_file(&self.credentials)?;
        let meta = match std::fs::metadata(&file) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(missing_credentials(&file, &self.credentials));
            }
            Err(error) => {
                return Err(ProviderError::decode(format!(
                    "Google credentials: {error}"
                )));
            }
        };
        if meta.len() > MAX_CREDENTIALS_BYTES {
            return Err(ProviderError::decode(
                "Google credentials file is larger than 1 MiB",
            ));
        }
        let stamp = file_stamp(&meta);
        if let Some(token) = self.cached(&stamp) {
            return Ok(token);
        }
        let (token, expiry) = mint_file(
            &self.token_client,
            &file,
            &self.impersonate,
            &self.token_url,
            &self.iam_root,
        )
        .await?;
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cache = Some(CachedToken {
            stamp,
            token: token.clone(),
            expiry,
        });
        Ok(token)
    }

    fn cached(&self, stamp: &str) -> Option<String> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cached = cache.as_ref()?;
        if cached.stamp != stamp {
            return None;
        }
        let fresh_until = cached.expiry.checked_sub(REFRESH_LEAD)?;
        if SystemTime::now() < fresh_until {
            Some(cached.token.clone())
        } else {
            None
        }
    }
}

fn file_stamp(meta: &std::fs::Metadata) -> String {
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{} {modified}", meta.len())
}

fn missing_credentials(file: &Path, given: &str) -> ProviderError {
    let file = file.display();
    if !given.is_empty() {
        return ProviderError::decode(format!(
            "no Google credentials at {file}: correct the provider's credentials file, or clear it to use Application Default Credentials"
        ));
    }
    if std::env::var("GOOGLE_APPLICATION_CREDENTIALS")
        .ok()
        .is_some_and(|value| !value.is_empty())
    {
        return ProviderError::decode(format!(
            "GOOGLE_APPLICATION_CREDENTIALS names {file}, which isn't there: correct or unset it, or give the provider a credentials file"
        ));
    }
    ProviderError::decode(format!(
        "no Google credentials at {file}: sign in with `gcloud auth application-default login`, or give the provider a credentials file"
    ))
}

fn credentials_file(given: &str) -> Result<PathBuf, ProviderError> {
    if !given.is_empty() {
        return expand_home(given);
    }
    if let Some(path) = env_path("GOOGLE_APPLICATION_CREDENTIALS")? {
        return Ok(path);
    }
    Ok(gcloud_dir()?.join("application_default_credentials.json"))
}

fn gcloud_dir() -> Result<PathBuf, ProviderError> {
    if let Some(dir) = env_path("CLOUDSDK_CONFIG")? {
        return Ok(dir);
    }
    if cfg!(windows) {
        let appdata = std::env::var("APPDATA").map_err(|_| {
            ProviderError::decode(
                "APPDATA is unset, so gcloud's credentials folder cannot be found",
            )
        })?;
        if appdata.is_empty() {
            return Err(ProviderError::decode(
                "APPDATA is unset, so gcloud's credentials folder cannot be found",
            ));
        }
        return Ok(PathBuf::from(appdata).join("gcloud"));
    }
    Ok(home_dir()?.join(".config").join("gcloud"))
}

fn env_path(name: &str) -> Result<Option<PathBuf>, ProviderError> {
    let Ok(value) = std::env::var(name) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    let expanded = expand_home(&value)?;
    if !expanded.is_absolute() {
        return Err(ProviderError::decode(format!(
            "{name} is {value:?}, not a full path: give the whole path, unset it, or give the provider a credentials file"
        )));
    }
    Ok(Some(expanded))
}

fn expand_home(path: &str) -> Result<PathBuf, ProviderError> {
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

fn home_dir() -> Result<PathBuf, ProviderError> {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => Err(ProviderError::decode(format!(
            "{name} is unset, so a credentials path starting with ~ cannot be expanded"
        ))),
    }
}

#[derive(Debug, Deserialize)]
struct GoogleCreds {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    client_email: String,
    #[serde(default)]
    private_key: String,
    #[serde(default)]
    private_key_id: String,
    #[serde(default)]
    service_account_impersonation_url: String,
    #[serde(default)]
    delegates: Vec<String>,
    #[serde(default)]
    source_credentials: Option<Box<GoogleCreds>>,
    #[serde(default)]
    universe_domain: String,
}

enum MintFail {
    Person(ProviderError),
    Other(ProviderError),
}

impl MintFail {
    fn message(&self) -> &str {
        match self {
            Self::Person(error) | Self::Other(error) => &error.message,
        }
    }
}

struct MintUrls<'a> {
    client: &'a reqwest::Client,
    token_url: &'a str,
    iam_root: &'a str,
}

async fn mint_file(
    client: &reqwest::Client,
    file: &Path,
    impersonate: &str,
    token_url: &str,
    iam_root: &str,
) -> Result<(String, SystemTime), ProviderError> {
    let bytes = std::fs::read(file)
        .map_err(|error| ProviderError::decode(format!("Google credentials: {error}")))?;
    let creds: GoogleCreds = serde_json::from_slice(&bytes).map_err(|error| {
        ProviderError::decode(format!(
            "{} holds no Google credentials: {error}",
            file.display()
        ))
    })?;
    let urls = MintUrls {
        client,
        token_url,
        iam_root,
    };
    let account = creds.impersonated_email();
    let minted = match mint_creds(&urls, &creds, true).await {
        Ok(minted) => minted,
        Err(error) => {
            let hint = if matches!(error, MintFail::Person(_)) {
                format!("; {}", sign_in_again(file, &account))
            } else {
                String::new()
            };
            return Err(ProviderError::decode(format!(
                "the Google credentials in {}: {}{hint}",
                file.display(),
                error.message()
            )));
        }
    };
    if impersonate.is_empty() {
        return Ok(minted);
    }
    impersonate_for(&urls, &minted.0, impersonate, &[])
        .await
        .map_err(|error| {
            ProviderError::decode(format!(
                "the Google credentials in {}: {error}",
                file.display()
            ))
        })
}

type MintFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(String, SystemTime), MintFail>> + Send + 'a>,
>;

fn mint_creds<'a>(urls: &'a MintUrls<'a>, creds: &'a GoogleCreds, top: bool) -> MintFuture<'a> {
    Box::pin(mint_creds_inner(urls, creds, top))
}

async fn mint_creds_inner(
    urls: &MintUrls<'_>,
    creds: &GoogleCreds,
    top: bool,
) -> Result<(String, SystemTime), MintFail> {
    if !creds.universe_domain.is_empty() && creds.universe_domain != "googleapis.com" {
        return Err(MintFail::Other(ProviderError::decode(format!(
            "they are for {}; Vertex AI is asked at googleapis.com",
            creds.universe_domain
        ))));
    }
    match creds.r#type.as_str() {
        "authorized_user" => {
            if creds.refresh_token.is_empty() || creds.client_id.is_empty() {
                return Err(MintFail::Other(ProviderError::decode(
                    "they have no refresh token",
                )));
            }
            let body = form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &creds.client_id),
                ("client_secret", &creds.client_secret),
                ("refresh_token", &creds.refresh_token),
            ]);
            match token_grant(urls, body).await {
                Ok(minted) => Ok(minted),
                Err(error) => Err(classify_refresh(error)),
            }
        }
        "service_account" => signed_jwt(urls, creds).await,
        "impersonated_service_account" => {
            if !top {
                return Err(MintFail::Other(ProviderError::decode(
                    "an impersonation's source is another impersonation",
                )));
            }
            let account = creds.impersonated_email();
            if account.is_empty() {
                return Err(MintFail::Other(ProviderError::decode(
                    "their service_account_impersonation_url names no service account",
                )));
            }
            let Some(source) = &creds.source_credentials else {
                return Err(MintFail::Other(ProviderError::decode(
                    "their source_credentials can't be read",
                )));
            };
            if source.r#type.is_empty() {
                return Err(MintFail::Other(ProviderError::decode(
                    "their source_credentials can't be read",
                )));
            }
            let (token, _) = mint_creds(urls, source, false).await?;
            impersonate_for(urls, &token, &account, &creds.delegates)
                .await
                .map_err(MintFail::Other)
        }
        "external_account" | "external_account_authorized_user" => {
            Err(MintFail::Other(ProviderError::decode(format!(
                "workload identity federation ({}) isn't read: use a person's credentials, a service account key, or impersonation",
                creds.r#type
            ))))
        }
        other => Err(MintFail::Other(ProviderError::decode(format!(
            "they are of type {other:?}; authorized_user, service_account and impersonated_service_account are read"
        )))),
    }
}

fn classify_refresh(error: ProviderError) -> MintFail {
    match error.status {
        Some(status) if status < 500 && status != 408 && status != 429 => MintFail::Person(error),
        _ => MintFail::Other(error),
    }
}

impl GoogleCreds {
    fn impersonated_email(&self) -> String {
        if self.r#type != "impersonated_service_account" {
            return String::new();
        }
        impersonated_account(&self.service_account_impersonation_url).unwrap_or_default()
    }
}

fn impersonated_account(url: &str) -> Option<String> {
    let marker = "/serviceAccounts/";
    let start = url.rfind(marker)? + marker.len();
    let email = url[start..].strip_suffix(":generateAccessToken")?;
    if email.is_empty() || email.contains('/') || email.contains(':') {
        return None;
    }
    Some(percent_decode(email))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &text[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sign_in_again(file: &Path, account: &str) -> String {
    let relative = gcloud_dir()
        .ok()
        .and_then(|dir| file.strip_prefix(dir).ok())
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

async fn signed_jwt(
    urls: &MintUrls<'_>,
    creds: &GoogleCreds,
) -> Result<(String, SystemTime), MintFail> {
    if creds.client_email.is_empty() {
        return Err(MintFail::Other(ProviderError::decode(
            "their client_email is missing",
        )));
    }
    let assertion = match service_assertion(creds, urls.token_url) {
        Ok(assertion) => assertion,
        Err(error) => return Err(MintFail::Other(error)),
    };
    let body = form(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
        ("assertion", &assertion),
    ]);
    token_grant(urls, body).await.map_err(MintFail::Other)
}

fn service_assertion(creds: &GoogleCreds, token_url: &str) -> Result<String, ProviderError> {
    let pkcs8 = pem_to_pkcs8(&creds.private_key)?;
    let key = RsaKeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| ProviderError::decode("their private_key is no RSA key"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ProviderError::decode("the system clock is before the unix epoch"))?;
    let iat = now.as_secs().saturating_sub(10);
    let exp = iat.saturating_add(60 * 60);
    let mut header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
    if !creds.private_key_id.is_empty() {
        header["kid"] = Value::String(creds.private_key_id.clone());
    }
    let claims = serde_json::json!({
        "iss": creds.client_email,
        "scope": SCOPE,
        "aud": token_url,
        "iat": iat,
        "exp": exp,
    });
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header"));
    let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("claims"));
    let unsigned = format!("{header}.{claims}");
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        unsigned.as_bytes(),
        &mut signature,
    )
    .map_err(|_| ProviderError::decode("their private_key could not sign"))?;
    Ok(format!("{unsigned}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

fn pem_to_pkcs8(text: &str) -> Result<Vec<u8>, ProviderError> {
    let mut body = String::new();
    let mut label = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            let rest = rest.strip_suffix("-----").unwrap_or(rest);
            label = Some(rest.to_owned());
            continue;
        }
        if line.starts_with("-----END ") {
            break;
        }
        if label.is_some() {
            body.push_str(line);
        }
    }
    let Some(label) = label else {
        return Err(ProviderError::decode("their private_key is no PEM key"));
    };
    if body.is_empty() {
        return Err(ProviderError::decode("their private_key is no PEM key"));
    }
    let der = STANDARD
        .decode(body.as_bytes())
        .map_err(|_| ProviderError::decode("their private_key is no PEM key"))?;
    match label.as_str() {
        "PRIVATE KEY" => Ok(der),
        "RSA PRIVATE KEY" => Ok(pkcs1_to_pkcs8(&der)),
        "ENCRYPTED PRIVATE KEY" => Err(ProviderError::decode(
            "their private_key is encrypted; a service account key is not",
        )),
        _ => Err(ProviderError::decode("their private_key is no RSA key")),
    }
}

fn pkcs1_to_pkcs8(pkcs1: &[u8]) -> Vec<u8> {
    let algorithm = der_seq(&[
        der_tlv(
            0x06,
            &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01],
        ),
        vec![0x05, 0x00],
    ]);
    der_seq(&[der_tlv(0x02, &[0x00]), algorithm, der_tlv(0x04, pkcs1)])
}

fn der_seq(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(part);
    }
    der_tlv(0x30, &body)
}

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(tag);
    let len = body.len();
    if len < 128 {
        out.push(len as u8);
    } else if len <= 0xff {
        out.push(0x81);
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    } else {
        out.push(0x83);
        out.push((len >> 16) as u8);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    }
    out.extend_from_slice(body);
    out
}

async fn token_grant(
    urls: &MintUrls<'_>,
    body: String,
) -> Result<(String, SystemTime), ProviderError> {
    let bytes = post_token(
        urls.client,
        urls.token_url,
        "application/x-www-form-urlencoded",
        None,
        body.into_bytes(),
    )
    .await?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| ProviderError::decode(format!("Google's token response: {error}")))?;
    let token = value
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if token.is_empty() {
        return Err(ProviderError::decode("Google gave no access token"));
    }
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(3600);
    let expiry = SystemTime::now() + Duration::from_secs(expires_in as u64);
    Ok((token, expiry))
}

async fn impersonate_for(
    urls: &MintUrls<'_>,
    token: &str,
    account: &str,
    delegates: &[String],
) -> Result<(String, SystemTime), ProviderError> {
    let mut body = serde_json::json!({
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
    let encoded = serde_json::to_vec(&body).expect("impersonation body");
    let url = format!(
        "{}/projects/-/serviceAccounts/{}:generateAccessToken",
        urls.iam_root.trim_end_matches('/'),
        generative::path_segment(account)
    );
    let bytes = match post_token(urls.client, &url, "application/json", Some(token), encoded).await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            if error.status == Some(403) {
                return Err(ProviderError::decode(format!(
                    "impersonating {account}: {error}; the account signed in needs the Service Account Token Creator role (roles/iam.serviceAccountTokenCreator) on it"
                )));
            }
            return Err(ProviderError::decode(format!(
                "impersonating {account}: {error}"
            )));
        }
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        ProviderError::decode(format!("impersonating {account}: IAM Credentials: {error}"))
    })?;
    let access = value
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if access.is_empty() {
        return Err(ProviderError::decode(format!(
            "impersonating {account}: IAM Credentials gave no access token"
        )));
    }
    let expiry = value
        .get("expireTime")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_utc)
        .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(3600));
    Ok((access, expiry))
}

fn delegate_name(value: &str) -> String {
    if value.starts_with("projects/") {
        value.to_owned()
    } else {
        format!("projects/-/serviceAccounts/{value}")
    }
}

async fn post_token(
    client: &reqwest::Client,
    url: &str,
    content_type: &str,
    bearer: Option<&str>,
    body: Vec<u8>,
) -> Result<Vec<u8>, ProviderError> {
    let mut builder = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, content_type)
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body);
    if let Some(token) = bearer {
        builder = builder.header(reqwest::header::AUTHORIZATION, bearer_value(token)?);
    }
    let response = builder.send().await.map_err(http::map_transport)?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(http::map_transport)?;
    if bytes.len() > MAX_CREDENTIALS_BYTES as usize {
        return Err(ProviderError::decode(
            "the token response is larger than 1 MiB",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(ProviderError::status(
            status,
            format!(
                "the sign-in was refused ({status}): {}",
                token_message(status, &bytes)
            ),
        ));
    }
    Ok(bytes.to_vec())
}

fn bearer_value(token: &str) -> Result<reqwest::header::HeaderValue, ProviderError> {
    let mut value = reqwest::header::HeaderValue::try_from(format!("Bearer {token}"))
        .map_err(|_| ProviderError::decode("access token is not valid header text"))?;
    value.set_sensitive(true);
    Ok(value)
}

fn token_message(status: u16, bytes: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim();
        if text.is_empty() {
            return http_status_text(status);
        }
        return text.to_owned();
    };
    for key in ["error_description", "detail", "message"] {
        if let Some(text) = value.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            return text.to_owned();
        }
    }
    match value.get("error") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        Some(Value::Object(map)) => map
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or(&http_status_text(status))
            .to_owned(),
        _ => http_status_text(status),
    }
}

fn http_status_text(status: u16) -> String {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => return format!("HTTP {status}"),
    }
    .to_owned()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{}={}", form_escape(name), form_escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push(char::from(HEX[(byte >> 4) as usize]));
            out.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    out
}

fn parse_rfc3339_utc(text: &str) -> Option<SystemTime> {
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
    if seconds < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

/// Days since the Unix epoch for a civil date. Howard Hinnant's algorithm.
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400) as u64;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + u64::from(day_of_year);
    Some(i64::from(era) * 146_097 + day_of_era as i64 - 719_468)
}

#[cfg(test)]
mod tests {
    use super::{
        GoogleCreds, base_url, check_settings, host, method_path, parse_rfc3339_utc, pem_to_pkcs8,
        service_assertion,
    };
    use base64::Engine;
    use ring::rand::SystemRandom;
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

    #[test]
    fn the_host_follows_the_location() {
        assert_eq!(host("global"), "aiplatform.googleapis.com");
        assert_eq!(host("us"), "aiplatform.us.rep.googleapis.com");
        assert_eq!(host("eu"), "aiplatform.eu.rep.googleapis.com");
        assert_eq!(host("us-central1"), "us-central1-aiplatform.googleapis.com");
        assert_eq!(
            base_url("My-Project", "", "").expect("global"),
            "https://aiplatform.googleapis.com/v1/projects/my-project/locations/global"
        );
        assert_eq!(
            base_url("my-project", "US-CENTRAL1", "").expect("region"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1"
        );
        assert_eq!(
            base_url("my-project", "global", "http://127.0.0.1:9").expect("override"),
            "http://127.0.0.1:9/v1/projects/my-project/locations/global"
        );
        assert_eq!(
            base_url(
                "my-project",
                "global",
                "http://127.0.0.1:9/v1/projects/my-project/locations/global/publishers/google/models/gemini:generateContent"
            )
            .expect("pasted"),
            "http://127.0.0.1:9/v1/projects/my-project/locations/global"
        );
        assert!(
            base_url("", "global", "")
                .unwrap_err()
                .to_string()
                .contains("Google Cloud project")
        );
        assert!(base_url("Bad_Project", "global", "").is_err());
        assert!(base_url("my-project", "not a region", "").is_err());
        assert!(
            super::Vertex::models_for("global")
                .iter()
                .any(|model| model == "gemini-2.5-pro")
        );
        assert!(
            super::Vertex::models_for("us")
                .iter()
                .any(|model| model == "gemini-3.5-flash")
        );
        assert_eq!(
            super::Vertex::models_for("us-central1"),
            super::Vertex::models_for("europe-west4")
        );
    }

    #[test]
    fn the_model_is_a_publisher_path_segment() {
        assert_eq!(
            method_path("gemini-2.5-flash", false).expect("unary"),
            "/publishers/google/models/gemini-2.5-flash:generateContent"
        );
        assert_eq!(
            method_path("models/gemini-2.5-pro", true).expect("stream"),
            "/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            method_path("vendor/model", false).expect("slash"),
            "/publishers/google/models/vendor%2Fmodel:generateContent"
        );
    }

    #[test]
    fn a_credentials_path_is_absolute_or_home_relative() {
        let absolute = if cfg!(windows) {
            r"C:\adc.json"
        } else {
            "/tmp/adc.json"
        };
        check_settings(absolute, "").expect("absolute");
        check_settings("~/adc.json", "worker@example.com").expect("home");
        assert!(check_settings("adc.json", "").is_err());
        assert!(check_settings(absolute, "not-an-email").is_err());
    }

    #[test]
    fn a_pkcs1_key_signs_like_its_pkcs8_form() {
        let pkcs8 =
            pem_to_pkcs8(include_str!("../tests/fixtures/vertex-sa-pkcs8.pem")).expect("pkcs8");
        let pkcs1_pem = include_str!("../tests/fixtures/vertex-sa-pkcs1.pem");
        let wrapped = pem_to_pkcs8(pkcs1_pem).expect("wrapped");
        let message = b"vertex-test";
        assert_eq!(sign(&pkcs8, message), sign(&wrapped, message));
    }

    #[test]
    fn a_service_account_assertion_names_the_account_and_the_token_url() {
        let pem = include_str!("../tests/fixtures/vertex-sa-pkcs8.pem");
        let creds = GoogleCreds {
            r#type: "service_account".to_owned(),
            client_id: String::new(),
            client_secret: String::new(),
            refresh_token: String::new(),
            client_email: "sa@example.com".to_owned(),
            private_key: pem.to_owned(),
            private_key_id: "key-1".to_owned(),
            service_account_impersonation_url: String::new(),
            delegates: Vec::new(),
            source_credentials: None,
            universe_domain: String::new(),
        };
        let jwt = service_assertion(&creds, "https://oauth2.googleapis.com/token").expect("jwt");
        let mut parts = jwt.split('.');
        let header = decode_part(parts.next().expect("header"));
        let claims = decode_part(parts.next().expect("claims"));
        assert!(parts.next().is_some());
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["kid"], "key-1");
        assert_eq!(claims["iss"], "sa@example.com");
        assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
        assert_eq!(claims["scope"], super::SCOPE);
        let iat = claims["iat"].as_i64().expect("iat");
        let exp = claims["exp"].as_i64().expect("exp");
        assert_eq!(exp - iat, 3600);
    }

    #[test]
    fn an_expire_time_parses_as_utc() {
        let parsed = parse_rfc3339_utc("2026-10-25T07:00:00Z").expect("time");
        let seconds = parsed
            .duration_since(std::time::UNIX_EPOCH)
            .expect("since")
            .as_secs();
        assert_eq!(seconds, 1_792_911_600);
        assert!(parse_rfc3339_utc("2026-10-25T07:00:00+00:00").is_none());
    }

    fn sign(pkcs8: &[u8], message: &[u8]) -> Vec<u8> {
        let key = RsaKeyPair::from_pkcs8(pkcs8).expect("key");
        let mut signature = vec![0u8; key.public().modulus_len()];
        key.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            message,
            &mut signature,
        )
        .expect("sign");
        signature
    }

    fn decode_part(part: &str) -> serde_json::Value {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .expect("b64");
        serde_json::from_slice(&bytes).expect("json")
    }
}
