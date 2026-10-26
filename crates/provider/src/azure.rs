//! Azure OpenAI deployment names and base-model fallback.

use hofund_plugin_api::{ListKind, ModelList, ModelSpec, PluginError, PluginErrorKind};
use hofund_plugin_sdk::{PageError, PageErrorKind, after_id};
use serde_json::Value;

use crate::error::{ProviderError, ProviderErrorKind};
use crate::http::Endpoint;
use crate::models::ModelClient;

const DEPLOYMENTS_VERSION: &str = "2022-12-01";

/// One deployment the resource accepts as a model id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AzureDeployment {
    /// Deployment name. This is the `model` field of a chat or responses call.
    pub id: String,
    /// Underlying model, when the deployments list named one. Empty when the
    /// id came from the v1 model list.
    pub model: String,
}

struct AzureList {
    list: ModelList,
    deployments: Vec<AzureDeployment>,
}

struct AzureFailure(PluginError);

impl From<PageError> for AzureFailure {
    fn from(error: PageError) -> Self {
        let message = error.to_string();
        if error.kind() == PageErrorKind::Budget {
            Self(PluginError::timeout(message))
        } else {
            Self(PluginError::decode(message))
        }
    }
}

/// Lists the deployments at `base_url`, falling back to its base-model list.
///
/// Both requests send `api-key` and no bearer token. `extra_headers` replace a
/// header of the same name. An adapter-owned call uses the same implementation
/// and its pooled endpoint client.
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
    let mut endpoint = Endpoint::new("azure", base_url);
    endpoint.api_key = api_key.to_owned();
    endpoint.headers = extra_headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    azure_list_inner(&endpoint)
        .await
        .map(|listed| listed.deployments)
        .map_err(provider_error)
}

pub(crate) async fn azure_list(endpoint: &Endpoint) -> Result<ModelList, PluginError> {
    azure_list_inner(endpoint).await.map(|listed| listed.list)
}

async fn azure_list_inner(endpoint: &Endpoint) -> Result<AzureList, PluginError> {
    let client = ModelClient::new(endpoint)?;
    let base = endpoint.base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(PluginError::decode("azure base URL is empty"));
    }
    let root = base
        .strip_suffix("/v1")
        .unwrap_or(base)
        .trim_end_matches('/');
    let deployments = format!("{root}/deployments?api-version={DEPLOYMENTS_VERSION}");
    let models = format!("{base}/models");
    let mut errors = Vec::new();
    for (index, url) in [deployments, models].into_iter().enumerate() {
        let result = after_id(&url, |at| {
            let client = client.clone();
            async move {
                let page = client
                    .get(
                        &at,
                        client.azure_headers().map_err(AzureFailure)?,
                        hofund_plugin_sdk::BODY_CAP,
                    )
                    .await
                    .map_err(AzureFailure)?;
                client.page_bytes(&at, page, false).map_err(AzureFailure)
            }
        })
        .await
        .map_err(|error| error.0);
        match result {
            Ok(rows) => {
                let (specs, deployments) = azure_rows(&rows, index == 0)?;
                if specs.is_empty() {
                    errors.push(PluginError::decode(format!(
                        "{}: no models listed",
                        client.safe_url(&url)
                    )));
                    continue;
                }
                let list = ModelList::new(specs, None)
                    .with_kind(ListKind::Deployments)
                    .with_base_model_fallback(index == 1)
                    .with_source_url(&url);
                return Ok(AzureList { list, deployments });
            }
            Err(error) => {
                let error = client.redact_error(error);
                let stop = stop_after_url_error(&error);
                errors.push(error);
                // reference: internal/provider/azure.go:149-158
                // In Rust, a dropped call future is the caller-cancellation signal.
                if stop {
                    break;
                }
            }
        }
    }
    Err(crate::models::join_errors(
        errors,
        "azure model list failed",
        " — type your deployments' names in as model ids",
    ))
}

fn stop_after_url_error(error: &PluginError) -> bool {
    error.kind() == PluginErrorKind::Cancelled
}

fn azure_rows(
    rows: &[Value],
    deployments: bool,
) -> Result<(Vec<ModelSpec>, Vec<AzureDeployment>), PluginError> {
    let mut specs = Vec::new();
    let mut names = Vec::new();
    for row in rows {
        let Some(spec) = crate::models::model_spec(row, false)? else {
            continue;
        };
        names.push(AzureDeployment {
            id: spec.id().to_owned(),
            model: if deployments {
                row.get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            } else {
                String::new()
            },
        });
        specs.push(spec);
    }
    Ok((specs, names))
}

fn provider_error(error: PluginError) -> ProviderError {
    let kind = match error.kind() {
        PluginErrorKind::Connect => ProviderErrorKind::Connect,
        PluginErrorKind::Timeout => ProviderErrorKind::Timeout,
        PluginErrorKind::Transport => ProviderErrorKind::Transport,
        PluginErrorKind::Status => ProviderErrorKind::Status,
        PluginErrorKind::Auth => ProviderErrorKind::Auth,
        PluginErrorKind::Quota => ProviderErrorKind::Quota,
        _ => ProviderErrorKind::Decode,
    };
    ProviderError {
        kind,
        message: error.message().to_owned(),
        status: error.status(),
        retry_after: error.retry_after(),
    }
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginError;

    use super::stop_after_url_error;

    #[test]
    fn a_cancelled_url_error_stops_before_the_second_url() {
        assert!(stop_after_url_error(&PluginError::cancelled("stop")));
        assert!(!stop_after_url_error(&PluginError::timeout(
            "try the fallback"
        )));
    }
}
