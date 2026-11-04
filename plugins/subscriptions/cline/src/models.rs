//! Cline model feeds: recommended models and optional cloud catalog.

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::Value;

use crate::Endpoints;
use crate::api;
use crate::tokens::Bearer;

const MODEL_ID: &str = r"^[a-z0-9][a-z0-9_.-]*/[a-z0-9][a-z0-9_.-]*$";

/// One model entry from a feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FeedModel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) free: bool,
    pub(crate) pass: bool,
    pub(crate) context: Option<u64>,
}

/// Parses Cline's recommended-models payload.
pub(crate) fn parse_feed(value: &Value) -> Vec<FeedModel> {
    let payload = match value {
        Value::Object(map) => map,
        _ => return Vec::new(),
    };
    let mut by_id: std::collections::BTreeMap<String, FeedModel> =
        std::collections::BTreeMap::new();
    for group in ["recommended", "free", "clinePass"] {
        let free = group == "free";
        let pass = group == "clinePass";
        let Some(list) = payload.get(group).and_then(Value::as_array) else {
            continue;
        };
        for entry in list {
            if let Some(model) = entry_model(entry, free, pass) {
                merge_model(&mut by_id, model);
            }
        }
    }
    by_id.into_values().collect()
}

fn entry_model(entry: &Value, group_free: bool, group_pass: bool) -> Option<FeedModel> {
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
    let free = group_free
        || entry
            .get("free")
            .is_some_and(|v| v.as_bool() == Some(true) || v.as_str() == Some("true"));
    let pass = group_pass || id.starts_with("cline-pass/");
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
    })
}

fn merge_model(by_id: &mut std::collections::BTreeMap<String, FeedModel>, model: FeedModel) {
    if let Some(was) = by_id.get_mut(&model.id) {
        if model.free && !was.free {
            was.free = true;
            was.name = model.name;
        }
        if model.pass {
            was.pass = true;
        }
        return;
    }
    by_id.insert(model.id.clone(), model);
}

fn valid_model_id(id: &str) -> bool {
    regex_lite(id, MODEL_ID)
}

fn regex_lite(text: &str, _pattern: &str) -> bool {
    // Lightweight check without pulling regex: split on `/` and validate segments.
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

/// Fetches recommended models and merges optional cloud catalog entries.
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
    let mut models = parse_feed(&rec_body);
    if let Ok(cloud) = fetch_cloud(host, endpoints).await {
        merge_cloud(&mut models, cloud);
    }
    if models.is_empty() {
        models = default_models();
    }
    let specs: Result<Vec<_>, _> = models
        .iter()
        .map(|model| ModelSpec::new(&model.id, Some(model.name.clone())))
        .collect();
    Ok(ModelList::new(specs?, None).with_source_url(url))
}

async fn fetch_cloud(
    host: &HostContext,
    endpoints: &Endpoints,
) -> Result<Vec<FeedModel>, PluginError> {
    let value = api::get(host, endpoints, "/ai/cline/models", None, false).await?;
    let empty = Vec::new();
    let list = match &value {
        Value::Array(items) => items,
        Value::Object(map) => map.get("data").and_then(Value::as_array).unwrap_or(&empty),
        _ => &empty,
    };
    Ok(list.iter().filter_map(cloud_entry).collect())
}

fn cloud_entry(entry: &Value) -> Option<FeedModel> {
    let id = entry.get("id").and_then(Value::as_str)?.trim();
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
        .filter(|name| name != id && name != last)
        .unwrap_or_else(|| prettify(id));
    let context = ["context_length", "context"]
        .iter()
        .find_map(|key| entry.get(*key))
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|n| n as u64)));
    Some(FeedModel {
        id: id.to_owned(),
        name,
        free: false,
        pass: id.starts_with("cline-pass/"),
        context,
    })
}

fn merge_cloud(models: &mut Vec<FeedModel>, cloud: Vec<FeedModel>) {
    let sizes: std::collections::HashMap<_, _> = cloud.iter().map(|m| (m.id.clone(), m)).collect();
    for model in models.iter_mut() {
        if let Some(c) = sizes.get(&model.id)
            && model.context.is_none()
        {
            model.context = c.context;
        }
    }
    let seen: std::collections::HashSet<_> = models.iter().map(|m| m.id.clone()).collect();
    for c in cloud {
        if !seen.contains(&c.id) {
            models.push(c);
        }
    }
}

fn default_models() -> Vec<FeedModel> {
    [
        "anthropic/claude-sonnet-5.5",
        "anthropic/claude-opus-5.5",
        "openai/gpt-6-astra",
        "cline-free/deepseek-v4.1-flash",
    ]
    .into_iter()
    .map(|id| FeedModel {
        id: id.to_owned(),
        name: prettify(id),
        free: id.starts_with("cline-free/"),
        pass: id.starts_with("cline-pass/"),
        context: None,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_sdk::json::json;

    #[test]
    fn parse_feed_merges_free_mark() {
        let payload = json!({
            "recommended": [{"id": "anthropic/claude-sonnet-5.5"}],
            "free": [{"id": "anthropic/claude-sonnet-5.5"}],
            "clinePass": [{"id": "cline-pass/mimo-v2.6-flash"}],
        });
        let models = parse_feed(&payload);
        assert_eq!(models.len(), 2);
        assert!(models[0].free);
        assert!(models[0].name.ends_with("(free)"));
        assert!(models[1].pass);
    }
}
