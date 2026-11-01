//! One tiny Codex backend request to start an allowance window.

use std::sync::Arc;

use hofund_plugin_api::{
    AccountId, HostContext, ModelList, ModelSpec, PluginError, ProbeQuery, ProtocolId,
    UpstreamProvider,
};

use crate::Endpoints;
use crate::models::{self, Prompts};
use crate::tokens::{self, Need};
use crate::upstream::CodexUpstream;

/// Chooses a model for warm-up from the account's own list.
pub(crate) fn pick_warmup_model(list: &ModelList) -> Result<String, PluginError> {
    let models = list.models();
    if models.is_empty() {
        return Err(PluginError::transport(
            "no models are known for this account; warm-up needs a model list first",
        ));
    }
    if let Some(cheapest) = cheapest_model(models) {
        return Ok(cheapest.id().to_owned());
    }
    if let Some(mini) = models.iter().find(|model| model.id().contains("mini")) {
        return Ok(mini.id().to_owned());
    }
    Ok(models[0].id().to_owned())
}

fn cheapest_model(models: &[ModelSpec]) -> Option<&ModelSpec> {
    let mut priced: Vec<_> = models
        .iter()
        .filter_map(|model| {
            let (input, output, cache) = model.prices();
            let input = input?;
            let output = output?;
            let cache = cache.unwrap_or(0);
            Some((model, input + output + cache))
        })
        .collect();
    if priced.is_empty() {
        return None;
    }
    priced.sort_by_key(|(_, cost)| *cost);
    Some(priced[0].0)
}

/// Sends one minimal request through the Codex backend.
pub(crate) async fn run(
    endpoints: &Arc<Endpoints>,
    prompts: &Arc<Prompts>,
    account: &AccountId,
    host: &HostContext,
) -> Result<(), PluginError> {
    let access = tokens::access(host, endpoints, account, Need::IfExpiring).await?;
    let list = models::list(host, endpoints, &access, prompts).await?;
    let model = pick_warmup_model(&list)?;
    let upstream = CodexUpstream::new(
        account.clone(),
        host.clone(),
        Arc::clone(endpoints),
        Arc::clone(prompts),
    );
    let query = ProbeQuery::new(ProtocolId::OPENAI_RESPONSES, &model)?;
    let request = query.minimal_request();
    let _completion = upstream
        .execute(request, ProtocolId::OPENAI_RESPONSES)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{ModelList, ModelSpec};

    use super::pick_warmup_model;

    fn spec(id: &str) -> ModelSpec {
        ModelSpec::new(id, None).expect("spec")
    }

    #[test]
    fn a3a_22_codex_warmup_picks_cheapest_priced_model() {
        let list = ModelList::new(
            vec![
                spec("gpt-5").with_prices(300, 600, 0),
                spec("gpt-5-mini").with_prices(100, 200, 0),
            ],
            None,
        );
        assert_eq!(pick_warmup_model(&list).expect("pick"), "gpt-5-mini");
    }

    #[test]
    fn a3a_22_codex_warmup_picks_mini_without_prices() {
        let list = ModelList::new(vec![spec("gpt-5"), spec("gpt-5-mini")], None);
        assert_eq!(pick_warmup_model(&list).expect("pick"), "gpt-5-mini");
    }

    #[test]
    fn a3a_22_codex_warmup_picks_first_when_no_mini() {
        let list = ModelList::new(vec![spec("alpha"), spec("beta")], None);
        assert_eq!(pick_warmup_model(&list).expect("pick"), "alpha");
    }

    #[test]
    fn a3a_24_codex_pick_warmup_model_empty_list_errors() {
        let list = ModelList::new(Vec::new(), None);
        let error = pick_warmup_model(&list).expect_err("empty");
        assert!(
            error
                .to_string()
                .contains("no models are known for this account"),
            "{error}"
        );
    }

    #[test]
    fn a3a_25_codex_warmup_cache_read_price_picks_lower_total_cost() {
        let list = ModelList::new(
            vec![
                spec("cheap-input").with_prices(10, 10, 1_000),
                spec("cheap-total").with_prices(100, 100, 10),
            ],
            None,
        );
        assert_eq!(pick_warmup_model(&list).expect("pick"), "cheap-total");
    }
}
