//! Parse the models.dev `api.json` provider map.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::spec::{LEAST_WINDOW, ModelSpec, Price};

/// Default catalog URL when no mirror is configured.
pub const DEFAULT_CATALOG_URL: &str = "https://models.dev/api.json";

/// Providers whose APIs accept an explicit off level with a reasoning toggle.
const REASONING_OFF_PROVIDERS: &[&str] =
    &["deepseek", "kimi-code-plan-cn", "kimi-code-plan-global"];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevCatalog {
    #[serde(flatten)]
    pub providers: BTreeMap<String, DevProvider>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevProvider {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub models: BTreeMap<String, DevModel>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevModel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "release_date")]
    pub release_date: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default, rename = "reasoning_options")]
    pub reasoning_options: Vec<DevReasoningOption>,
    #[serde(default)]
    pub modalities: DevModalities,
    #[serde(default)]
    pub cost: Option<DevPrice>,
    #[serde(default, rename = "canonical_model_id")]
    pub canonical_model_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub temperature: Option<bool>,
    #[serde(default)]
    pub limit: DevLimit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevReasoningOption {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub values: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DevModalities {
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub output: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevPrice {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default, rename = "cache_read")]
    pub cache_read: f64,
    #[serde(default, rename = "cache_write")]
    pub cache_write: f64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DevLimit {
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
}

/// Parses the top-level provider map.
///
/// # Errors
///
/// Returns an error when the JSON is not an object with at least one provider.
pub fn parse_catalog(bytes: &[u8]) -> Result<DevCatalog, serde_json::Error> {
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_slice(bytes)?;
    let mut providers = BTreeMap::new();
    for (key, value) in raw {
        let provider: DevProvider = serde_json::from_value(value)?;
        providers.insert(key, provider);
    }
    if providers.is_empty() {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "catalog is empty",
        )));
    }
    Ok(DevCatalog { providers })
}

/// Builds lookup indexes from a parsed catalog and config overrides.
pub fn build_index(
    catalog: &DevCatalog,
    overrides: &hofund_config::CatalogOverrides,
) -> CatalogIndex {
    let mut by_bare_id: BTreeMap<String, ResolvedEntry> = BTreeMap::new();
    let mut by_provider_model: BTreeMap<String, ResolvedEntry> = BTreeMap::new();

    for (provider_id, provider) in &catalog.providers {
        for (model_key, model) in &provider.models {
            let id = if model.id.is_empty() {
                model_key.clone()
            } else {
                model.id.clone()
            };
            let mut entry = ResolvedEntry {
                provider_id: provider_id.clone(),
                model_id: id.clone(),
                display_name: display_name(model, &id),
                spec: model_spec(provider_id, model, &model.limit, &catalog.providers),
                price: model.cost.as_ref().map(dev_price),
                release_date: non_empty(&model.release_date),
                deprecated: is_deprecated(&model.status),
                retired: model.status.eq_ignore_ascii_case("retired"),
            };
            merge_override(&mut entry, overrides.lookup(&id));
            merge_override(&mut entry, overrides.lookup(&format!("{provider_id}/{id}")));
            by_provider_model.insert(format!("{provider_id}/{id}"), entry.clone());
            by_bare_id
                .entry(bare_id(&id))
                .or_insert_with(|| entry.clone());
            if !by_bare_id.contains_key(&id) {
                by_bare_id.insert(id.clone(), entry);
            }
        }
    }

    for key in overrides.keys() {
        if let Some(patch) = overrides.lookup(key)
            && !by_bare_id.contains_key(key)
            && !by_provider_model.contains_key(key)
        {
            let mut entry = ResolvedEntry {
                provider_id: String::new(),
                model_id: key.clone(),
                display_name: key.clone(),
                spec: ModelSpec::default(),
                price: None,
                release_date: None,
                deprecated: false,
                retired: false,
            };
            merge_override(&mut entry, Some(patch));
            if key.contains('/') {
                by_provider_model.insert(key.clone(), entry.clone());
            }
            by_bare_id.insert(key.clone(), entry);
        }
    }

    CatalogIndex {
        by_bare_id,
        by_provider_model,
    }
}

#[derive(Clone, Debug)]
pub struct CatalogIndex {
    pub by_bare_id: BTreeMap<String, ResolvedEntry>,
    pub by_provider_model: BTreeMap<String, ResolvedEntry>,
}

#[derive(Clone, Debug)]
pub struct ResolvedEntry {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    pub spec: ModelSpec,
    pub price: Option<Price>,
    pub release_date: Option<String>,
    pub deprecated: bool,
    pub retired: bool,
}

impl CatalogIndex {
    #[must_use]
    pub fn resolve(&self, model: &str) -> Option<&ResolvedEntry> {
        if let Some(entry) = self.by_provider_model.get(model) {
            return Some(entry);
        }
        if let Some(entry) = self.by_bare_id.get(model) {
            return Some(entry);
        }
        self.by_bare_id.get(&bare_id(model))
    }
}

fn display_name(model: &DevModel, id: &str) -> String {
    if model.name.is_empty() || model.name == id {
        id.to_owned()
    } else {
        model.name.clone()
    }
}

fn model_spec(
    provider_id: &str,
    model: &DevModel,
    limit: &DevLimit,
    providers: &BTreeMap<String, DevProvider>,
) -> ModelSpec {
    let window = effective_window(limit);
    let output = positive_limit(limit.output);
    let vision = modalities_include_image(&model.modalities.input);
    let reasoning = model.reasoning || !efforts(provider_id, model).is_empty();
    let levels = family_levels(provider_id, model, providers);
    ModelSpec {
        context_window: window,
        max_output: output,
        vision,
        reasoning,
        reasoning_levels: levels,
        input_modalities: normalize_modalities(&model.modalities.input),
        output_modalities: normalize_modalities(&model.modalities.output),
    }
}

fn merge_override(entry: &mut ResolvedEntry, patch: Option<&hofund_config::CatalogOverride>) {
    let Some(patch) = patch else {
        return;
    };
    if let Some(context) = patch.context {
        entry.spec.context_window = Some(context);
    }
    if let Some(output) = patch.output {
        entry.spec.max_output = Some(output);
    }
    if let Some(vision) = patch.vision {
        entry.spec.vision = vision;
    }
    if let Some(levels) = &patch.reasoning_levels {
        entry.spec.reasoning_levels = levels.clone();
        entry.spec.reasoning = !levels.is_empty();
    }
    if let Some(price) = &patch.price {
        entry.price = Some(crate::spec::Price {
            input: price.input_micros as f64 / 1_000_000.0,
            output: price.output_micros as f64 / 1_000_000.0,
            cache_read: price.cache_read_micros as f64 / 1_000_000.0,
            cache_write: price.cache_write_micros as f64 / 1_000_000.0,
        });
    }
    if let Some(name) = &patch.name {
        entry.display_name.clone_from(name);
    }
}

fn dev_price(price: &DevPrice) -> Price {
    Price {
        input: price.input,
        output: price.output,
        cache_read: price.cache_read,
        cache_write: price.cache_write,
    }
}

fn effective_window(limit: &DevLimit) -> Option<u32> {
    let context = window_limit(limit.context);
    let input = window_limit(limit.input);
    if let Some(input) = input {
        if let Some(context) = context
            && context < input
        {
            return Some(context);
        }
        return Some(input);
    }
    context
}

fn window_limit(value: u64) -> Option<u32> {
    if value == 0 {
        return None;
    }
    if value < LEAST_WINDOW as u64 {
        return None;
    }
    u32::try_from(value).ok()
}

fn positive_limit(value: u64) -> Option<u32> {
    if value == 0 {
        None
    } else {
        u32::try_from(value).ok()
    }
}

fn modalities_include_image(modalities: &[String]) -> bool {
    modalities.iter().any(|kind| {
        kind.eq_ignore_ascii_case("image")
            || kind.eq_ignore_ascii_case("image/jpeg")
            || kind.eq_ignore_ascii_case("image/png")
    })
}

fn normalize_modalities(modalities: &[String]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for kind in modalities {
        let normalized = if kind.starts_with("image/") {
            "image".to_owned()
        } else {
            kind.to_ascii_lowercase()
        };
        out.insert(normalized);
    }
    out.into_iter().collect()
}

fn efforts(provider_id: &str, model: &DevModel) -> Vec<String> {
    let mut toggle = false;
    let mut out = Vec::new();
    for option in &model.reasoning_options {
        match option.kind.as_str() {
            "toggle" => toggle = true,
            "effort" => out.clone_from(&option.values),
            _ => {}
        }
    }
    if REASONING_OFF_PROVIDERS.contains(&provider_id)
        && toggle
        && !out.is_empty()
        && !out.iter().any(|level| level == "none")
    {
        out.insert(0, "none".to_owned());
    }
    out
}

fn family_levels(
    provider_id: &str,
    model: &DevModel,
    providers: &BTreeMap<String, DevProvider>,
) -> Vec<String> {
    let direct = efforts(provider_id, model);
    if !direct.is_empty() || !model.reasoning || model.family.is_empty() {
        return direct;
    }
    let mut best_date = String::new();
    let mut best_id = String::new();
    let mut best_levels = Vec::new();
    let Some(provider) = providers.get(provider_id) else {
        return direct;
    };
    for candidate in provider.models.values() {
        if candidate.family != model.family || !candidate.reasoning || candidate.id == model.id {
            continue;
        }
        if !model.release_date.is_empty() && candidate.release_date > model.release_date {
            continue;
        }
        let levels = efforts(provider_id, candidate);
        if levels.is_empty() {
            continue;
        }
        if !best_levels.is_empty()
            && (candidate.release_date < best_date
                || (candidate.release_date == best_date && candidate.id <= best_id))
        {
            continue;
        }
        best_date.clone_from(&candidate.release_date);
        best_id.clone_from(&candidate.id);
        best_levels = levels;
    }
    best_levels
}

fn bare_id(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_owned()
}

fn non_empty(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn is_deprecated(status: &str) -> bool {
    status.eq_ignore_ascii_case("deprecated")
}
