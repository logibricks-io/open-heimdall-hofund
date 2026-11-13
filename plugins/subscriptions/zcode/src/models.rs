//! GLM model list from the plan Anthropic-compatible API.

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{TokenCache, secret_text};

use crate::session::{self, Stored};
use crate::{APP_VERSION, Endpoints, gift};

pub(crate) async fn list(
    host: &HostContext,
    endpoints: &Endpoints,
    account: &AccountId,
    stored: &Stored,
    device_file: &std::path::Path,
    team_keys: &TokenCache<SecretString>,
) -> Result<ModelList, PluginError> {
    let start_only = stored.auth.api_key.is_none() && !stored.auth.team();
    let mut models = if start_only {
        Vec::new()
    } else {
        let url = format!("{}/v1/models", stored.auth.base.trim_end_matches('/'));
        let reply = Call::get(url.clone())
            .header("accept", "application/json")
            .header("user-agent", format!("ZCode/{APP_VERSION}"))
            .sensitive_header("authorization", secret_text(&stored.api_key))
            .fetch(host.http())
            .await?;
        if reply.status() == 401 || reply.status() == 403 {
            if stored.auth.team() && stored.auth.api_key.is_none() {
                session::drop_team_key_cache(team_keys, account);
            }
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
        parse_list(reply.body())?
    };
    if stored.auth.jwt.is_some()
        && let Ok(balance) = gift::read(host, endpoints, stored, device_file).await
    {
        if start_only && balance.all_models.is_empty() {
            for id in ["GLM-5.3-Flash", "GLM-5.2", "GLM-5-Turbo"] {
                models.push(fallback(id)?);
            }
        }
        for model in balance.all_models {
            if start_only {
                push_model(&mut models, fallback(&model)?);
            }
            push_model(&mut models, fallback(&gift::gift_id(&model))?);
        }
    }
    if models.is_empty() {
        return Err(PluginError::decode(
            "the Z Code account has no models in its plan or gift balance",
        ));
    }
    Ok(ModelList::new(models, None))
}

fn push_model(models: &mut Vec<ModelSpec>, model: ModelSpec) {
    if !models
        .iter()
        .any(|existing| existing.id().eq_ignore_ascii_case(model.id()))
    {
        models.push(model);
    }
}

fn fallback(id: &str) -> Result<ModelSpec, PluginError> {
    let (context, output, efforts) = match id {
        "GLM-5.3" | "GLM-5.3-Flash" => (
            1_000_000,
            128_000,
            vec!["low".to_owned(), "high".to_owned(), "max".to_owned()],
        ),
        "GLM-5.2" => (
            1_000_000,
            128_000,
            vec!["none".to_owned(), "high".to_owned(), "max".to_owned()],
        ),
        "GLM-5-Turbo" => (200_000, 64_000, vec!["none".to_owned(), "high".to_owned()]),
        _ => (0, 0, Vec::new()),
    };
    Ok(ModelSpec::new(gift::canonical_model(id), None)?
        .with_context(context)
        .with_max_output(output)
        .with_efforts(efforts)
        .with_endpoints(vec!["anthropic-messages".to_owned()]))
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
