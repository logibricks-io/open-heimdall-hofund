//! Live model lists.
//!
//! A chat or responses root is asked at `GET {base}/models`
//! (<https://platform.openai.com/docs/api-reference/models/list>). A messages
//! root is also asked at `GET {base}/v1/models`
//! (<https://platform.claude.com/docs/en/api/models/list>). The Generative
//! Language API is `GET {base}/models` with `x-goog-api-key`, keeping models
//! that serve `generateContent`
//! (<https://ai.google.dev/api/models#method:-models.list>). Azure deployments
//! use [`crate::list_deployments`]. A preset that records no list to ask
//! returns its catalog ids and is not fetched here.

use std::collections::BTreeSet;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;

use crate::azure::list_deployments;
use crate::error::ProviderError;
use crate::http::{Credential, insert_credential};
use crate::presets::Wire;

/// Shortest interval a scheduled fetch accepts.
pub const MIN_REFRESH: Duration = Duration::from_secs(10 * 60);

/// How long a failed scheduled fetch waits before it is tried again, unless
/// the provider's own interval is shorter.
pub const LIST_RETRY: Duration = Duration::from_secs(15 * 60);

const PAGE_LIMIT: u32 = 1000;
const MAX_PAGES: usize = 20;
const BODY_LIMIT: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// What one provider asked the catalog to list.
pub struct ListQuery<'a> {
    /// API root. The list path is appended.
    pub base_url: &'a str,
    /// Protocol whose list to ask.
    pub wire: Wire,
    /// Header the API key uses.
    pub credential: Credential,
    /// API key. Empty when the endpoint does not use one.
    pub api_key: &'a str,
    /// Extra headers. A name already set by the list request is replaced.
    pub headers: &'a [(String, String)],
    /// Preset id. Empty when the provider has no preset.
    pub preset_id: &'a str,
}

/// Ids that [`diff_ids`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdDiff {
    /// Present in `after` and absent from `before`, in `after`'s order.
    pub added: Vec<String>,
    /// Present in `before` and absent from `after`, in `before`'s order.
    pub removed: Vec<String>,
}

/// Ids that arrived or left between two lists.
#[must_use]
pub fn diff_ids(before: &[String], after: &[String]) -> IdDiff {
    let before_set: BTreeSet<&str> = before.iter().map(String::as_str).collect();
    let after_set: BTreeSet<&str> = after.iter().map(String::as_str).collect();
    IdDiff {
        added: after
            .iter()
            .filter(|id| !before_set.contains(id.as_str()))
            .cloned()
            .collect(),
        removed: before
            .iter()
            .filter(|id| !after_set.contains(id.as_str()))
            .cloned()
            .collect(),
    }
}

/// Whether a scheduled fetch should run at `now_unix`.
///
/// `every` shorter than [`MIN_REFRESH`] never runs. A list fetched recently
/// waits out `every`. A failed attempt waits [`LIST_RETRY`], or `every` when
/// that is shorter. `fetched_unix` and `attempt_unix` are `0` when unset.
#[must_use]
pub fn refresh_due(
    every: Duration,
    fetched_unix: i64,
    attempt_unix: i64,
    attempt_ok: bool,
    now_unix: i64,
) -> bool {
    if every < MIN_REFRESH {
        return false;
    }
    let every_secs = secs(every);
    if fetched_unix > 0 && now_unix.saturating_sub(fetched_unix) < every_secs {
        return false;
    }
    let retry_secs = secs(every.min(LIST_RETRY));
    if !attempt_ok && attempt_unix > 0 && now_unix.saturating_sub(attempt_unix) < retry_secs {
        return false;
    }
    true
}

fn secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// Lists the models `query` can call.
///
/// # Errors
///
/// Returns [`ProviderError`] when no list URL answers with a model id. The
/// message names each URL and does not include the API key.
pub async fn list_models(query: &ListQuery<'_>) -> Result<Vec<String>, ProviderError> {
    if query.preset_id == "azure" || query.credential == Credential::ApiKey {
        let rows =
            list_deployments(query.base_url, query.api_key, &header_pairs(query.headers)).await?;
        return Ok(rows.into_iter().map(|row| row.id).collect());
    }
    if query.wire == Wire::Generative {
        return gemini_list(query).await;
    }
    openai_list(query).await
}

fn header_pairs(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

async fn openai_list(query: &ListQuery<'_>) -> Result<Vec<String>, ProviderError> {
    let urls = list_urls(query.base_url, query.wire == Wire::Messages);
    if urls.is_empty() {
        return Err(ProviderError::decode("provider base URL is empty"));
    }
    let client = client()?;
    let headers = list_headers(query)?;
    let mut errors = Vec::new();
    for url in urls {
        match fetch_pages(&client, &url, &headers).await {
            Ok(ids) => return Ok(ids),
            Err(error) => errors.push(error.message),
        }
    }
    Err(ProviderError::decode(format!(
        "{}; type its model ids in by hand",
        errors.join("; ")
    )))
}

async fn fetch_pages(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<Vec<String>, ProviderError> {
    let mut ids = Vec::new();
    let mut after = String::new();
    for page in 0..MAX_PAGES {
        let at = page_url(url, &after)?;
        let (rows, has_more, last) = fetch_openai_page(client, &at, headers).await?;
        push_ids(&mut ids, rows);
        if !has_more {
            if ids.is_empty() {
                return Err(ProviderError::decode(format!("{url}: no models listed")));
            }
            return Ok(ids);
        }
        if last.is_empty() || last == after {
            return Err(ProviderError::decode(format!(
                "{url}: only a page of the model list (last id `{last}`), not the whole list"
            )));
        }
        if page + 1 >= MAX_PAGES {
            return Err(ProviderError::decode(format!(
                "{url}: still paging after {MAX_PAGES} pages of the model list"
            )));
        }
        after = last;
    }
    Err(ProviderError::decode(format!(
        "{url}: still paging after {MAX_PAGES} pages of the model list"
    )))
}

fn page_url(list: &str, after: &str) -> Result<String, ProviderError> {
    if after.is_empty() {
        return Ok(list.to_owned());
    }
    let mut url = reqwest::Url::parse(list)
        .map_err(|error| ProviderError::decode(format!("{list}: not a URL to page: {error}")))?;
    url.query_pairs_mut()
        .append_pair("after_id", after)
        .append_pair("limit", &PAGE_LIMIT.to_string());
    Ok(url.to_string())
}

async fn fetch_openai_page(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<(Vec<String>, bool, String), ProviderError> {
    let body = get_json(client, url, headers).await?;
    let parsed: OpenAiPage = serde_json::from_str(&body)
        .map_err(|_| ProviderError::decode(format!("{url}: not a model list")))?;
    let rows = parsed
        .data
        .filter(|rows| !rows.is_empty())
        .or(parsed.models)
        .unwrap_or_default();
    let ids = rows.into_iter().filter_map(row_id).collect();
    Ok((
        ids,
        parsed.has_more.unwrap_or(false),
        parsed.last_id.unwrap_or_default(),
    ))
}

fn row_id(row: OpenAiRow) -> Option<String> {
    row.id
        .filter(|id| !id.is_empty())
        .or(row.name)
        .filter(|id| !id.is_empty())
}

async fn gemini_list(query: &ListQuery<'_>) -> Result<Vec<String>, ProviderError> {
    let base = query.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(ProviderError::decode("provider base URL is empty"));
    }
    let client = client()?;
    let headers = list_headers(query)?;
    let mut ids = Vec::new();
    let mut token = String::new();
    let first = gemini_url(base, "")?;
    for page in 0..MAX_PAGES {
        let url = gemini_url(base, &token)?;
        let (rows, next) = fetch_gemini_page(&client, &url, &headers).await?;
        push_ids(&mut ids, rows);
        if next.is_empty() {
            if ids.is_empty() {
                return Err(ProviderError::decode(format!("{first}: no models listed")));
            }
            return Ok(ids);
        }
        if next == token {
            return Err(ProviderError::decode(format!(
                "{first}: only a page of the model list, not the whole list"
            )));
        }
        if page + 1 >= MAX_PAGES {
            return Err(ProviderError::decode(format!(
                "{first}: still paging after {MAX_PAGES} pages of the model list"
            )));
        }
        token = next;
    }
    Err(ProviderError::decode(format!(
        "{first}: still paging after {MAX_PAGES} pages of the model list"
    )))
}

fn gemini_url(base: &str, token: &str) -> Result<String, ProviderError> {
    let mut url = reqwest::Url::parse(&format!("{base}/models")).map_err(|error| {
        ProviderError::decode(format!("{base}/models: not a URL to page: {error}"))
    })?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("pageSize", "1000");
        if !token.is_empty() {
            pairs.append_pair("pageToken", token);
        }
    }
    Ok(url.to_string())
}

async fn fetch_gemini_page(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<(Vec<String>, String), ProviderError> {
    let body = get_json(client, url, headers).await?;
    let parsed: GeminiPage = serde_json::from_str(&body)
        .map_err(|_| ProviderError::decode(format!("{url}: not a model list")))?;
    let mut ids = Vec::new();
    for model in parsed.models.unwrap_or_default() {
        let methods = model.supported_generation_methods.unwrap_or_default();
        if !methods.is_empty() && !methods.iter().any(|method| method == "generateContent") {
            continue;
        }
        let name = model.name.unwrap_or_default();
        let id = name
            .trim()
            .strip_prefix("models/")
            .unwrap_or(name.trim())
            .to_owned();
        if !id.is_empty() {
            ids.push(id);
        }
    }
    Ok((ids, parsed.next_page_token.unwrap_or_default()))
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<String, ProviderError> {
    let response = client
        .get(url)
        .headers(headers.clone())
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                ProviderError::timeout(error.to_string())
            } else {
                ProviderError::connect(error.to_string())
            }
        })?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| ProviderError::connect(error.to_string()))?;
    if bytes.len() > BODY_LIMIT {
        return Err(ProviderError::decode(format!(
            "{url}: model list is larger than {BODY_LIMIT} bytes"
        )));
    }
    let body = String::from_utf8_lossy(&bytes);
    if !status.is_success() {
        return Err(status_error(url, status.as_u16(), &body));
    }
    Ok(body.into_owned())
}

fn client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| ProviderError::connect(error.to_string()))
}

fn list_headers(query: &ListQuery<'_>) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    let (style, fallback) = match query.wire {
        Wire::Generative => (Credential::GoogApiKey, Credential::GoogApiKey),
        Wire::Messages => (query.credential, Credential::XApiKey),
        Wire::Chat | Wire::Responses | Wire::Vertex | Wire::Converse => {
            (query.credential, Credential::Bearer)
        }
    };
    insert_credential(&mut headers, query.api_key, style, fallback)?;
    if query.wire == Wire::Messages {
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
    }
    for (name, value) in query.headers {
        let header_name = HeaderName::try_from(name.as_str())
            .map_err(|_| ProviderError::decode(format!("invalid header name `{name}`")))?;
        let header_value = HeaderValue::try_from(value.as_str())
            .map_err(|_| ProviderError::decode(format!("invalid header value for `{name}`")))?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

fn push_ids(into: &mut Vec<String>, ids: Vec<String>) {
    for id in ids {
        if !id.is_empty() && !into.iter().any(|have| have == &id) {
            into.push(id);
        }
    }
}

fn list_urls(base: &str, anthropic: bool) -> Vec<String> {
    let base = base.trim().trim_end_matches('/').to_owned();
    if base.is_empty() {
        return Vec::new();
    }
    let mut urls = Vec::new();
    let mut add = |url: String| {
        if !urls.contains(&url) {
            urls.push(url);
        }
    };
    add(format!("{base}/models"));
    if anthropic || !versioned(&base) {
        add(format!("{base}/v1/models"));
        let mut root = base;
        for suffix in [
            "/anthropic",
            "/apps/anthropic",
            "/api/anthropic",
            "/v1",
            "/api",
            "/api/v1",
        ] {
            if let Some(stripped) = root.strip_suffix(suffix) {
                root = stripped.to_owned();
            }
        }
        add(format!("{root}/v1/models"));
        add(format!("{root}/models"));
    }
    urls
}

fn versioned(base: &str) -> bool {
    let rest = base.split_once("://").map_or(base, |(_, rest)| rest);
    let path = rest.split_once('/').map_or("", |(_, path)| path);
    let path = path.split(['?', '#']).next().unwrap_or(path);
    path.split('/').any(|segment| {
        let mut chars = segment.chars();
        matches!(chars.next(), Some('v' | 'V'))
            && matches!(chars.next(), Some(digit) if digit.is_ascii_digit())
    })
}

fn status_error(url: &str, status: u16, body: &str) -> ProviderError {
    let message = match api_message(body) {
        Some(detail) => format!("{url}: {status} ({detail})"),
        None => format!("{url}: {status}"),
    };
    ProviderError::status(status, message)
}

fn api_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let message = value
        .pointer("/error/message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("error").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("message").and_then(serde_json::Value::as_str))
        .or_else(|| value.get("msg").and_then(serde_json::Value::as_str))
        .filter(|message| !message.is_empty())?;
    Some(message.to_owned())
}

#[derive(Debug, Deserialize)]
struct OpenAiPage {
    data: Option<Vec<OpenAiRow>>,
    models: Option<Vec<OpenAiRow>>,
    #[serde(default)]
    has_more: Option<bool>,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiRow {
    id: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiPage {
    models: Option<Vec<GeminiModel>>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GeminiModel {
    name: Option<String>,
    #[serde(default, rename = "supportedGenerationMethods")]
    supported_generation_methods: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::{LIST_RETRY, MIN_REFRESH, diff_ids, list_urls, refresh_due};

    #[test]
    fn a_versioned_root_is_asked_once_and_a_host_is_asked_at_v1() {
        let openai = list_urls("https://api.openai.com/v1", false);
        assert_eq!(openai, ["https://api.openai.com/v1/models"]);
        let anthropic = list_urls("https://api.anthropic.com", true);
        assert_eq!(
            anthropic,
            [
                "https://api.anthropic.com/models",
                "https://api.anthropic.com/v1/models"
            ]
        );
    }

    #[test]
    fn the_diff_keeps_list_order() {
        let diff = diff_ids(
            &["a".to_owned(), "b".to_owned()],
            &["b".to_owned(), "c".to_owned()],
        );
        assert_eq!(diff.added, ["c"]);
        assert_eq!(diff.removed, ["a"]);
    }

    #[test]
    fn a_recent_list_waits_and_a_failure_backs_off() {
        let every = MIN_REFRESH;
        let now = 1_000_000;
        assert!(!refresh_due(
            every - std::time::Duration::from_secs(1),
            0,
            0,
            false,
            now
        ));
        assert!(refresh_due(every, 0, 0, true, now));
        assert!(!refresh_due(every, now - 60, now - 60, true, now));
        assert!(refresh_due(every, now - 601, now - 601, true, now));
        assert!(!refresh_due(every, 0, now - 60, false, now));
        assert!(refresh_due(
            every,
            0,
            now - i64::try_from(LIST_RETRY.as_secs()).expect("secs") - 1,
            false,
            now
        ));
    }
}
