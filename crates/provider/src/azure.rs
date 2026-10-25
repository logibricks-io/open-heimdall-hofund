//! Azure OpenAI deployment names.
//!
//! The v1 API takes a deployment's name as the model id. That name is not a
//! model id from a catalog: the resource owner chooses it. The data-plane
//! list that still answers an `api-key` is
//! `GET {resource}/openai/deployments?api-version=2022-12-01`
//! (<https://learn.microsoft.com/en-us/rest/api/azureopenai/data-plane/deployments/list?view=rest-azureopenai-dataplane-2022-12-01>,
//! checked 2026-10-25). Later versions of that list moved to the control
//! plane. When the data plane refuses, the v1 model list is the fallback:
//! `GET {resource}/openai/v1/models`
//! (<https://learn.microsoft.com/en-us/azure/foundry/openai/api-version-lifecycle>).
//! The key is the `api-key` header, never `Authorization`
//! (<https://learn.microsoft.com/en-us/azure/ai-services/openai/reference>).
//!
//! The gateway does not call this on a schedule. That refresh is the live
//! model list.

use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;

use crate::error::ProviderError;

const DEPLOYMENTS_VERSION: &str = "2022-12-01";
const PAGE_LIMIT: u32 = 1000;
const MAX_PAGES: usize = 20;

/// One deployment the resource accepts as a model id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AzureDeployment {
    /// Deployment name. This is the `model` field of a chat or responses call.
    pub id: String,
    /// Underlying model, when the deployments list named one. Empty when the
    /// id came from the v1 model list.
    pub model: String,
}

/// Lists the deployments at `base_url`.
///
/// `base_url` is the v1 root, `https://<host>/openai/v1`, including a test
/// server's equivalent. The first request is
/// `{root}/deployments?api-version=2022-12-01`, where `root` is `base_url`
/// without a trailing `/v1`. A failure or an empty list tries `{base_url}/models`.
/// A deployment whose id names an embedding, image, audio, or similar model
/// is left out. Both requests send `api-key` and no bearer token.
/// `extra_headers` replace a header of the same name.
///
/// # Errors
///
/// Returns [`ProviderError`] when neither list can be read. The message says
/// to type the deployment names in as model ids.
pub async fn list_deployments(
    base_url: &str,
    api_key: &str,
    extra_headers: &[(&str, &str)],
) -> Result<Vec<AzureDeployment>, ProviderError> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(ProviderError::decode("azure base URL is empty"));
    }
    let root = base
        .strip_suffix("/v1")
        .unwrap_or(base)
        .trim_end_matches('/');
    let deployments = format!("{root}/deployments?api-version={DEPLOYMENTS_VERSION}");
    let models = format!("{base}/models");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|error| ProviderError::connect(error.to_string()))?;
    let headers = request_headers(api_key, extra_headers)?;
    let mut errors = Vec::new();
    for url in [deployments, models] {
        match fetch_list(&client, &url, &headers).await {
            Ok(rows) => return Ok(rows),
            Err(error) => errors.push(error.message),
        }
    }
    Err(ProviderError::decode(format!(
        "{} — type your deployments' names in as model ids",
        errors.join("; ")
    )))
}

fn request_headers(
    api_key: &str,
    extra_headers: &[(&str, &str)],
) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    if !api_key.is_empty() {
        headers.insert(
            HeaderName::from_static("api-key"),
            HeaderValue::try_from(api_key)
                .map_err(|_| ProviderError::decode("azure api key is not a valid header value"))?,
        );
    }
    for (name, value) in extra_headers {
        let header_name = HeaderName::try_from(*name)
            .map_err(|_| ProviderError::decode(format!("invalid header name `{name}`")))?;
        let header_value = HeaderValue::try_from(*value)
            .map_err(|_| ProviderError::decode(format!("invalid header value for `{name}`")))?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

async fn fetch_list(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<Vec<AzureDeployment>, ProviderError> {
    let mut out = Vec::new();
    let mut after = String::new();
    for page in 0..MAX_PAGES {
        let at = page_url(url, &after)?;
        let (rows, has_more, last) = fetch_page(client, &at, headers).await?;
        out.extend(rows);
        if !has_more {
            if out.is_empty() {
                return Err(ProviderError::decode(format!("{url}: no models listed")));
            }
            return Ok(out);
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

async fn fetch_page(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<(Vec<AzureDeployment>, bool, String), ProviderError> {
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
    let body = response
        .text()
        .await
        .map_err(|error| ProviderError::connect(error.to_string()))?;
    if !status.is_success() {
        return Err(status_error(url, status.as_u16(), &body));
    }
    let parsed: ListBody = serde_json::from_str(&body)
        .map_err(|_| ProviderError::decode(format!("{url}: not a model list")))?;
    let rows = if parsed.data.as_ref().is_some_and(|rows| !rows.is_empty()) {
        parsed.data.unwrap_or_default()
    } else {
        parsed.models.or(parsed.data).unwrap_or_default()
    };
    let mut out = Vec::new();
    for row in rows {
        let Some(id) = deployment_id(&row) else {
            continue;
        };
        if !chat_deployment(&id, row.modalities.as_ref()) {
            continue;
        }
        out.push(AzureDeployment {
            id,
            model: row.model.unwrap_or_default(),
        });
    }
    let last = parsed.last_id.unwrap_or_default();
    Ok((out, parsed.has_more.unwrap_or(false), last))
}

fn deployment_id(row: &Row) -> Option<String> {
    let id = row
        .id
        .as_deref()
        .filter(|id| !id.is_empty())
        .or_else(|| row.name.as_deref().filter(|name| !name.is_empty()))?;
    Some(id.to_owned())
}

fn chat_deployment(id: &str, modalities: Option<&Modalities>) -> bool {
    if let Some(output) = modalities.map(|item| item.output.as_slice())
        && !output.is_empty()
        && !output.iter().any(|item| item == "text")
    {
        return false;
    }
    let id = id.to_ascii_lowercase();
    const SKIP: &[&str] = &[
        "embed",
        "-tts",
        "image",
        "audio",
        "-live",
        "robotics",
        "computer-use",
        "deep-research",
        "transcribe",
        "realtime",
        "moderation",
        "whisper",
        "dall-e",
        "sora",
    ];
    !SKIP.iter().any(|needle| id.contains(needle))
}

fn status_error(url: &str, status: u16, body: &str) -> ProviderError {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .filter(|message| !message.is_empty())
                .map(str::to_owned)
        });
    let message = match detail {
        Some(detail) => format!("{url}: {status} ({detail})"),
        None => format!("{url}: {status}"),
    };
    ProviderError::status(status, message)
}

#[derive(Debug, Deserialize)]
struct ListBody {
    data: Option<Vec<Row>>,
    models: Option<Vec<Row>>,
    #[serde(default)]
    has_more: Option<bool>,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Row {
    id: Option<String>,
    name: Option<String>,
    model: Option<String>,
    modalities: Option<Modalities>,
}

#[derive(Debug, Deserialize)]
struct Modalities {
    #[serde(default)]
    output: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_keep_deployment_names_and_drop_other_kinds() {
        assert!(chat_deployment("team-gpt", None));
        assert!(chat_deployment("gpt-5-codex", None));
        assert!(!chat_deployment("text-embedding-3-small", None));
        assert!(!chat_deployment("dall-e-3", None));
        assert!(!chat_deployment(
            "caption",
            Some(&Modalities {
                output: vec!["image".to_owned()],
            })
        ));
        assert!(chat_deployment(
            "caption",
            Some(&Modalities {
                output: vec!["text".to_owned(), "image".to_owned()],
            })
        ));
    }
}
