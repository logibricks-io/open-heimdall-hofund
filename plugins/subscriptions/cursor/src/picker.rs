//! Request-time model parameters from Cursor's model picker.

use hofund_plugin_api::{HostContext, Request, SecretString};
use hofund_plugin_sdk::json::Value;

use crate::endpoints::Endpoints;
use crate::proto::ModelParameter;
use crate::{connect, models};

const LEVEL_RANK: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const EFFORT_PARAMS: &[&str] = &["effort", "reasoning_effort", "reasoning"];
const PICKER_SKIP: &[&str] = &[
    "claude-4.5-haiku",
    "claude-4.5-haiku-thinking",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
    "claude-haiku-4-5",
];

/// The requested-model id, its native parameters, and the variant's mode.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub(crate) id: String,
    pub(crate) parameters: Vec<ModelParameter>,
    pub(crate) max_mode: bool,
}

impl ModelChoice {
    pub(crate) fn passthrough(model: &str) -> Self {
        Self {
            id: model.to_owned(),
            parameters: Vec::new(),
            max_mode: false,
        }
    }
}

/// Resolves picker inputs. Calls without one keep the pre-picker request path.
pub(crate) async fn choose(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
    request: &Request,
) -> ModelChoice {
    if !needs_picker(request) {
        return ModelChoice::passthrough(&request.model);
    }
    let effort = request
        .params
        .effort
        .map(|effort| effort.openai_label())
        .unwrap_or_default();
    let fast = fast_of(request);
    let Ok(body) = connect::available_models(host, endpoints, token).await else {
        return ModelChoice::passthrough(&request.model);
    };
    let models = parse_models(&body);
    if models.is_empty() {
        return ModelChoice::passthrough(&request.model);
    }
    request_model(&models, &request.model, effort, fast)
}

fn needs_picker(request: &Request) -> bool {
    request.params.effort.is_some()
        || request.extensions.get("service_tier").is_some()
        || split_context(&request.model).1.is_some()
}

fn fast_of(request: &Request) -> Option<bool> {
    request
        .extensions
        .get("service_tier")
        .and_then(Value::as_str)
        .map(|tier| matches!(tier, "priority" | "fast"))
}

#[derive(Clone, Debug)]
struct PickerModel {
    name: String,
    legacy_slugs: Vec<String>,
    id_aliases: Vec<String>,
    context_token_limit: u64,
    supports_non_max_mode: Option<bool>,
    definitions: Vec<ParameterDefinition>,
    variants: Vec<Variant>,
}

#[derive(Clone, Debug)]
struct ParameterDefinition {
    id: String,
    values: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct Variant {
    legacy_slug: Option<String>,
    values: Vec<ModelParameter>,
    is_default_non_max: bool,
    is_default_max: bool,
    is_max: bool,
}

fn parse_models(body: &Value) -> Vec<PickerModel> {
    body.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_model)
        .collect()
}

fn parse_model(value: &Value) -> Option<PickerModel> {
    let name = value.get("name")?.as_str()?.to_owned();
    if name.is_empty()
        || PICKER_SKIP.contains(&name.as_str())
        || value.get("isHidden").and_then(Value::as_bool) == Some(true)
        || value.get("isChatOnly").and_then(Value::as_bool) == Some(true)
        || value.get("onlySupportsCmdK").and_then(Value::as_bool) == Some(true)
        || value.get("supportsAgent").and_then(Value::as_bool) == Some(false)
    {
        return None;
    }
    Some(PickerModel {
        name,
        legacy_slugs: strings(value.get("legacySlugs")),
        id_aliases: strings(value.get("idAliases")),
        context_token_limit: value
            .get("contextTokenLimit")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        supports_non_max_mode: value.get("supportsNonMaxMode").and_then(Value::as_bool),
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

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn scalar_string(value: &Value) -> Option<String> {
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

fn parse_variant(value: &Value) -> Option<Variant> {
    if !value.is_object() {
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

fn request_model(
    models: &[PickerModel],
    requested: &str,
    effort: &str,
    fast: Option<bool>,
) -> ModelChoice {
    let (bare, explicit_size) = split_context(requested);
    let bare = if bare == "auto" { "default" } else { bare };

    if let Some(model) = models.iter().find(|model| model.name == bare) {
        let size = entry_size(model, explicit_size, None);
        return pick(model, &size, effort, fast, None);
    }

    for model in models {
        if let Some(seed) = model
            .variants
            .iter()
            .find(|variant| variant.legacy_slug.as_deref() == Some(bare))
        {
            let seed_fast = parameter(&seed.values, "fast") == Some("true");
            let size = entry_size(model, explicit_size, Some(seed));
            return pick(
                model,
                &size,
                effort,
                Some(fast.unwrap_or(false) || seed_fast),
                Some(seed),
            );
        }
    }

    let (family, id_effort, id_fast) = family_of(bare);
    for model in models {
        let candidates = model
            .variants
            .iter()
            .filter(|variant| {
                variant
                    .legacy_slug
                    .as_deref()
                    .is_some_and(|slug| models::split_id(slug).0 == family)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            continue;
        }
        let default = default_variant(model);
        let seed = nearest_variant(&candidates, default.map_or(&[][..], |v| &v.values));
        let size = entry_size(model, explicit_size, seed);
        let effort = if effort.is_empty() {
            &id_effort
        } else {
            effort
        };
        let seed_fast = seed.is_some_and(|seed| parameter(&seed.values, "fast") == Some("true"));
        return pick(
            model,
            &size,
            effort,
            Some(fast.unwrap_or(false) || id_fast || seed_fast),
            seed,
        );
    }

    for model in models {
        if model.legacy_slugs.iter().any(|slug| slug == bare)
            || model.id_aliases.iter().any(|alias| alias == bare)
        {
            let size = entry_size(model, explicit_size, None);
            return pick(model, &size, effort, fast, None);
        }
    }

    ModelChoice::passthrough(requested)
}

fn split_context(model: &str) -> (&str, Option<&str>) {
    let Some((base, size)) = model.rsplit_once('@') else {
        return (model, None);
    };
    if base.is_empty() || size.is_empty() || base.contains('@') {
        return (model, None);
    }
    (base, Some(size))
}

fn family_of(id: &str) -> (String, String, bool) {
    let (mut family, effort) = models::split_id(id);
    let fast = family.len() > "-fast".len() && family.ends_with("-fast");
    if fast {
        family.truncate(family.len() - "-fast".len());
    }
    (family, effort, fast)
}

fn default_variant(model: &PickerModel) -> Option<&Variant> {
    model
        .variants
        .iter()
        .find(|variant| variant.is_default_non_max)
        .or_else(|| model.variants.iter().find(|variant| variant.is_default_max))
        .or_else(|| model.variants.first())
}

fn entry_size(model: &PickerModel, explicit: Option<&str>, seed: Option<&Variant>) -> String {
    let contexts = definition(model, "context")
        .map(|definition| definition.values.as_slice())
        .unwrap_or_default();
    if contexts.is_empty() {
        return String::new();
    }
    if let Some(size) = explicit {
        if contexts.iter().any(|candidate| candidate == size) {
            return size.to_owned();
        }
        let wanted = tokens_of(size);
        return contexts
            .iter()
            .min_by_key(|candidate| tokens_of(candidate).abs_diff(wanted))
            .cloned()
            .unwrap_or_default();
    }
    let seeded = seed
        .and_then(|seed| parameter(&seed.values, "context"))
        .or_else(|| {
            default_variant(model).and_then(|variant| parameter(&variant.values, "context"))
        });
    if let Some(seeded) = seeded.filter(|seeded| contexts.iter().any(|value| value == *seeded)) {
        return seeded.to_owned();
    }
    contexts.first().cloned().unwrap_or_default()
}

fn tokens_of(value: &str) -> u64 {
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "k" => 1_000.0,
        "m" => 1_000_000.0,
        _ => return 0,
    };
    number
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| (number * multiplier).round() as u64)
        .unwrap_or(0)
}

fn definition<'a>(model: &'a PickerModel, id: &str) -> Option<&'a ParameterDefinition> {
    model
        .definitions
        .iter()
        .find(|definition| definition.id == id && !definition.values.is_empty())
}

fn parameter<'a>(values: &'a [ModelParameter], id: &str) -> Option<&'a str> {
    values
        .iter()
        .find(|parameter| parameter.id == id)
        .map(|parameter| parameter.value.as_str())
}

fn set_parameter(values: &mut Vec<ModelParameter>, id: &str, value: &str) {
    if let Some(parameter) = values.iter_mut().find(|parameter| parameter.id == id) {
        parameter.value = value.to_owned();
    } else {
        values.push(ModelParameter {
            id: id.to_owned(),
            value: value.to_owned(),
        });
    }
}

fn efforts_of(model: &PickerModel) -> (Option<&str>, Vec<(String, String)>) {
    let effort = EFFORT_PARAMS
        .iter()
        .find_map(|id| definition(model, id).map(|definition| (id, definition)));
    let Some((id, definition)) = effort else {
        if definition(model, "thinking").is_some_and(|definition| {
            definition.values.iter().any(|value| value == "true")
                && definition.values.iter().any(|value| value == "false")
        }) {
            return (
                Some("thinking"),
                vec![
                    ("none".to_owned(), "false".to_owned()),
                    ("high".to_owned(), "true".to_owned()),
                ],
            );
        }
        return (None, Vec::new());
    };
    let levels = definition
        .values
        .iter()
        .filter_map(|value| {
            let level = if value == "extra-high" {
                "xhigh"
            } else {
                value.as_str()
            };
            LEVEL_RANK
                .contains(&level)
                .then(|| (level.to_owned(), value.clone()))
        })
        .collect();
    (Some(id), levels)
}

fn fit_effort(want: &str, levels: &[(String, String)]) -> String {
    let want = if want == "off" {
        "none"
    } else if want == "ultra" {
        "max"
    } else {
        want
    };
    if levels.is_empty() || levels.iter().any(|(level, _)| level == want) {
        return want.to_owned();
    }
    let Some(at) = LEVEL_RANK.iter().position(|level| *level == want) else {
        return String::new();
    };
    let mut best = "";
    let mut distance = LEVEL_RANK.len();
    for (level, _) in levels {
        let Some(index) = LEVEL_RANK.iter().position(|candidate| candidate == level) else {
            continue;
        };
        if *level == "none" {
            continue;
        }
        let candidate_distance = index.abs_diff(at);
        if candidate_distance < distance || (candidate_distance == distance && index > at) {
            best = level;
            distance = candidate_distance;
        }
    }
    best.to_owned()
}

fn same_variant(variant: &Variant, want: &[ModelParameter]) -> bool {
    want.iter().all(|wanted| {
        parameter(&variant.values, &wanted.id).is_none_or(|value| value == wanted.value)
    }) && variant
        .values
        .iter()
        .all(|held| parameter(want, &held.id).is_none_or(|value| value == held.value))
}

fn nearest_variant<'a>(variants: &[&'a Variant], want: &[ModelParameter]) -> Option<&'a Variant> {
    let mut best = None;
    let mut most = -1_i32;
    for variant in variants {
        let mut score = want
            .iter()
            .filter(|wanted| parameter(&variant.values, &wanted.id) == Some(wanted.value.as_str()))
            .count() as i32;
        if parameter(&variant.values, "context") == parameter(want, "context") {
            score += 100;
        }
        if score > most {
            best = Some(*variant);
            most = score;
        }
    }
    best
}

fn normalized_level(value: &str) -> &str {
    if value == "extra-high" {
        "xhigh"
    } else {
        value
    }
}

fn branch<'a>(model: &'a PickerModel, want: &[ModelParameter], pid: &str) -> Option<&'a Variant> {
    if pid == "thinking" {
        return None;
    }
    let default = default_variant(model).map_or(&[][..], |variant| &variant.values);
    let wanted_level = parameter(want, pid).map(normalized_level);
    let named = model
        .variants
        .iter()
        .any(|variant| parameter(&variant.values, pid).map(normalized_level) == wanted_level);
    if !named
        && want.iter().all(|wanted| {
            wanted.id == pid
                || wanted.id == "context"
                || parameter(default, &wanted.id).is_none_or(|value| value == wanted.value)
        })
    {
        return None;
    }
    let candidates = model
        .variants
        .iter()
        .filter(|variant| {
            parameter(&variant.values, pid).is_some()
                && want.iter().all(|wanted| {
                    wanted.id == pid
                        || parameter(&variant.values, &wanted.id)
                            .is_none_or(|value| value == wanted.value)
                })
        })
        .collect::<Vec<_>>();
    let levels = candidates
        .iter()
        .filter_map(|variant| {
            parameter(&variant.values, pid).map(|value| {
                let level = normalized_level(value);
                (level.to_owned(), value.to_owned())
            })
        })
        .collect::<Vec<_>>();
    let wanted = parameter(want, pid)
        .map(normalized_level)
        .unwrap_or_default();
    let at = fit_effort(wanted, &levels);
    candidates
        .into_iter()
        .find(|variant| parameter(&variant.values, pid).map(normalized_level) == Some(at.as_str()))
}

fn pick(
    model: &PickerModel,
    size: &str,
    effort: &str,
    fast: Option<bool>,
    seed: Option<&Variant>,
) -> ModelChoice {
    let default = default_variant(model);
    let mut want = seed
        .or(default)
        .map(|variant| variant.values.clone())
        .unwrap_or_default();
    if !size.is_empty() {
        set_parameter(&mut want, "context", size);
    }
    let (pid, levels) = efforts_of(model);
    if let Some(pid) = pid {
        if pid != "thinking"
            && effort == "none"
            && !levels.iter().any(|(level, _)| level == "none")
            && parameter(&want, "thinking").is_some()
        {
            set_parameter(&mut want, "thinking", "false");
        } else {
            let at = fit_effort(effort, &levels);
            if let Some(value) = levels
                .iter()
                .find(|(level, _)| level == &at)
                .map(|(_, value)| value.clone())
            {
                set_parameter(&mut want, pid, &value);
                if seed.is_none() && pid != "thinking" && parameter(&want, "thinking").is_some() {
                    set_parameter(&mut want, "thinking", "true");
                }
            }
        }
    }
    if definition(model, "fast").is_some() {
        set_parameter(
            &mut want,
            "fast",
            if fast == Some(true) { "true" } else { "false" },
        );
    }
    let selected = model
        .variants
        .iter()
        .find(|variant| same_variant(variant, &want))
        .or_else(|| seed.and_then(|_| pid.and_then(|pid| branch(model, &want, pid))));
    if let Some(selected) = selected {
        for value in &selected.values {
            set_parameter(&mut want, &value.id, &value.value);
        }
    }
    let max_mode = selected.map_or_else(
        || {
            model.supports_non_max_mode == Some(false)
                || default.is_some_and(|default| needs_max(model, default))
                || tokens_of(size) > model.context_token_limit && model.context_token_limit > 0
        },
        |selected| needs_max(model, selected),
    );
    let id = selected
        .and_then(|selected| selected.legacy_slug.clone())
        .or_else(|| default.and_then(|default| default.legacy_slug.clone()))
        .unwrap_or_else(|| model.name.clone());
    ModelChoice {
        id,
        parameters: want,
        max_mode,
    }
}

fn needs_max(model: &PickerModel, variant: &Variant) -> bool {
    model.supports_non_max_mode == Some(false) || variant.is_max
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::Request;
    use hofund_plugin_sdk::json::{Value, json};

    use super::{ModelChoice, needs_picker, parse_models, request_model};
    use crate::proto::ModelParameter;

    fn models() -> Vec<super::PickerModel> {
        let enum_values = |values: &[&str]| json!({"enumParameter": {"values": values.iter().map(|value| json!({"value": value})).collect::<Vec<_>>()}});
        let boolean =
            json!({"booleanParameter": {"values": [{"value": "false"}, {"value": "true"}]}});
        let definition = |id: &str, kind: Value| json!({"id": id, "parameterType": kind});
        let variant = |slug: &str, values: &[(&str, &str)], default: bool, max: bool| {
            json!({
                "legacySlug": slug,
                "parameterValues": values.iter().map(|(id, value)| json!({"id": id, "value": value})).collect::<Vec<_>>(),
                "isDefaultNonMaxConfig": default,
                "isMaxMode": max,
            })
        };
        parse_models(&json!({"models": [
            {
                "name": "claude-opus-5-5",
                "contextTokenLimit": 300000,
                "supportsNonMaxMode": true,
                "parameterDefinitions": [
                    definition("context", enum_values(&["300k", "1m"])),
                    definition("effort", enum_values(&["low", "medium", "high", "extra-high"])),
                    definition("fast", boolean.clone()),
                ],
                "variants": [
                    variant("claude-opus-5-5-medium", &[("context", "300k"), ("effort", "medium"), ("fast", "false")], true, false),
                    variant("claude-opus-5-5-extra-high-fast", &[("context", "300k"), ("effort", "extra-high"), ("fast", "true")], false, true),
                ],
            },
            {
                "name": "claude-haiku-5-5",
                "supportsNonMaxMode": true,
                "parameterDefinitions": [
                    definition("thinking", boolean.clone()),
                    definition("reasoning_effort", enum_values(&["low", "high"])),
                ],
                "variants": [
                    variant("claude-haiku-5-5-thinking-high", &[("thinking", "true"), ("reasoning_effort", "high")], true, false),
                    variant("claude-haiku-5-5-high", &[("thinking", "false"), ("reasoning_effort", "high")], false, false),
                    variant("claude-haiku-5-5-low", &[("thinking", "false"), ("reasoning_effort", "low")], false, false),
                ],
            },
            {
                "name": "gpt-5.3-codex",
                "supportsNonMaxMode": true,
                "parameterDefinitions": [definition("reasoning", enum_values(&["low", "high"]))],
                "variants": [
                    variant("gpt-5.3-codex-high", &[("reasoning", "high")], true, false),
                    variant("gpt-5.3-codex-low", &[("reasoning", "low")], false, false),
                ],
            },
            {
                "name": "thinking-only",
                "supportsNonMaxMode": true,
                "parameterDefinitions": [definition("thinking", boolean)],
                "variants": [
                    variant("thinking-only", &[("thinking", "false")], true, false),
                    variant("thinking-only-thinking", &[("thinking", "true")], false, false),
                ],
            },
            {
                "name": "composer-2.5",
                "supportsNonMaxMode": true,
                "variants": [variant("composer-2.5", &[], true, false)],
            },
            {
                "name": "max-only",
                "contextTokenLimit": 200000,
                "supportsNonMaxMode": false,
            },
        ]}))
    }

    fn parameters(choice: &ModelChoice) -> Vec<(&str, &str)> {
        choice
            .parameters
            .iter()
            .map(|parameter| (parameter.id.as_str(), parameter.value.as_str()))
            .collect()
    }

    #[test]
    fn effort_fast_and_context_pick_the_native_variant_and_its_mode() {
        let models = models();
        let choice = request_model(&models, "claude-opus-5-5@300k", "xhigh", Some(true));
        assert_eq!(choice.id, "claude-opus-5-5-extra-high-fast");
        assert!(choice.max_mode);
        assert_eq!(
            choice.parameters,
            [
                ModelParameter {
                    id: "context".to_owned(),
                    value: "300k".to_owned(),
                },
                ModelParameter {
                    id: "effort".to_owned(),
                    value: "extra-high".to_owned(),
                },
                ModelParameter {
                    id: "fast".to_owned(),
                    value: "true".to_owned(),
                },
            ]
        );

        let default = request_model(&models, "claude-opus-5-5@300k", "", None);
        assert_eq!(default.id, "claude-opus-5-5-medium");
        assert_eq!(
            parameters(&default),
            [("context", "300k"), ("effort", "medium"), ("fast", "false")]
        );

        let no_matching_variant = request_model(&models, "claude-opus-5-5@1m", "medium", None);
        assert!(no_matching_variant.max_mode);
        assert_eq!(
            parameters(&no_matching_variant),
            [("context", "1m"), ("effort", "medium"), ("fast", "false")]
        );
        assert!(request_model(&models, "max-only", "", None).max_mode);
    }

    #[test]
    fn all_effort_parameter_ids_and_thinking_only_follow_the_picker() {
        let models = models();
        let off = request_model(&models, "claude-haiku-5-5", "none", None);
        assert_eq!(off.id, "claude-haiku-5-5-high");
        assert_eq!(
            parameters(&off),
            [("thinking", "false"), ("reasoning_effort", "high")]
        );

        let nearest = request_model(&models, "gpt-5.3-codex-low", "xhigh", None);
        assert_eq!(nearest.id, "gpt-5.3-codex-high");
        assert_eq!(parameters(&nearest), [("reasoning", "high")]);
        let minimal = request_model(&models, "gpt-5.3-codex", "minimal", None);
        assert_eq!(parameters(&minimal), [("reasoning", "low")]);

        let thinking = request_model(&models, "thinking-only", "high", None);
        assert_eq!(thinking.id, "thinking-only-thinking");
        assert_eq!(parameters(&thinking), [("thinking", "true")]);
        let no_thinking = request_model(&models, "thinking-only", "none", None);
        assert_eq!(parameters(&no_thinking), [("thinking", "false")]);
    }

    #[test]
    fn a_model_without_effort_parameters_has_no_effort_field() {
        let models = models();
        let choice = request_model(&models, "composer-2.5", "high", None);
        assert_eq!(choice.id, "composer-2.5");
        assert!(choice.parameters.is_empty());
        assert!(choice.parameters.iter().all(|parameter| {
            !["effort", "reasoning_effort", "reasoning"].contains(&parameter.id.as_str())
        }));

        assert!(!needs_picker(&Request::new("composer-2.5")));
        let mut request = Request::new("composer-2.5");
        request.extensions.insert("service_tier", json!("auto"));
        assert!(needs_picker(&request));
        assert!(needs_picker(&Request::new("composer-2.5@200k")));
    }
}
