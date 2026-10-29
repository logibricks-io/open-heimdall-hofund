//! Bounded live model lists and shared row metadata.

use std::time::Duration;

use futures_util::StreamExt;
use hofund_plugin_api::{
    AuthChoice, ListKind, ListQuery, ModelList, ModelSpec, PluginError, PluginErrorKind,
    redact_token, safe_url,
};
use hofund_plugin_sdk::{
    PageError, PageErrorKind, after_id, gemini_models_with_dropped, json_error_message,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::http::{Credential, Endpoint};

const VERSION: &str = "2023-06-01";

// reference: internal/catalog/authheader.go:20-41
const BOTH_AUTH_REFUSALS: &[&str] = &[
    "\u{540c}\u{65f6}",
    "\u{5e76}\u{5b58}",
    "\u{5171}\u{5b58}",
    "\u{53ea}\u{80fd}\u{4f7f}\u{7528}\u{4e00}\u{4e2a}",
    "\u{53ea}\u{80fd}\u{9009}\u{62e9}\u{4e00}\u{4e2a}",
    "\u{53ea}\u{80fd}\u{6709}\u{4e00}\u{4e2a}",
    "cannot both",
    "can't both",
    "can not both",
    "cannot be both",
    "not both",
    "together",
    "at the same time",
    "simultaneous",
    "mutually exclusive",
    "only one of",
    "only one auth",
    "only one header",
    "both present",
    "both be present",
    "both provided",
    "both be provided",
];

#[derive(Clone, Debug)]
pub(crate) struct ModelClient {
    pub endpoint: Endpoint,
    pub client: reqwest::Client,
    pub mantle: bool,
}

struct ListFailure(PluginError);

impl From<PageError> for ListFailure {
    fn from(error: PageError) -> Self {
        let message = error.to_string();
        if error.kind() == PageErrorKind::Budget {
            Self(PluginError::timeout(message))
        } else {
            Self(PluginError::decode(message))
        }
    }
}

pub(crate) struct Page {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

impl ModelClient {
    pub fn new(endpoint: &Endpoint) -> Result<Self, PluginError> {
        let client = match endpoint.client.clone() {
            Some(client) => client,
            None => reqwest::Client::builder()
                .connect_timeout(endpoint.timeout)
                .read_timeout(crate::http::socket_backstop(endpoint.timeout))
                .build()
                .map_err(|_| PluginError::connect("could not build the provider HTTP client"))?,
        };
        Ok(Self {
            endpoint: endpoint.clone(),
            client,
            mantle: crate::mantle::is_mantle(endpoint),
        })
    }

    pub async fn live_list(
        &self,
        query: &ListQuery,
        anthropic: bool,
    ) -> Result<ModelList, PluginError> {
        let azure = self.is_azure_host();
        let learning = anthropic && self.can_learn_auth();
        let mut errors = Vec::new();
        for candidate in list_urls(&self.endpoint.base_url, anthropic) {
            let learned = std::sync::Arc::new(std::sync::Mutex::new(None));
            let learned_for_page = std::sync::Arc::clone(&learned);
            let result = after_id(&candidate.url, |url| {
                let learned = std::sync::Arc::clone(&learned_for_page);
                async move {
                    self.fetch_list_page(
                        &url,
                        anthropic,
                        azure,
                        learning,
                        query.auth_choice(),
                        &learned,
                    )
                    .await
                    .map_err(ListFailure)
                }
            })
            .await
            .map_err(|error| error.0);
            let learned = *learned.lock().unwrap_or_else(|error| error.into_inner());
            match result {
                Ok(rows) => {
                    let (models, dropped) = model_specs(&rows, false)?;
                    if models.is_empty() {
                        errors.push(PluginError::decode(format!(
                            "{}: no models listed",
                            safe_url(&candidate.url)
                        )));
                        continue;
                    }
                    let mut list = ModelList::new(models, None)
                        .with_dropped(dropped)
                        .with_kind(ListKind::Live)
                        .with_v1_fallback(candidate.v1_fallback)
                        .with_source_url(&candidate.url);
                    if let Some(choice) = learned {
                        list = list.with_auth_choice(choice);
                    }
                    return Ok(list);
                }
                Err(error) => errors.push(self.redact_error(error)),
            }
        }
        Err(join_errors(errors, "model list failed", ""))
    }

    pub async fn gemini_list(&self) -> Result<ModelList, PluginError> {
        let (rows, method_dropped) =
            gemini_models_with_dropped(&self.endpoint.base_url, |url| async move {
                let headers = self
                    .auth_headers(AuthChoice::Both, false, true)
                    .map_err(ListFailure)?;
                let page = self.fetch_page(&url, headers).await.map_err(ListFailure)?;
                self.page_bytes(&url, page, false).map_err(ListFailure)
            })
            .await
            .map_err(|error| self.redact_error(error.0))?;
        let (models, mut dropped) = model_specs(&rows, true)?;
        dropped.extend(method_dropped);
        if models.is_empty() {
            return Err(PluginError::decode("no models listed"));
        }
        Ok(ModelList::new(models, None)
            .with_dropped(dropped)
            .with_kind(ListKind::Live)
            .with_source_url(format!(
                "{}/models",
                self.endpoint.base_url.trim().trim_end_matches('/')
            )))
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_list_page(
        &self,
        url: &str,
        anthropic: bool,
        azure: bool,
        learning: bool,
        supplied: Option<AuthChoice>,
        learned: &std::sync::Mutex<Option<AuthChoice>>,
    ) -> Result<Vec<u8>, PluginError> {
        let initial = if azure || (anthropic && self.is_bedrock()) {
            AuthChoice::XApiKey
        } else {
            supplied.unwrap_or(AuthChoice::Both)
        };
        let mut choice = initial;
        let mut page = self
            .fetch_page(url, self.auth_headers(choice, anthropic, false)?)
            .await?;
        if learning && initial == AuthChoice::Both && both_auth_refused(page.status, &page.body) {
            for single in [AuthChoice::XApiKey, AuthChoice::Bearer] {
                choice = single;
                page = self
                    .fetch_page(url, self.auth_headers(choice, anthropic, false)?)
                    .await?;
                if page.status != 401 && page.status != 403 {
                    *learned.lock().unwrap_or_else(|error| error.into_inner()) = Some(single);
                    break;
                }
            }
        }
        self.page_bytes(url, page, false)
    }

    async fn fetch_page(&self, url: &str, headers: HeaderMap) -> Result<Page, PluginError> {
        self.get(url, headers, hofund_plugin_sdk::BODY_CAP).await
    }

    pub async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
        cap: usize,
    ) -> Result<Page, PluginError> {
        let builder = self.client.get(url).headers(headers);
        let response = send(builder).await?;
        read_page(response, cap, true).await
    }

    pub fn page_bytes(&self, url: &str, page: Page, probe: bool) -> Result<Vec<u8>, PluginError> {
        // reference: internal/catalog/live.go:542 and internal/catalog/gemini_list.go:81
        if page.status == 200 || (probe && (200..300).contains(&page.status)) {
            return Ok(page.body);
        }
        let detail = if probe {
            probe_error_message(&page.body)
        } else {
            json_error_message(&page.body)
        };
        let mut message = format!("{}: HTTP {}", safe_url(url), page.status);
        if !detail.is_empty() {
            message.push_str(&format!(" ({detail})"));
        }
        message = self.redact(&message);
        message.push_str(&self.redact(&status_hint(&page.body)));
        let mut error = PluginError::status_error(page.status, message);
        if let Some(after) = page.retry_after {
            error = error.with_retry_after(after);
        }
        Err(error)
    }

    pub fn auth_headers(
        &self,
        choice: AuthChoice,
        anthropic: bool,
        gemini: bool,
    ) -> Result<HeaderMap, PluginError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        if anthropic {
            headers.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(VERSION),
            );
        }
        if !self.endpoint.api_key.is_empty() {
            if gemini {
                insert_secret(&mut headers, "x-goog-api-key", &self.endpoint.api_key)?;
            } else {
                match choice {
                    AuthChoice::Both => {
                        insert_secret(&mut headers, "x-api-key", &self.endpoint.api_key)?;
                        insert_secret(
                            &mut headers,
                            "authorization",
                            &format!("Bearer {}", self.endpoint.api_key),
                        )?;
                    }
                    AuthChoice::XApiKey => {
                        insert_secret(&mut headers, "x-api-key", &self.endpoint.api_key)?;
                    }
                    AuthChoice::Bearer => {
                        insert_secret(
                            &mut headers,
                            "authorization",
                            &format!("Bearer {}", self.endpoint.api_key),
                        )?;
                    }
                    _ => {}
                }
            }
        }
        self.user_headers(headers)
    }

    pub fn bearer_headers(&self) -> Result<HeaderMap, PluginError> {
        self.auth_headers(AuthChoice::Bearer, false, false)
    }

    pub fn user_headers(&self, mut headers: HeaderMap) -> Result<HeaderMap, PluginError> {
        for (name, value) in &self.endpoint.headers {
            let name = HeaderName::try_from(name.as_str())
                .map_err(|_| PluginError::config("provider header name is invalid"))?;
            let mut value = HeaderValue::try_from(value.as_str())
                .map_err(|_| PluginError::config("provider header value is invalid"))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }

    pub fn pinned_auth(&self) -> bool {
        self.endpoint.headers.iter().any(|(name, _)| {
            name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
        })
    }

    pub fn can_learn_auth(&self) -> bool {
        both_auth_applies(
            &url_host(&self.endpoint.base_url),
            !self.endpoint.api_key.is_empty(),
            !self.endpoint.base_url.trim().is_empty(),
            self.endpoint.credential,
            self.pinned_auth(),
        )
    }

    /// Whether the base is on an Azure resource host. Only the Messages
    /// adapter reaches one: Chat and Responses there go to the azure plugin.
    pub fn is_azure_host(&self) -> bool {
        azure_host(&url_host(&self.endpoint.base_url))
    }

    pub fn is_bedrock(&self) -> bool {
        crate::mantle::is_bedrock(&self.endpoint)
    }

    pub fn is_mantle(&self) -> bool {
        self.mantle
    }

    pub fn is_official_anthropic(&self) -> bool {
        anthropic_host(&url_host(&self.endpoint.base_url))
    }

    pub fn redact_error(&self, error: PluginError) -> PluginError {
        let message = self.redact(error.message());
        let mut clean = match error.kind() {
            PluginErrorKind::Connect => PluginError::connect(message),
            PluginErrorKind::Timeout => PluginError::timeout(message),
            PluginErrorKind::Transport => PluginError::transport(message),
            PluginErrorKind::Interrupted => PluginError::interrupted(message),
            PluginErrorKind::Status => {
                PluginError::status_error(error.status().unwrap_or(502), message)
            }
            PluginErrorKind::Decode => PluginError::decode(message),
            PluginErrorKind::Unsupported => PluginError::unsupported(message),
            PluginErrorKind::Auth => PluginError::auth(message),
            PluginErrorKind::Quota => PluginError::quota(message),
            PluginErrorKind::Config => PluginError::config(message),
            PluginErrorKind::Cancelled => PluginError::cancelled(message),
            PluginErrorKind::Io => PluginError::io(message),
            PluginErrorKind::Internal => PluginError::internal(message),
            _ => PluginError::decode(message),
        };
        if let Some(after) = error.retry_after() {
            clean = clean.with_retry_after(after);
        }
        clean
    }

    pub fn redact(&self, text: &str) -> String {
        let mut clean = text.to_owned();
        for secret in std::iter::once(&self.endpoint.api_key)
            .chain(self.endpoint.headers.iter().map(|(_, value)| value))
            .filter(|value| !value.is_empty())
        {
            clean = redact_token(&clean, secret);
        }
        clean
    }
}

pub(crate) struct Candidate {
    url: String,
    v1_fallback: bool,
}

// reference: internal/catalog/live.go:351-396
fn list_urls(base: &str, anthropic: bool) -> Vec<Candidate> {
    let base = base.trim().trim_end_matches('/');
    let mut candidates = Vec::new();
    let mut add = |url: String, v1_fallback: bool| {
        if !candidates
            .iter()
            .any(|candidate: &Candidate| candidate.url == url)
        {
            candidates.push(Candidate { url, v1_fallback });
        }
    };
    add(format!("{base}/models"), false);
    if anthropic || !versioned(base) {
        let direct_v1 = format!("{base}/v1/models");
        add(direct_v1, true);
        let mut root = base.to_owned();
        const SUFFIXES: &[&str] = &[
            "/anthropic",
            "/apps/anthropic",
            "/api/anthropic",
            "/v1",
            "/api",
            "/api/v1",
        ];
        // reference: internal/catalog/live.go:366-371
        for suffix in SUFFIXES {
            if let Some(stripped) = root.strip_suffix(suffix) {
                root = stripped.to_owned();
            }
        }
        add(format!("{root}/v1/models"), false);
        add(format!("{root}/models"), false);
    }
    candidates
}

fn versioned(base: &str) -> bool {
    reqwest::Url::parse(base).is_ok_and(|url| {
        url.path_segments().is_some_and(|segments| {
            segments.into_iter().any(|segment| {
                let mut chars = segment.chars();
                matches!(chars.next(), Some('v' | 'V'))
                    && chars.next().is_some_and(|digit| digit.is_ascii_digit())
            })
        })
    })
}

/// Whether Anthropic-compatible listing may learn a single auth header.
///
/// BothAuth requires a key and base, excludes Azure, the official Anthropic
/// host, Bedrock runtime and Mantle, and respects user-pinned auth headers.
#[must_use]
pub fn both_auth_applies(
    host: &str,
    api_key_present: bool,
    base_present: bool,
    credential: Credential,
    pinned_auth: bool,
) -> bool {
    let host = host.to_ascii_lowercase();
    let bedrock = crate::mantle::is_bedrock_host(&host);
    api_key_present
        && base_present
        && credential != Credential::ApiKey
        && !azure_host(&host)
        && !anthropic_host(&host)
        && !bedrock
        && !pinned_auth
}

/// Whether `host` is `anthropic.com` or one of its subdomains.
///
/// Lookalike hosts such as `evilanthropic.com` do not match.
#[must_use]
pub(crate) fn anthropic_host(host: &str) -> bool {
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    host == "anthropic.com" || host.ends_with(".anthropic.com")
}

/// Whether `host` is an Azure resource host, with an optional port.
///
/// Kept for the Messages adapter's `x-api-key` and BothAuth rules: an Azure
/// resource serves Anthropic Messages under `/anthropic`, which the azure
/// plugin's v1 root does not.
// reference: internal/provider/azure.go:27-53
#[must_use]
pub(crate) fn azure_host(host: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        ".openai.azure.com",
        ".cognitiveservices.azure.com",
        ".services.ai.azure.com",
    ];
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix) && host.len() > suffix.len())
}

pub(crate) fn url_host(base: &str) -> String {
    reqwest::Url::parse(base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

fn model_specs(rows: &[Value], gemini: bool) -> Result<(Vec<ModelSpec>, Vec<String>), PluginError> {
    let mut models = Vec::new();
    let mut dropped = Vec::new();
    for row in rows {
        let id = row.as_str().or_else(|| {
            row.get("id")
                .and_then(Value::as_str)
                .or_else(|| row.get("name").and_then(Value::as_str))
        });
        if let Some(model) = model_spec(row, gemini)? {
            models.push(model);
        } else if let Some(id) = id.filter(|id| !id.is_empty()) {
            dropped.push(id.to_owned());
        }
    }
    Ok((models, dropped))
}

/// Parses OpenAI-compatible or Anthropic model rows and reports dropped ids.
///
/// # Errors
///
/// Returns [`PluginError`] when a row has an invalid model id.
pub fn parse_model_rows(rows: &[Value]) -> Result<(Vec<ModelSpec>, Vec<String>), PluginError> {
    model_specs(rows, false)
}

pub(crate) fn model_spec(row: &Value, gemini: bool) -> Result<Option<ModelSpec>, PluginError> {
    if let Value::String(id) = row {
        if hofund_plugin_sdk::classify::classify_model(id, "").is_none() {
            return Ok(None);
        }
        return ModelSpec::new(id, Some(id.to_owned())).map(Some);
    }
    let Some(row) = row.as_object() else {
        return Ok(None);
    };
    let Some(id) = row
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| row.get("name").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };
    let kind = row.get("kind").and_then(Value::as_str).unwrap_or("");
    let Some(tags) = hofund_plugin_sdk::classify::classify_model(id, kind) else {
        return Ok(None);
    };
    // reference: internal/catalog/live.go:589-603 and internal/catalog/gemini_list.go:100-103
    let name = if gemini {
        row.get("displayName").and_then(Value::as_str)
    } else {
        row.get("display_name").and_then(Value::as_str).or_else(|| {
            row.get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.contains(": "))
        })
    }
    .filter(|name| !name.is_empty())
    .unwrap_or(id)
    .to_owned();
    let mut model = ModelSpec::new(id, Some(name))?
        .with_draws(tags.draws())
        .with_films(tags.films())
        .with_decides(tags.decides());
    if gemini {
        model = model
            .with_context(positive(row.get("inputTokenLimit")))
            .with_max_output(positive(row.get("outputTokenLimit")));
        return Ok(Some(model));
    }
    model = model
        .with_context(positive(row.get("context_length")))
        .with_max_output(positive(row.get("max_output_tokens")))
        .with_efforts(named_values(
            row.get("supported_reasoning_levels"),
            "effort",
        ))
        .with_service_tiers(named_values(row.get("service_tiers"), "id"));
    if let Some(search) = row.get("web_search").and_then(Value::as_str) {
        model = model.with_web_search(search);
    }
    if let Some(modalities) = row.get("modalities").and_then(Value::as_object) {
        let input = string_array(modalities.get("input"));
        let output = string_array(modalities.get("output"));
        if modalities.get("input").and_then(Value::as_array).is_some() {
            model = model.with_image_input(Some(input.iter().any(|item| item == "image")));
        }
        model = model
            .with_input_modalities(input)
            .with_output_modalities(output);
    }
    let native = string_array(row.get("native_endpoints"));
    let supported = string_array(row.get("supported_endpoints"));
    let endpoints = if !native.is_empty() {
        native
    } else if !supported.is_empty() {
        supported
    } else {
        string_array(row.get("type_target"))
    };
    model = model.with_endpoints(endpoints);
    Ok(Some(model))
}

fn positive(value: Option<&Value>) -> u64 {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= u64::MAX as f64)
        .map(|value| value as u64)
        .unwrap_or(0)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => vec![value.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn named_values(value: Option<&Value>, key: &str) -> Vec<String> {
    let mut values = Vec::new();
    for value in value.and_then(Value::as_array).into_iter().flatten() {
        let named = value
            .as_str()
            .or_else(|| value.get(key).and_then(Value::as_str));
        if let Some(named) = named.filter(|named| !named.is_empty())
            && !values.iter().any(|present| present == named)
        {
            values.push(named.to_owned());
        }
    }
    values
}

/// Whether a 401 body says the two Anthropic-compatible auth headers cannot be sent together.
#[must_use]
pub fn both_auth_refused(status: u16, body: &[u8]) -> bool {
    if status != 401 {
        return false;
    }
    let body = String::from_utf8_lossy(body).to_lowercase();
    body.contains("x-api-key")
        && body.contains("authorization")
        && BOTH_AUTH_REFUSALS
            .iter()
            .any(|phrase| body.contains(phrase))
}

async fn send(builder: reqwest::RequestBuilder) -> Result<reqwest::Response, PluginError> {
    builder.send().await.map_err(|error| {
        if error.is_timeout() {
            PluginError::timeout("upstream request exceeded its deadline")
        } else if error.is_connect() {
            PluginError::connect("could not connect to the upstream")
        } else {
            PluginError::transport("upstream transport failed")
        }
    })
}

pub(crate) async fn read_page(
    response: reqwest::Response,
    cap: usize,
    exact_cap: bool,
) -> Result<Page, PluginError> {
    let status = response.status().as_u16();
    let retry_after = crate::http::retry_after(response.headers());
    if exact_cap
        && response
            .content_length()
            .is_some_and(|length| length > cap as u64)
    {
        return Err(PluginError::decode("response body is too large"));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) if !(200..300).contains(&status) => break,
            Err(error) => {
                return Err(if error.is_timeout() {
                    PluginError::interrupted("upstream response timed out while reading")
                } else {
                    PluginError::interrupted("could not read the upstream response")
                });
            }
        };
        let remaining = cap
            .saturating_add(usize::from(exact_cap))
            .saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() > cap {
            return Err(PluginError::decode("response body is too large"));
        }
        if body.len() == cap && !exact_cap {
            break;
        }
    }
    Ok(Page {
        status,
        retry_after,
        body,
    })
}

pub(crate) fn probe_error_message(body: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        for pointer in [
            "/error/message",
            "/message",
            "/msg",
            "/detail",
            "/errors/0/message",
        ] {
            if let Some(message) = value.pointer(pointer).and_then(Value::as_str)
                && !message.trim().is_empty()
            {
                return cut_runes(message.trim(), 300);
            }
        }
        if let Some(message) = value.get("error").and_then(Value::as_str) {
            return cut_runes(message.trim(), 300);
        }
    }
    let text = String::from_utf8_lossy(body);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() || compact.starts_with('<') {
        String::new()
    } else {
        cut_runes(&compact, 300)
    }
}

fn cut_runes(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let mut cut = text.chars().take(cap).collect::<String>();
    cut.push('…');
    cut
}

const BLOCK_ADVICE: &str =
    "the provider's network edge blocked this address; wait, or try another network or proxy";
const VERIFY_ADVICE: &str = "this Google account needs verification";

fn status_hint(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_ascii_lowercase();
    if lower.contains("request has been blocked")
        || lower.contains("errors.aliyun.com")
        || lower.contains(&BLOCK_ADVICE.to_ascii_lowercase())
    {
        return format!(" — {BLOCK_ADVICE}");
    }
    // reference: internal/provider/test.go:690-727
    if let Some(link) = verification(body, &text, &lower) {
        return link.map_or_else(
            || format!(" — {VERIFY_ADVICE} before API access can work"),
            |link| {
                format!(
                    " — {VERIFY_ADVICE}: open {link} in a browser, verify the account, then try again"
                )
            },
        );
    }
    if lower.contains("go plan doesn") && lower.contains("include api access") {
        return " — this Go plan does not include Provider API access; use an API-enabled plan or a provider API key".to_owned();
    }
    String::new()
}

fn verification(body: &[u8], text: &str, lower: &str) -> Option<Option<String>> {
    let value: Option<Value> = serde_json::from_slice(body).ok();
    let details = value
        .as_ref()
        .and_then(|value| value.pointer("/error/details"))
        .and_then(Value::as_array);
    let mut required = false;
    let mut help = None;
    let mut validation = None;
    for detail in details.into_iter().flatten() {
        for link in detail
            .get("links")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if help.is_none() {
                help = link.get("url").and_then(Value::as_str).map(str::to_owned);
            }
        }
        if detail.get("reason").and_then(Value::as_str) == Some("VALIDATION_REQUIRED") {
            required = true;
            if validation.is_none() {
                validation = detail
                    .get("metadata")
                    .and_then(Value::as_object)
                    .and_then(|metadata| {
                        metadata
                            .get("validation_url")
                            .or_else(|| metadata.get("validationUrl"))
                    })
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
        }
    }
    let words = lower.contains("verify your account")
        || lower.contains("account verification required")
        || lower.contains(&VERIFY_ADVICE.to_ascii_lowercase());
    if !required && !words {
        return None;
    }
    let link = if required { validation.or(help) } else { None }
        .or_else(|| own_advice_link(text))
        .and_then(|link| {
            let url = reqwest::Url::parse(&link).ok()?;
            (url.scheme() == "https" && url.host_str().is_some()).then_some(link)
        });
    Some(link)
}

fn own_advice_link(text: &str) -> Option<String> {
    let start = text.find(VERIFY_ADVICE)? + VERIFY_ADVICE.len();
    let rest = text.get(start..)?.strip_prefix(": open ")?;
    let end = rest
        .find(|character: char| character.is_whitespace() || character == '"' || character == '\\')
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

/// Joins error messages, preserving their shared kind. For mixed kinds, the
/// last error supplies the kind, status, and retry hint.
pub(crate) fn join_errors(errors: Vec<PluginError>, fallback: &str, suffix: &str) -> PluginError {
    let Some(last) = errors.last() else {
        return PluginError::decode(format!("{fallback}{suffix}"));
    };
    let mut message = errors
        .iter()
        .map(|error| error.message())
        .collect::<Vec<_>>()
        .join("; ");
    message.push_str(suffix);
    let mut joined = match last.kind() {
        PluginErrorKind::Connect => PluginError::connect(message),
        PluginErrorKind::Timeout => PluginError::timeout(message),
        PluginErrorKind::Transport => PluginError::transport(message),
        PluginErrorKind::Interrupted => PluginError::interrupted(message),
        PluginErrorKind::Status => PluginError::status_error(last.status().unwrap_or(502), message),
        PluginErrorKind::Decode => PluginError::decode(message),
        PluginErrorKind::Unsupported => PluginError::unsupported(message),
        PluginErrorKind::Auth => PluginError::auth(message),
        PluginErrorKind::Quota => PluginError::quota(message),
        PluginErrorKind::Config => PluginError::config(message),
        PluginErrorKind::Cancelled => PluginError::cancelled(message),
        PluginErrorKind::Io => PluginError::io(message),
        PluginErrorKind::Internal => PluginError::internal(message),
        _ => PluginError::decode(message),
    };
    if let Some(after) = last.retry_after() {
        joined = joined.with_retry_after(after);
    }
    joined
}

fn insert_secret(
    headers: &mut HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), PluginError> {
    let mut value = HeaderValue::try_from(value)
        .map_err(|_| PluginError::config("API key is not valid header text"))?;
    value.set_sensitive(true);
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

pub(crate) fn static_models(ids: Vec<String>) -> Result<ModelList, PluginError> {
    let models = ids
        .into_iter()
        .map(|id| ModelSpec::new(id, None))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ModelList::new(models, None).with_kind(ListKind::Catalog))
}

#[cfg(test)]
mod tests {
    #[test]
    fn both_auth_refusal_keeps_escaped_non_english_markers() {
        let body = "x-api-key Authorization \u{540c}\u{65f6}";
        assert!(super::both_auth_refused(401, body.as_bytes()));
        assert!(!super::both_auth_refused(403, body.as_bytes()));
        assert!(!super::both_auth_refused(401, b"x-api-key only"));
    }

    #[test]
    fn anthropic_host_matches_the_domain_and_subdomains_only() {
        for host in [
            "anthropic.com",
            "api.anthropic.com",
            "API.Anthropic.com:443",
        ] {
            assert!(super::anthropic_host(host), "{host}");
        }
        for host in [
            "evilanthropic.com",
            "anthropic.com.example.test",
            "notanthropic.com",
            "",
        ] {
            assert!(!super::anthropic_host(host), "{host}");
        }
        assert!(!super::both_auth_applies(
            "api.anthropic.com",
            true,
            true,
            crate::Credential::Default,
            false,
        ));
        assert!(super::both_auth_applies(
            "evilanthropic.com",
            true,
            true,
            crate::Credential::Default,
            false,
        ));
        let lookalike = crate::http::Endpoint::new("lookalike", "https://evilanthropic.com/v1");
        assert!(
            !super::ModelClient::new(&lookalike)
                .expect("client")
                .is_official_anthropic()
        );
        let official = crate::http::Endpoint::new("official", "https://api.anthropic.com");
        assert!(
            super::ModelClient::new(&official)
                .expect("client")
                .is_official_anthropic()
        );
    }

    #[test]
    fn only_api_aws_mantle_hosts_are_bedrock() {
        let legacy = crate::http::Endpoint::new(
            "legacy",
            "https://bedrock-mantle.us-east-1.amazonaws.com/anthropic",
        );
        let client = super::ModelClient::new(&legacy).expect("client");
        assert!(!client.is_bedrock());
        assert!(super::both_auth_applies(
            "bedrock-mantle.us-east-1.amazonaws.com",
            true,
            true,
            crate::Credential::Default,
            false,
        ));
        for host in [
            "bedrock-mantle.us-east-1.api.aws",
            "bedrock-runtime.us-east-1.amazonaws.com",
        ] {
            assert!(!super::both_auth_applies(
                host,
                true,
                true,
                crate::Credential::Default,
                false,
            ));
        }
    }

    async fn truncated_page(status: u16) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("connection");
            let mut request = [0_u8; 2048];
            let _ = socket.read(&mut request).await;
            let status_line = match status {
                200 => "200 OK",
                429 => "429 Too Many Requests",
                _ => panic!("unsupported test status"),
            };
            let headers = format!(
                "HTTP/1.1 {status_line}\r\ncontent-length: 2\r\nretry-after: 120\r\nconnection: close\r\n\r\n"
            );
            socket.write_all(headers.as_bytes()).await.expect("headers");
            socket.write_all(b"{").await.expect("partial body");
        });
        (format!("http://{address}/models"), server)
    }

    #[tokio::test]
    async fn a_truncated_429_page_preserves_status_and_retry_after() {
        let (url, server) = truncated_page(429).await;
        let endpoint = crate::Endpoint::new("models", "http://127.0.0.1");
        let client = super::ModelClient::new(&endpoint).expect("client");
        let page = client
            .get(&url, reqwest::header::HeaderMap::new(), 1024)
            .await
            .expect("partial page");
        assert_eq!(page.body, b"{");
        let error = client.page_bytes(&url, page, false).expect_err("status");
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Status);
        assert_eq!(error.status(), Some(429));
        assert_eq!(
            error.retry_after(),
            Some(std::time::Duration::from_secs(120))
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn a_truncated_200_page_is_interrupted() {
        let (url, server) = truncated_page(200).await;
        let endpoint = crate::Endpoint::new("models", "http://127.0.0.1");
        let client = super::ModelClient::new(&endpoint).expect("client");
        let error = match client
            .get(&url, reqwest::header::HeaderMap::new(), 1024)
            .await
        {
            Ok(_) => panic!("expected an interrupted body"),
            Err(error) => error,
        };
        assert_eq!(
            error.kind(),
            hofund_plugin_api::PluginErrorKind::Interrupted
        );
        server.await.expect("server");
    }

    // reference: internal/provider/azure.go:27-53
    #[test]
    fn azure_hosts_keep_messages_on_x_api_key_and_chat_off_api_key() {
        assert!(super::azure_host("resource.openai.azure.com:443"));
        assert!(super::azure_host("Resource.Services.AI.Azure.com"));
        assert!(!super::azure_host("openai.azure.com"));
        assert!(!super::azure_host("evil-openai.azure.com.example"));
        let base = "https://resource.services.ai.azure.com/anthropic";
        assert_eq!(
            crate::credential_for("", crate::Wire::Messages, base),
            crate::Credential::XApiKey
        );
        for wire in [crate::Wire::Chat, crate::Wire::Responses] {
            assert_eq!(
                crate::credential_for("", wire, base),
                crate::Credential::Bearer
            );
        }
        assert!(!super::both_auth_applies(
            "resource.services.ai.azure.com",
            true,
            true,
            crate::Credential::MessagesBoth,
            false,
        ));
        let mut endpoint = crate::http::Endpoint::new("azure", base);
        endpoint.credential = crate::Credential::ApiKey;
        let client = super::ModelClient::new(&endpoint).expect("client");
        assert!(client.is_azure_host());
        endpoint.base_url = "https://api.example.test/v1".to_owned();
        let client = super::ModelClient::new(&endpoint).expect("client");
        assert!(!client.is_azure_host());
    }

    #[test]
    fn redaction_preserves_cancellation_and_hides_the_api_key() {
        let mut endpoint = crate::http::Endpoint::new("azure", "https://example.test/openai/v1");
        endpoint.api_key = "SECRET-API-KEY".to_owned();
        let client = super::ModelClient::new(&endpoint).expect("client");
        let error = client.redact_error(hofund_plugin_api::PluginError::cancelled(
            "cancel SECRET-API-KEY",
        ));
        assert_eq!(error.kind(), hofund_plugin_api::PluginErrorKind::Cancelled);
        assert_eq!(error.message(), "cancel [redacted]");
        assert!(!error.message().contains("SECRET-API-KEY"));
    }

    #[test]
    fn redaction_does_not_mask_secret_substrings() {
        let mut endpoint = crate::http::Endpoint::new("azure", "https://example.test/openai/v1");
        endpoint.api_key = "secret-token".to_owned();
        let client = super::ModelClient::new(&endpoint).expect("client");
        assert_eq!(
            client.redact("secret-token prefixsecret-token secret-tokensuffix"),
            "[redacted] prefixsecret-token secret-tokensuffix"
        );
    }

    #[test]
    fn redaction_ignores_short_header_values() {
        let mut endpoint = crate::http::Endpoint::new("azure", "https://example.test/openai/v1");
        endpoint
            .headers
            .push(("x-short".to_owned(), "tiny".to_owned()));
        let client = super::ModelClient::new(&endpoint).expect("client");
        assert_eq!(client.redact("tiny tinyish"), "tiny tinyish");
    }

    #[test]
    fn model_urls_hide_userinfo_and_short_query_values() {
        assert_eq!(
            super::safe_url("https://user:pass@example.test/models?key=tiny#fragment"),
            "https://example.test/models?<redacted>"
        );
    }

    #[test]
    fn joined_errors_preserve_every_shared_kind() {
        use hofund_plugin_api::{PluginError, PluginErrorKind};

        let cases = [
            (
                PluginErrorKind::Auth,
                PluginError::auth("first"),
                PluginError::auth("second"),
            ),
            (
                PluginErrorKind::Quota,
                PluginError::quota("first"),
                PluginError::quota("second"),
            ),
            (
                PluginErrorKind::Io,
                PluginError::io("first"),
                PluginError::io("second"),
            ),
            (
                PluginErrorKind::Config,
                PluginError::config("first"),
                PluginError::config("second"),
            ),
            (
                PluginErrorKind::Unsupported,
                PluginError::unsupported("first"),
                PluginError::unsupported("second"),
            ),
            (
                PluginErrorKind::Internal,
                PluginError::internal("first"),
                PluginError::internal("second"),
            ),
        ];
        for (kind, first, second) in cases {
            let joined = super::join_errors(vec![first, second], "fallback", "");
            assert_eq!(joined.kind(), kind);
            assert_eq!(joined.message(), "first; second");
        }
    }

    #[test]
    fn mixed_joined_errors_use_the_last_kind_status_and_retry_hint() {
        use hofund_plugin_api::{PluginError, PluginErrorKind};

        let last = PluginError::status_error(429, "second")
            .with_retry_after(std::time::Duration::from_secs(120));
        let joined = super::join_errors(vec![PluginError::auth("first"), last], "fallback", "");
        assert_eq!(joined.kind(), PluginErrorKind::Status);
        assert_eq!(joined.status(), Some(429));
        assert_eq!(
            joined.retry_after(),
            Some(std::time::Duration::from_secs(120))
        );
    }
}
