//! GLM model lists from provider config and live gift balances.

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError, SecretString};
#[cfg(test)]
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::TokenCache;

use crate::session::Stored;
use crate::{Endpoints, gift, provider_config};

pub(crate) async fn list(
    host: &HostContext,
    endpoints: &Endpoints,
    _account: &AccountId,
    stored: &Stored,
    device_file: &std::path::Path,
    _team_keys: &TokenCache<SecretString>,
) -> Result<ModelList, PluginError> {
    let start_only = stored.auth.api_key.is_none() && !stored.auth.team();
    let loaded = provider_config::load(host, endpoints, stored).await;
    let plan = if start_only {
        "account:zai-start-plan"
    } else if stored.auth.base.contains("bigmodel.cn") {
        "account:bigmodel-individual-coding-plan"
    } else {
        "account:zai-individual-coding-plan"
    };
    let configured = loaded
        .as_ref()
        .map(|loaded| provider_config::models(&loaded.config, plan))
        .transpose()?
        .unwrap_or_default();
    let mut models = if configured.is_empty() {
        let ids: &[&str] = if start_only {
            &["GLM-5.3-Flash", "GLM-5.2", "GLM-5-Turbo"]
        } else {
            &["GLM-5.3", "GLM-5.3-Flash", "GLM-5.2", "GLM-5-Turbo"]
        };
        ids.iter()
            .map(|id| fallback(id))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        configured
    };
    if stored.auth.jwt.is_some()
        && let Ok(balance) = gift::read(host, endpoints, stored, device_file).await
    {
        for model in balance.all_models {
            let about = loaded
                .as_ref()
                .and_then(|loaded| provider_config::about(&loaded.config, &model));
            if start_only {
                push_model(&mut models, about.clone().unwrap_or(fallback(&model)?));
            }
            let id = gift::gift_id(&model);
            let mut trial = about.unwrap_or(fallback(&model)?);
            trial = ModelSpec::new(id, trial.display_name().map(str::to_owned))?
                .with_context(trial.context())
                .with_max_output(trial.max_output())
                .with_image_input(trial.image_input())
                .with_input_modalities(trial.input_modalities().to_vec())
                .with_output_modalities(trial.output_modalities().to_vec())
                .with_efforts(trial.efforts().to_vec())
                .with_endpoints(vec!["anthropic-messages".to_owned()]);
            push_model(&mut models, trial);
        }
    }
    if models.is_empty() {
        return Err(PluginError::decode(
            "the Z Code account has no models in its plan or gift balance",
        ));
    }
    let mut list = ModelList::new(models, None);
    if let Some(source) = loaded.and_then(|loaded| loaded.source_url) {
        list = list.with_source_url(source);
    }
    Ok(list)
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

#[cfg(test)]
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
