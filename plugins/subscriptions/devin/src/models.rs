//! Model families, variant selection, and catalog shaping.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use hofund_plugin_api::{ModelList, ModelSpec, PluginError};
use hofund_plugin_sdk::json::Value;

use crate::snapshot_families::{NO_IMAGES, SNAPSHOT_JSON};

pub(crate) const LEVEL_RANK: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const LEVELS: &[(&str, &str)] = &[
    ("none", "none"),
    ("xhigh", "xhigh"),
    ("minimal", "minimal"),
    ("min", "minimal"),
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("max", "max"),
];
const TIERS: &[(&str, &str)] = &[("fast", "Fast"), ("priority", "Priority")];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Variant {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) context: u64,
    pub(crate) output: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Family {
    pub(crate) uid: String,
    pub(crate) label: String,
    pub(crate) aliases: Vec<String>,
    pub(crate) models: Vec<Variant>,
}

pub(crate) fn snapshot_families() -> &'static [Family] {
    static FAMILIES: OnceLock<Vec<Family>> = OnceLock::new();
    FAMILIES.get_or_init(|| families_of(SNAPSHOT_JSON).expect("the built-in family table is valid"))
}

pub(crate) fn snapshot_images() -> &'static HashMap<String, bool> {
    static IMAGES: OnceLock<HashMap<String, bool>> = OnceLock::new();
    IMAGES.get_or_init(|| {
        snapshot_families()
            .iter()
            .flat_map(|family| &family.models)
            .map(|model| (model.id.clone(), !NO_IMAGES.contains(&model.id.as_str())))
            .collect()
    })
}

pub(crate) fn families_of(text: &str) -> Result<Vec<Family>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_str(text)
        .map_err(|_| PluginError::decode("Devin model list is not JSON"))?;
    let rows = root.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for row in rows {
        let Some(row) = row.as_array() else {
            continue;
        };
        let uid = row.first().and_then(Value::as_str).unwrap_or_default();
        if uid.is_empty() || skipped_family(uid) {
            continue;
        }
        let label = row.get(1).and_then(Value::as_str).unwrap_or(uid).to_owned();
        let aliases = row
            .get(2)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let models = row
            .get(3)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|variant| {
                let values = variant.as_array()?;
                let id = values.first()?.as_str()?.to_owned();
                if id.is_empty() {
                    return None;
                }
                Some(Variant {
                    name: values
                        .get(1)
                        .and_then(Value::as_str)
                        .unwrap_or(&id)
                        .to_owned(),
                    context: values.get(2).and_then(Value::as_u64).unwrap_or(0),
                    output: values.get(3).and_then(Value::as_u64).unwrap_or(0),
                    id,
                })
            })
            .collect();
        out.push(Family {
            uid: uid.to_owned(),
            label,
            aliases,
            models,
        });
    }
    Ok(out)
}

pub(crate) fn parse_families(body: &[u8]) -> Result<Vec<Family>, PluginError> {
    let root: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Devin model list is not JSON"))?;
    let families = root
        .get("families")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for family in families {
        let Some(object) = family.as_object() else {
            continue;
        };
        let uid = object
            .get("family_uid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if uid.is_empty() || skipped_family(uid) {
            continue;
        }
        let label = object
            .get("family_label")
            .and_then(Value::as_str)
            .unwrap_or(uid)
            .to_owned();
        let aliases = object
            .get("aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        let models = object
            .get("variants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .filter_map(|variant| {
                let id = variant
                    .get("model_uid")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if id.is_empty() {
                    return None;
                }
                Some(Variant {
                    id: id.to_owned(),
                    name: variant
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    context: variant
                        .get("max_context_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output: variant
                        .get("max_output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                })
            })
            .collect();
        out.push(Family {
            uid: uid.to_owned(),
            label,
            aliases,
            models,
        });
    }
    Ok(out)
}

fn skipped_family(uid: &str) -> bool {
    uid.eq_ignore_ascii_case("adaptive") || uid.eq_ignore_ascii_case("fusion")
}

pub(crate) fn parse_models(body: &[u8]) -> Result<Vec<ModelSpec>, PluginError> {
    listed(&parse_families(body)?, snapshot_images())
}

pub(crate) fn listed(
    families: &[Family],
    images: &HashMap<String, bool>,
) -> Result<Vec<ModelSpec>, PluginError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for family in families {
        let context = family
            .models
            .iter()
            .find_map(|model| (model.context > 0).then_some(model.context))
            .unwrap_or(0);
        let output = family
            .models
            .iter()
            .find_map(|model| (model.output > 0).then_some(model.output))
            .unwrap_or(0);
        let image = family_images(family, images);
        push_listed(
            &mut out,
            &mut seen,
            &family.uid,
            &family.label,
            context,
            output,
            efforts(family, ""),
            image,
        )?;
        for (tier, label) in TIERS {
            if by_level(family, tier).is_empty() {
                continue;
            }
            let id = format!("{}-{tier}", family.uid);
            if families.iter().any(|other| named(other, &id)) {
                continue;
            }
            push_listed(
                &mut out,
                &mut seen,
                &id,
                &format!("{} {label}", family.label),
                context,
                output,
                efforts(family, tier),
                image,
            )?;
        }
        if family.models.len() == 1 {
            continue;
        }
        for model in &family.models {
            if model.id == family.uid || !tier_of(&model.id).1.is_empty() {
                continue;
            }
            push_listed(
                &mut out,
                &mut seen,
                &model.id,
                &model.name,
                if model.context == 0 {
                    context
                } else {
                    model.context
                },
                if model.output == 0 {
                    output
                } else {
                    model.output
                },
                Vec::new(),
                images.get(&model.id).copied(),
            )?;
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn push_listed(
    out: &mut Vec<ModelSpec>,
    seen: &mut HashSet<String>,
    id: &str,
    name: &str,
    context: u64,
    output: u64,
    efforts: Vec<String>,
    image: Option<bool>,
) -> Result<(), PluginError> {
    if !seen.insert(id.to_owned()) {
        return Ok(());
    }
    out.push(
        ModelSpec::new(id, Some(name.to_owned()))?
            .with_endpoints(vec!["/v1/chat/completions".to_owned()])
            .with_context(context)
            .with_max_output(output)
            .with_efforts(efforts)
            .with_image_input(image),
    );
    Ok(())
}

fn family_images(family: &Family, images: &HashMap<String, bool>) -> Option<bool> {
    let mut said = None;
    for model in &family.models {
        let Some(value) = images.get(&model.id).copied() else {
            continue;
        };
        if said.is_some_and(|previous| previous != value) {
            return None;
        }
        said = Some(value);
    }
    said
}

pub(crate) fn effort_of(value: &str) -> String {
    let effort = value.trim().to_ascii_lowercase();
    match effort.as_str() {
        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => effort,
        "ultra" => "max".to_owned(),
        _ => String::new(),
    }
}

pub(crate) fn variant_for(families: &[Family], model: &str, effort: &str) -> String {
    for family in families {
        if !named(family, model) || family.models.is_empty() {
            continue;
        }
        if !effort.is_empty() {
            let by = by_level(family, "");
            let levels = efforts(family, "");
            if let Some(id) = by.get(&nearest(effort, &levels)) {
                return id.clone();
            }
        }
        if let Some(model) = default_model(family) {
            return model.id.clone();
        }
    }
    for family in families {
        for (tier, _) in TIERS {
            let suffix = format!("-{tier}");
            let Some(base) = model.strip_suffix(&suffix) else {
                continue;
            };
            if !named(family, base) {
                continue;
            }
            let by = by_level(family, tier);
            let levels = efforts(family, tier);
            let wanted = if effort.is_empty() {
                default_level(family)
            } else {
                effort.to_owned()
            };
            if let Some(id) = by.get(&nearest(&wanted, &levels)) {
                return id.clone();
            }
            if let Some(id) = levels.first().and_then(|level| by.get(level)) {
                return id.clone();
            }
        }
    }
    if !std::ptr::eq(families, snapshot_families())
        && !families
            .iter()
            .any(|family| family.models.iter().any(|variant| variant.id == model))
    {
        return variant_for(snapshot_families(), model, effort);
    }
    model.to_owned()
}

fn named(family: &Family, id: &str) -> bool {
    family.uid == id || family.aliases.iter().any(|alias| alias == id)
}

fn level_of(id: &str) -> &'static str {
    let lower = id.to_ascii_lowercase();
    for (word, level) in LEVELS {
        if lower.ends_with(&format!("-{word}")) || lower.ends_with(&format!("_{word}")) {
            return level;
        }
    }
    ""
}

fn tier_of(id: &str) -> (&'static str, &'static str) {
    let lower = id.to_ascii_lowercase();
    for (tier, _) in TIERS {
        for separator in ["-", "_"] {
            let suffix = format!("{separator}{tier}");
            if lower.ends_with(&suffix) {
                let level = level_of(&lower[..lower.len() - suffix.len()]);
                if !level.is_empty() {
                    return (tier, level);
                }
            }
        }
    }
    ("", level_of(id))
}

fn by_level(family: &Family, tier: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for model in &family.models {
        let (model_tier, level) = tier_of(&model.id);
        if model_tier == tier
            && !level.is_empty()
            && model.id != family.uid
            && !out.contains_key(level)
        {
            out.insert(level.to_owned(), model.id.clone());
        }
    }
    out
}

fn efforts(family: &Family, tier: &str) -> Vec<String> {
    let by = by_level(family, tier);
    LEVEL_RANK
        .iter()
        .filter(|level| by.contains_key(**level))
        .map(|level| (*level).to_owned())
        .collect()
}

fn suffixed(id: &str) -> bool {
    let Some((prefix, word)) = id.rsplit_once('-') else {
        return false;
    };
    !TIERS
        .iter()
        .any(|(tier, _)| word.eq_ignore_ascii_case(tier))
        && !level_of(prefix).is_empty()
}

fn default_model(family: &Family) -> Option<&Variant> {
    family
        .models
        .iter()
        .find(|model| !suffixed(&model.id))
        .or_else(|| family.models.first())
}

fn default_level(family: &Family) -> String {
    if let Some(model) = default_model(family) {
        let (tier, level) = tier_of(&model.id);
        if tier.is_empty() && !level.is_empty() {
            return level.to_owned();
        }
    }
    "medium".to_owned()
}

fn nearest(want: &str, levels: &[String]) -> String {
    let Some(at) = LEVEL_RANK.iter().position(|level| *level == want) else {
        return want.to_owned();
    };
    if levels.is_empty() || levels.iter().any(|level| level == want) {
        return want.to_owned();
    }
    let mut best = want.to_owned();
    let mut distance = LEVEL_RANK.len();
    for level in levels {
        let Some(index) = LEVEL_RANK.iter().position(|candidate| candidate == level) else {
            continue;
        };
        let next = index.abs_diff(at);
        if next < distance || (next == distance && index > at) {
            best = level.clone();
            distance = next;
        }
    }
    best
}

#[allow(dead_code)]
pub(crate) fn list_from_fixture(body: &[u8]) -> Result<ModelList, PluginError> {
    Ok(ModelList::new(parse_models(body)?, None))
}

#[cfg(test)]
mod tests {
    use super::{effort_of, listed, parse_models, snapshot_families, snapshot_images, variant_for};

    #[test]
    fn family_ids_resolve_to_snapshot_default_variants() {
        assert_eq!(variant_for(snapshot_families(), "swe-2", ""), "swe-2-high");
        assert_eq!(variant_for(snapshot_families(), "swe", "max"), "swe-2-max");
        assert_eq!(
            variant_for(snapshot_families(), "swe-2-medium", ""),
            "swe-2-medium"
        );
        assert_eq!(variant_for(&[], "swe-2", ""), "swe-2-high");
        assert_eq!(variant_for(&[], "swe-2", "none"), "swe-2-medium");
        assert_eq!(
            variant_for(snapshot_families(), "claude-opus-4.8", ""),
            "claude-opus-4-8-medium"
        );
        assert!(
            snapshot_families()
                .iter()
                .any(|family| family.uid == "gemini-3-7-flash")
        );
    }

    #[test]
    fn nearest_effort_and_fast_tiers_follow_family_levels() {
        assert_eq!(
            variant_for(snapshot_families(), "swe-2", "low"),
            "swe-2-medium"
        );
        assert_eq!(
            variant_for(snapshot_families(), "claude-opus-5-fast", "high"),
            "claude-opus-5-high-fast"
        );
        assert_eq!(effort_of(" ultra "), "max");
        assert_eq!(effort_of("adaptive"), "");
    }

    #[test]
    fn listed_catalog_contains_families_and_no_effort_variants() {
        let models = parse_models(include_bytes!("fixtures/models.json")).expect("models");
        assert!(models.iter().any(|model| model.id() == "swe-2"));
        let snapshot = listed(snapshot_families(), snapshot_images()).expect("snapshot");
        assert!(snapshot.iter().any(|model| model.id() == "glm-5-2-1m"));
        assert!(!snapshot.iter().any(|model| model.id() == "adaptive"));
    }
}
