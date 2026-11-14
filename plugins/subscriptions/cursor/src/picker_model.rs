//! Parsed Cursor picker / catalog model records from AvailableModels JSON.

use hofund_plugin_sdk::json::Value;

use crate::proto::ModelParameter;

pub(crate) const PICKER_SKIP: &[&str] = &[
    "claude-4.5-haiku",
    "claude-4.5-haiku-thinking",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "claude-haiku-4-5",
];

/// One model from AvailableModels, before listing filters.
#[derive(Clone, Debug)]
pub(crate) struct PickerModel {
    pub(crate) name: String,
    pub(crate) client_display_name: Option<String>,
    pub(crate) legacy_slugs: Vec<String>,
    pub(crate) id_aliases: Vec<String>,
    pub(crate) context_token_limit: u64,
    pub(crate) context_token_limit_for_max_mode: u64,
    pub(crate) supports_non_max_mode: Option<bool>,
    pub(crate) is_hidden: bool,
    pub(crate) is_chat_only: bool,
    pub(crate) only_supports_cmd_k: bool,
    pub(crate) supports_agent: bool,
    pub(crate) vendor_id: Option<String>,
    pub(crate) definitions: Vec<ParameterDefinition>,
    pub(crate) variants: Vec<Variant>,
}

#[derive(Clone, Debug)]
pub(crate) struct ParameterDefinition {
    pub(crate) id: String,
    pub(crate) values: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Variant {
    pub(crate) legacy_slug: Option<String>,
    pub(crate) values: Vec<ModelParameter>,
    pub(crate) is_default_non_max: bool,
    pub(crate) is_default_max: bool,
    pub(crate) is_max: bool,
}

pub(crate) fn models_array(body: &Value) -> Vec<&Value> {
    body.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect()
}

/// Parses every model object in a response body.
pub(crate) fn parse_all(body: &Value) -> Vec<PickerModel> {
    models_array(body)
        .into_iter()
        .filter_map(parse_one)
        .collect()
}

impl PickerModel {
    pub(crate) fn visible_in_picker(&self) -> bool {
        !self.name.is_empty()
            && !PICKER_SKIP.contains(&self.name.as_str())
            && !self.is_hidden
            && !self.is_chat_only
            && !self.only_supports_cmd_k
            && self.supports_agent
    }

    pub(crate) fn title(&self) -> String {
        clean(self.client_display_name.as_deref().unwrap_or(&self.name))
    }
}

pub(crate) fn parse_one(value: &Value) -> Option<PickerModel> {
    let name = value.get("name")?.as_str()?.to_owned();
    if name.is_empty() {
        return None;
    }
    Some(PickerModel {
        name,
        client_display_name: value
            .get("clientDisplayName")
            .and_then(Value::as_str)
            .map(str::to_owned),
        legacy_slugs: strings(value.get("legacySlugs")),
        id_aliases: strings(value.get("idAliases")),
        context_token_limit: value
            .get("contextTokenLimit")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        context_token_limit_for_max_mode: value
            .get("contextTokenLimitForMaxMode")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        supports_non_max_mode: value.get("supportsNonMaxMode").and_then(Value::as_bool),
        is_hidden: value
            .get("isHidden")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_chat_only: value
            .get("isChatOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        only_supports_cmd_k: value
            .get("onlySupportsCmdK")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        supports_agent: value
            .get("supportsAgent")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        vendor_id: value
            .get("vendor")
            .and_then(|vendor| vendor.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        definitions: value
            .get("parameterDefinitions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_definition)
            .collect(),
        variants: value
            .get("variants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_variant)
            .collect(),
    })
}

pub(crate) fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_definition(value: &Value) -> Option<ParameterDefinition> {
    let id = value.get("id")?.as_str()?.to_owned();
    if id.is_empty() {
        return None;
    }
    let kind = value.get("parameterType")?;
    let values = kind
        .get("enumParameter")
        .or_else(|| kind.get("booleanParameter"))
        .and_then(|parameter| parameter.get("values"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.get("value"))
        .filter_map(scalar_string)
        .collect();
    Some(ParameterDefinition { id, values })
}

pub(crate) fn parse_variant(value: &Value) -> Option<Variant> {
    if !value.is_object() {
        return None;
    }
    let parameter_values = value.get("parameterValues");
    if !parameter_values.is_some_and(Value::is_array) {
        return None;
    }
    Some(Variant {
        legacy_slug: value
            .get("legacySlug")
            .and_then(Value::as_str)
            .filter(|slug| !slug.is_empty())
            .map(str::to_owned),
        values: value
            .get("parameterValues")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|parameter| {
                Some(ModelParameter {
                    id: parameter.get("id")?.as_str()?.to_owned(),
                    value: scalar_string(parameter.get("value")?)?,
                })
            })
            .collect(),
        is_default_non_max: value
            .get("isDefaultNonMaxConfig")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_default_max: value
            .get("isDefaultMaxConfig")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_max: value
            .get("isMaxMode")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub(crate) fn clean(s: &str) -> String {
    s.replace('\u{200b}', "")
        .split_whitespace()
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn values_of(variant: &Variant) -> std::collections::HashMap<String, String> {
    variant
        .values
        .iter()
        .map(|parameter| (parameter.id.clone(), parameter.value.clone()))
        .collect()
}

pub(crate) fn param_defs(model: &PickerModel) -> std::collections::HashMap<String, Vec<String>> {
    model
        .definitions
        .iter()
        .map(|definition| (definition.id.clone(), definition.values.clone()))
        .collect()
}

pub(crate) fn default_variant(model: &PickerModel) -> Option<&Variant> {
    model
        .variants
        .iter()
        .find(|variant| variant.is_default_non_max)
        .or_else(|| model.variants.iter().find(|variant| variant.is_default_max))
        .or_else(|| model.variants.first())
}
