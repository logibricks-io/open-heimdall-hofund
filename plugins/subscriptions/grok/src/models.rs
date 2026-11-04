//! Model list from the CLI backend.

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError, ProtocolId};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{Map, Value};

use crate::headers;
use crate::{Endpoints, refresh};

/// Reads `/models` and returns Responses models with context windows.
pub(crate) async fn list(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    program: impl FnOnce() -> Result<std::path::PathBuf, PluginError>,
) -> Result<ModelList, PluginError> {
    let stored = refresh::ensure_fresh(host, account, program, false).await?;
    let url = format!("{}/models", endpoints.base);
    let reply = signed_get(&url, &stored.credential.key)
        .fetch(host.http())
        .await?;
    match reply.status() {
        200..=299 => {
            let models = parse_list(reply.body())?;
            Ok(ModelList::new(models, None).with_source_url(url))
        }
        401 => Err(PluginError::auth(
            "the CLI backend refused the sign-in while listing models",
        )),
        status => Err(PluginError::status_error(
            status,
            "the CLI backend did not return a model list",
        )),
    }
}

fn signed_get(url: &str, token: &hofund_plugin_api::SecretString) -> Call {
    headers::client(token).into_iter().fold(
        Call::get(url).header("accept", "application/json"),
        |call, (name, value)| {
            if name.eq_ignore_ascii_case("authorization") {
                call.sensitive_header(name, value)
            } else {
                call.header(name, value)
            }
        },
    )
}

pub(crate) fn parse_list(body: &[u8]) -> Result<Vec<ModelSpec>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("the model list is not JSON"))?;
    let rows = root
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut models = Vec::new();
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let Some(id) = id else {
            continue;
        };
        if object
            .get("api_backend")
            .and_then(Value::as_str)
            .is_some_and(|backend| backend != "responses")
        {
            continue;
        }
        let mut efforts: Vec<String> = object
            .get("reasoning_efforts")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("value").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        efforts.reverse();
        models.extend(windows_of(id, object, &efforts)?);
    }
    if models.is_empty() {
        return Err(PluginError::decode(
            "the CLI backend listed no Responses models",
        ));
    }
    Ok(models)
}

fn windows_of(
    id: &str,
    row: &Map<String, Value>,
    efforts: &[String],
) -> Result<Vec<ModelSpec>, PluginError> {
    let name = row
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_owned();
    let fallback = row
        .get("context_window")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let listed: Vec<u64> = row
        .get("context_windows")
        .and_then(Value::as_array)
        .map(|windows| {
            windows
                .iter()
                .filter_map(Value::as_u64)
                .filter(|window| *window > 0)
                .collect::<Vec<_>>()
        })
        .filter(|windows| !windows.is_empty())
        .unwrap_or_else(|| {
            if fallback > 0 {
                vec![fallback]
            } else {
                Vec::new()
            }
        });
    let mut windows = listed;
    windows.sort_unstable();
    windows.dedup();
    let base = if windows.contains(&fallback) {
        fallback
    } else {
        windows.first().copied().unwrap_or(fallback)
    };
    let mut specs = Vec::new();
    specs.push(model_spec(id, &name, base.max(fallback), efforts)?);
    for window in windows {
        if window == base {
            continue;
        }
        let suffix = label_of(window);
        specs.push(model_spec(
            &format!("{id}@{suffix}"),
            &format!("{name} @ {suffix}"),
            window,
            efforts,
        )?);
    }
    Ok(specs)
}

fn model_spec(
    id: &str,
    name: &str,
    context: u64,
    efforts: &[String],
) -> Result<ModelSpec, PluginError> {
    Ok(ModelSpec::new(id, Some(name.to_owned()))?
        .with_context(context)
        .with_input_modalities(vec!["text".to_owned(), "image".to_owned()])
        .with_output_modalities(vec!["text".to_owned()])
        .with_image_input(Some(true))
        .with_endpoints(vec![ProtocolId::OPENAI_RESPONSES.as_str().to_owned()])
        .with_decides(!efforts.is_empty())
        .with_efforts(efforts.to_vec()))
}

fn label_of(window: u64) -> String {
    if window.is_multiple_of(1000) {
        format!("{}k", window / 1000)
    } else {
        window.to_string()
    }
}
