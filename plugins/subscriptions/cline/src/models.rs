//! Cline model feeds and their OpenRouter-style capabilities.

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::Value;

use crate::Endpoints;
use crate::api;
use crate::tokens::Bearer;

const MODEL_ID: &str = r"^[a-z0-9][a-z0-9_.-]*/[a-z0-9][a-z0-9_.-]*$";
const KINDS: [&str; 5] = ["text", "image", "audio", "video", "pdf"];

/// Input capabilities stated by the cloud catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Input {
    pub(crate) modalities: Vec<String>,
    pub(crate) image: bool,
}

/// One model entry from a feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FeedModel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) free: bool,
    pub(crate) pass: bool,
    pub(crate) context: Option<u64>,
    pub(crate) input: Option<Input>,
}

/// Parses Cline's recommended-models payload in feed order.
pub(crate) fn parse_feed(value: &Value) -> Vec<FeedModel> {
    let Value::Object(payload) = value else {
        return Vec::new();
    };
    let mut models = Vec::new();
    for group in ["recommended", "free", "clinePass"] {
        let Some(list) = payload.get(group).and_then(Value::as_array) else {
            continue;
        };
        for entry in list {
            if let Some(model) = entry_model(entry, group) {
                merge_feed_model(&mut models, model);
            }
        }
    }
    models
}

fn entry_model(entry: &Value, group: &str) -> Option<FeedModel> {
    let (id, given_name) = match entry {
        Value::String(text) => (text.trim().to_owned(), None),
        Value::Object(map) => {
            let id = ["id", "model", "modelId", "model_id", "key", "slug"]
                .iter()
                .find_map(|key| map.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty())?;
            let name = ["display_name", "displayName", "name", "label"]
                .iter()
                .find_map(|key| map.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned);
            (id.to_owned(), name)
        }
        _ => return None,
    };
    if !valid_model_id(&id) {
        return None;
    }
    let free = group == "free"
        || entry
            .get("free")
            .is_some_and(|v| v.as_bool() == Some(true) || v.as_str() == Some("true"));
    let pass = group == "clinePass" || id.starts_with("cline-pass/");
    let last = id.split('/').next_back().unwrap_or(&id);
    let name = given_name.filter(|name| name != &id && name != last);
    let name = name.unwrap_or_else(|| prettify(&id));
    let suffix = if free {
        " (free)"
    } else if pass {
        " (ClinePass)"
    } else {
        ""
    };
    Some(FeedModel {
        id: id.clone(),
        name: format!("{name}{suffix}"),
        free,
        pass,
        context: None,
        input: None,
    })
}

fn merge_feed_model(models: &mut Vec<FeedModel>, model: FeedModel) {
    if let Some(was) = models.iter_mut().find(|was| was.id == model.id) {
        if model.free && !was.free {
            was.free = true;
            was.name = model.name;
        }
        if model.pass {
            was.pass = true;
        }
        return;
    }
    models.push(model);
}

fn valid_model_id(id: &str) -> bool {
    regex_lite(id, MODEL_ID)
}

fn regex_lite(text: &str, _pattern: &str) -> bool {
    let Some((left, right)) = text.split_once('/') else {
        return false;
    };
    fn segment(part: &str) -> bool {
        let Some(first) = part.chars().next() else {
            return false;
        };
        first.is_ascii_alphanumeric()
            && part
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    }
    segment(left) && segment(right)
}

/// Turns `anthropic/claude-sonnet-5.5` into `Claude Sonnet 5.5`.
pub(crate) fn prettify(id: &str) -> String {
    let seg = id.split('/').next_back().unwrap_or(id);
    seg.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            match lower.as_str() {
                "gpt" | "glm" | "ai" | "api" | "llm" | "k3" => word.to_ascii_uppercase(),
                "mimo" => "MiMo".to_owned(),
                _ => {
                    let mut chars = word.chars();
                    let Some(first) = chars.next() else {
                        return String::new();
                    };
                    format!(
                        "{}{}",
                        first.to_ascii_uppercase(),
                        chars.as_str().to_ascii_lowercase()
                    )
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses the optional cloud catalog.
pub(crate) fn parse_cloud(value: &Value) -> Vec<FeedModel> {
    let list = match value {
        Value::Array(list) => list,
        Value::Object(map) => match map.get("data").and_then(Value::as_array) {
            Some(list) => list,
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    list.iter().filter_map(cloud_model).collect()
}

fn cloud_model(entry: &Value) -> Option<FeedModel> {
    let id = entry.get("id")?.as_str()?.trim();
    if id.is_empty() {
        return None;
    }
    let given = ["display_name", "displayName", "name"]
        .iter()
        .find_map(|key| entry.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let last = id.split('/').next_back().unwrap_or(id);
    let name = given
        .filter(|name| name != &id && name != last)
        .unwrap_or_else(|| prettify(id));
    let context = tokens(entry.get("context_length"))
        .or_else(|| tokens(entry.get("top_provider")?.get("context_length")))
        .or_else(|| tokens(entry.get("context")));
    Some(FeedModel {
        id: id.to_owned(),
        name,
        free: false,
        pass: id.starts_with("cline-pass/"),
        context,
        input: inputs_of(entry),
    })
}

fn tokens(value: Option<&Value>) -> Option<u64> {
    let number = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }?;
    (number.is_finite() && number > 0.0).then_some(number.floor() as u64)
}

fn inputs_of(entry: &Value) -> Option<Input> {
    let listed = entry
        .get("architecture")
        .and_then(|architecture| architecture.get("input_modalities"))
        .or_else(|| entry.get("input_modalities"))?
        .as_array()?;
    if listed.is_empty() {
        return None;
    }
    let names: Vec<String> = listed
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect();
    let image = names.iter().any(|name| name == "image");
    let modalities = KINDS
        .iter()
        .filter(|kind| names.iter().any(|name| name == *kind))
        .map(|kind| (*kind).to_owned())
        .collect();
    Some(Input { modalities, image })
}

fn bare_of(id: &str) -> String {
    id.split('/')
        .next_back()
        .unwrap_or(id)
        .split(':')
        .next()
        .unwrap_or(id)
        .to_ascii_lowercase()
}

/// Reads a model's input capabilities without guessing when cloud rows disagree.
pub(crate) fn inputs_for(id: &str, cloud: &[FeedModel]) -> Option<Input> {
    if let Some(own) = cloud.iter().find(|model| model.id == id) {
        return own.input.clone();
    }
    let bare = bare_of(id);
    let mut said = cloud
        .iter()
        .filter(|model| model.input.is_some() && bare_of(&model.id) == bare)
        .filter_map(|model| model.input.clone());
    let first = said.next()?;
    if said.any(|other| other != first) {
        return None;
    }
    Some(first)
}

/// Merges required recommended models with additive cloud models.
pub(crate) fn merge_feed(recommended: Vec<FeedModel>, cloud: Vec<FeedModel>) -> Vec<FeedModel> {
    let mut models: Vec<FeedModel> = recommended
        .into_iter()
        .map(|mut model| {
            if let Some(own) = cloud.iter().find(|entry| entry.id == model.id) {
                model.context = own.context;
            }
            model.input = inputs_for(&model.id, &cloud);
            model
        })
        .collect();
    for model in cloud {
        if !models.iter().any(|listed| listed.id == model.id) {
            models.push(model);
        }
    }
    models
}

/// Fetches both model feeds; only the recommended feed is required.
pub(crate) async fn list(
    host: &HostContext,
    endpoints: &Endpoints,
    bearer: &Bearer,
) -> Result<ModelList, PluginError> {
    let url = format!(
        "{}/ai/cline/recommended-models",
        endpoints.api.trim_end_matches('/')
    );
    let rec_body = api::get(
        host,
        endpoints,
        "/ai/cline/recommended-models",
        Some(&bearer.token),
        false,
    )
    .await?;
    let cloud = api::get(
        host,
        endpoints,
        "/ai/cline/models",
        Some(&bearer.token),
        false,
    )
    .await
    .map(|body| parse_cloud(&body))
    .unwrap_or_default();
    let models = merge_feed(parse_feed(&rec_body), cloud);
    if models.is_empty() {
        return Err(PluginError::config(
            "Cline returned no models for this account",
        ));
    }
    let specs: Result<Vec<_>, _> = models
        .iter()
        .map(|model| {
            let mut spec = ModelSpec::new(&model.id, Some(model.name.clone()))?
                .with_context(model.context.unwrap_or(0))
                .with_free(model.free);
            if let Some(input) = &model.input {
                spec = spec
                    .with_input_modalities(input.modalities.clone())
                    .with_image_input(Some(input.image));
            }
            Ok::<ModelSpec, PluginError>(spec)
        })
        .collect();
    Ok(ModelList::new(specs?, None).with_source_url(url))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_sdk::json::json;

    #[test]
    fn parse_feed_reads_all_groups_and_merges_the_free_mark() {
        let payload = json!({
            "recommended": [
                {"id": "anthropic/claude-sonnet-5.5"},
                {"id": "stealth/space-bunny-alpha"}
            ],
            "free": [{"id": "stealth/space-bunny-alpha", "name": "Space Bunny Alpha"}],
            "clinePass": [{"id": "cline-pass/mimo-v2.6-flash"}],
            "clineCloud": ["ignored/model"]
        });
        let models = parse_feed(&payload);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "anthropic/claude-sonnet-5.5");
        assert_eq!(models[1].name, "Space Bunny Alpha (free)");
        assert!(models[1].free);
        assert!(models[2].pass);
    }

    #[test]
    fn cloud_carries_windows_and_input_modalities() {
        let models = parse_cloud(&json!({"data": [
            {
                "id": "deepseek/deepseek-v4.1-flash",
                "context_length": "262144",
                "top_provider": {"max_completion_tokens": 32768},
                "architecture": {"input_modalities": ["text", "image", "file"]}
            },
            {"id": "local-model", "top_provider": {"context_length": 200000}}
        ]}));
        assert_eq!(models[0].context, Some(262144));
        assert_eq!(
            models[0].input.as_ref().map(|input| input.image),
            Some(true)
        );
        assert_eq!(
            models[0]
                .input
                .as_ref()
                .map(|input| input.modalities.as_slice()),
            Some(["text".to_owned(), "image".to_owned()].as_slice())
        );
        assert_eq!(models[1].context, Some(200000));
        assert!(models[1].input.is_none());
    }

    #[test]
    fn input_aliases_apply_only_when_every_cloud_row_agrees() {
        let cloud = parse_cloud(&json!([
            {"id": "a/m-1", "input_modalities": ["text", "image"]},
            {"id": "b/m-1:batch", "input_modalities": ["text", "image"]},
            {"id": "a/text-only", "input_modalities": ["text"]},
            {"id": "b/text-only", "input_modalities": ["text", "image"]}
        ]));
        assert_eq!(
            inputs_for("cline-pass/m-1", &cloud).map(|input| input.image),
            Some(true)
        );
        assert!(inputs_for("cline-pass/text-only", &cloud).is_none());
        assert!(inputs_for("a/m-1", &cloud).is_some());
        assert!(inputs_for("a/text-only", &cloud).is_some_and(|input| !input.image));
    }

    #[test]
    fn merge_keeps_recommended_first_and_adds_new_cloud_models() {
        let recommended = parse_feed(&json!({
            "recommended": [{"id": "anthropic/claude-sonnet-5.5"}],
            "free": [{"id": "cline-free/mimo-v2.6-flash"}]
        }));
        let cloud = parse_cloud(&json!([
            {"id": "anthropic/claude-sonnet-5.5", "context_length": 1000000},
            {"id": "local-model", "display_name": "Local Model"}
        ]));
        let models = merge_feed(recommended, cloud);
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            [
                "anthropic/claude-sonnet-5.5",
                "cline-free/mimo-v2.6-flash",
                "local-model"
            ]
        );
        assert_eq!(models[0].context, Some(1000000));
        assert_eq!(models[2].name, "Local Model");
    }
}
