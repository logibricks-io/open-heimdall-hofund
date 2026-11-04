//! GLM model list from the plan Anthropic-compatible API.

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::secret_text;

use crate::session::Stored;
use crate::{APP_VERSION, Endpoints};

pub(crate) async fn list(
    host: &HostContext,
    endpoints: &Endpoints,
    stored: &Stored,
) -> Result<ModelList, PluginError> {
    let _ = endpoints;
    let url = format!("{}/v1/models", stored.auth.base.trim_end_matches('/'));
    let reply = Call::get(url.clone())
        .header("accept", "application/json")
        .header("user-agent", format!("ZCode/{APP_VERSION}"))
        .sensitive_header("authorization", secret_text(&stored.api_key))
        .fetch(host.http())
        .await?;
    if reply.status() == 401 || reply.status() == 403 {
        return Err(PluginError::auth(
            "Z Code refused the plan key while listing models",
        ));
    }
    if !(200..300).contains(&reply.status()) {
        return Err(PluginError::status_error(
            reply.status(),
            "Z Code did not return a model list",
        ));
    }
    let models = parse_list(reply.body())?;
    Ok(ModelList::new(models, None).with_source_url(url))
}

pub(crate) fn parse_list(body: &[u8]) -> Result<Vec<ModelSpec>, PluginError> {
    let root: Value = json::from_slice(body)
        .map_err(|_| PluginError::decode("the Z Code model list is not JSON"))?;
    let rows = root
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| root.get("models").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let id = object
            .get("id")
            .or_else(|| object.get("model"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let Some(id) = id else {
            continue;
        };
        out.push(
            ModelSpec::new(id.to_owned(), None)?
                .with_endpoints(vec!["anthropic-messages".to_owned()]),
        );
    }
    if out.is_empty() {
        return Err(PluginError::decode(
            "the Z Code model list named no Anthropic models",
        ));
    }
    Ok(out)
}
