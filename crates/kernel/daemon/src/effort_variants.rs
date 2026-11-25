//! Folds relay ids that repeat one model at several reasoning efforts.

use std::collections::{BTreeMap, BTreeSet};

use hofund_plugin_api::Effort;
use hofund_plugin_api::ModelSpec;
use hofund_router::EffortModels;

const WORDS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];

#[derive(Debug)]
struct Family {
    id: String,
    bare: bool,
    by_level: BTreeMap<String, String>,
    conflict: bool,
}

impl Family {
    fn new(id: String) -> Self {
        Self {
            id,
            bare: false,
            by_level: BTreeMap::new(),
            conflict: false,
        }
    }

    fn members(&self) -> Vec<String> {
        self.by_level
            .values()
            .cloned()
            .chain(self.bare.then(|| self.id.clone()))
            .collect()
    }

    fn resolver(&self) -> EffortModels {
        let by_level = WORDS
            .iter()
            .filter_map(|level| {
                self.by_level
                    .get(*level)
                    .map(|id| ((*level).to_owned(), id.clone()))
            })
            .collect::<Vec<_>>();
        let resolver = EffortModels::new(self.id.clone(), self.id.clone(), by_level);
        let default = if self.bare {
            self.id.clone()
        } else {
            resolver.at(&self.id, Some(Effort::Medium))
        };
        EffortModels::new(
            self.id.clone(),
            default,
            WORDS
                .iter()
                .filter_map(|level| {
                    self.by_level
                        .get(*level)
                        .map(|id| ((*level).to_owned(), id.clone()))
                })
                .collect(),
        )
    }
}

fn split(id: &str) -> Option<(String, String)> {
    let (base, suffix) = id.rsplit_once('-')?;
    if base.is_empty() || !WORDS.contains(&suffix.to_ascii_lowercase().as_str()) {
        return None;
    }
    let last = base.rsplit('/').next().unwrap_or(base);
    if !last.bytes().any(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((base.to_owned(), suffix.to_ascii_lowercase()))
}

fn families(models: &[ModelSpec]) -> Vec<Family> {
    let listed = models
        .iter()
        .map(|model| model.id())
        .collect::<BTreeSet<_>>();
    let mut order = Vec::new();
    let mut found = BTreeMap::<String, Family>::new();
    for model in models {
        let Some((base, level)) = split(model.id()) else {
            continue;
        };
        let family = found.entry(base.clone()).or_insert_with(|| {
            order.push(base.clone());
            Family::new(base)
        });
        if family
            .by_level
            .get(&level)
            .is_some_and(|present| present != model.id())
        {
            family.conflict = true;
            continue;
        }
        family.by_level.insert(level, model.id().to_owned());
    }
    order
        .into_iter()
        .filter_map(|base| found.remove(&base))
        .filter(|family| family.by_level.len() >= 2 && !family.conflict)
        .map(|mut family| {
            family.bare = listed.contains(family.id.as_str());
            family
        })
        .collect()
}

pub(crate) fn resolver(models: &[ModelSpec], model: &str) -> Option<EffortModels> {
    families(models)
        .into_iter()
        .find(|family| family.id == model)
        .map(|family| family.resolver())
}

#[cfg(test)]
pub(crate) fn sent_id(models: &[ModelSpec], model: &str, effort: Option<Effort>) -> String {
    resolver(models, model).map_or_else(|| model.to_owned(), |family| family.at(model, effort))
}

pub(crate) fn fold(models: &[ModelSpec]) -> (Vec<ModelSpec>, Vec<EffortModels>) {
    let families = families(models);
    if families.is_empty() {
        return (models.to_vec(), Vec::new());
    }
    let by_id = models
        .iter()
        .map(|model| (model.id(), model))
        .collect::<BTreeMap<_, _>>();
    let member_family = families
        .iter()
        .flat_map(|family| {
            family
                .members()
                .into_iter()
                .map(|member| (member, family.id.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let resolvers = families
        .iter()
        .map(|family| (family.id.clone(), family.resolver()))
        .collect::<BTreeMap<_, _>>();
    let mut folded = Vec::with_capacity(models.len());
    let mut done = BTreeSet::new();
    for model in models {
        let Some(base) = member_family.get(model.id()) else {
            folded.push(model.clone());
            continue;
        };
        if !done.insert(base.clone()) {
            continue;
        }
        let family = &resolvers[base];
        let Some(default) = by_id.get(family.default_id()) else {
            folded.extend(
                family
                    .member_ids()
                    .filter_map(|id| by_id.get(id).map(|model| (*model).clone())),
            );
            continue;
        };
        folded.push(family_model(default, family, &by_id));
    }
    (folded, resolvers.into_values().collect())
}

fn family_display_name(default: &ModelSpec, family: &EffortModels, bare: bool) -> Option<String> {
    if bare {
        return default.display_name().map(str::to_owned);
    }
    let Some(name) = default.display_name().filter(|name| !name.is_empty()) else {
        return Some(family.family().to_owned());
    };
    if name == default.id() {
        return Some(family.family().to_owned());
    }
    if let Some(stripped) = bracket_name(name, family) {
        return Some(stripped);
    }
    if let Some((head, tail)) = name.rsplit_once(' ')
        && WORDS.contains(&tail.to_ascii_lowercase().as_str())
    {
        let stripped = head.trim();
        if !stripped.is_empty() {
            return Some(stripped.to_owned());
        }
    }
    Some(family.family().to_owned())
}

fn bracket_name(name: &str, family: &EffortModels) -> Option<String> {
    let open = name.rfind(" (")?;
    if !name.ends_with(')') {
        return None;
    }
    let level = match name[open + 2..name.len() - 1].to_ascii_lowercase().as_str() {
        "minimal" | "extra low" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "extra high" | "xhigh" => "xhigh",
        _ => return None,
    };
    if !family.levels().contains(&level) {
        return None;
    }
    let stripped = name[..open].trim();
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

fn family_model(
    default: &ModelSpec,
    family: &EffortModels,
    by_id: &BTreeMap<&str, &ModelSpec>,
) -> ModelSpec {
    let mut context = default.context();
    let mut max_output = default.max_output();
    let mut image_input = default.image_input();
    for member in family.member_ids().filter_map(|id| by_id.get(id)) {
        if member.context() > 0 && (context == 0 || member.context() < context) {
            context = member.context();
        }
        if member.max_output() > 0 && (max_output == 0 || member.max_output() < max_output) {
            max_output = member.max_output();
        }
        image_input = match (image_input, member.image_input()) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        };
    }
    let display_name = family_display_name(default, family, by_id.contains_key(family.family()));
    let (input_price, output_price, cache_read_price) = default.prices();
    let (rate, rate_was) = default.rate_parts();
    let mut model = ModelSpec::new(family.family(), display_name)
        .unwrap_or_else(|_| unreachable!("a family id is non-empty"))
        .with_context(context)
        .with_max_output(max_output)
        .with_input_modalities(default.input_modalities().to_vec())
        .with_output_modalities(default.output_modalities().to_vec())
        .with_image_input(image_input)
        .with_endpoints(default.endpoints().to_vec())
        .with_draws(default.draws())
        .with_films(default.films())
        .with_decides(default.decides())
        .with_efforts(family.levels().into_iter().map(str::to_owned).collect())
        .with_service_tiers(default.service_tiers().to_vec());
    if let Some(free) = default.free() {
        model = model.with_free(free);
    }
    if let (Some(input), Some(output), Some(cache_read)) =
        (input_price, output_price, cache_read_price)
    {
        model = model.with_prices(input, output, cache_read);
    }
    if let Some(rate) = rate {
        model = model.with_rate(rate, rate_was);
    }
    if let Some(api) = default.preferred_api() {
        model = model.with_preferred_api(api);
    }
    if let Some(entry) = default.catalog_entry() {
        model = model.with_catalog_entry(entry);
    }
    if let Some(search) = default.web_search() {
        model = model.with_web_search(search);
    }
    model
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::Effort;
    use hofund_plugin_api::ModelSpec;

    use super::{fold, resolver, sent_id, split};

    fn models(ids: &[&str]) -> Vec<ModelSpec> {
        ids.iter()
            .map(|id| ModelSpec::new(*id, None).expect("model"))
            .collect()
    }

    #[test]
    fn d246_split_requires_an_effort_word_and_a_versioned_base() {
        for (id, expected) in [
            ("model-4-alpha-High", Some("model-4-alpha high")),
            ("vendor/model-5-xhigh", Some("vendor/model-5 xhigh")),
            ("model-4-highspeed", None),
            ("model-4-lite", None),
            ("model-4-max", None),
            ("medium", None),
            ("size-medium", None),
            ("vendor9/size-high", None),
            ("-high", None),
        ] {
            let actual = split(id).map(|(base, level)| format!("{base} {level}"));
            assert_eq!(actual.as_deref(), expected, "{id}");
        }
    }

    #[test]
    fn d246_fold_keeps_order_efforts_limits_and_nearest_selection() {
        let source = vec![
            ModelSpec::new("model-4-alpha-high", None)
                .expect("model")
                .with_context(1000)
                .with_max_output(200)
                .with_image_input(Some(true)),
            ModelSpec::new("plain", None).expect("model"),
            ModelSpec::new("model-4-alpha-low", None)
                .expect("model")
                .with_context(800)
                .with_max_output(300)
                .with_image_input(Some(false)),
            ModelSpec::new("model-4-alpha-medium", None).expect("model"),
            ModelSpec::new("model-4-beta-low", None).expect("model"),
        ];
        let (folded, families) = fold(&source);
        assert_eq!(
            folded.iter().map(|model| model.id()).collect::<Vec<_>>(),
            ["model-4-alpha", "plain", "model-4-beta-low"]
        );
        assert_eq!(folded[0].efforts(), ["low", "medium", "high"]);
        assert_eq!(folded[0].context(), 800);
        assert_eq!(folded[0].max_output(), 200);
        assert_eq!(folded[0].image_input(), Some(false));
        assert_eq!(families.len(), 1);
        for (effort, expected) in [
            (None, "model-4-alpha-medium"),
            (Some(Effort::None), "model-4-alpha-low"),
            (Some(Effort::Minimal), "model-4-alpha-low"),
            (Some(Effort::Low), "model-4-alpha-low"),
            (Some(Effort::High), "model-4-alpha-high"),
            (Some(Effort::Xhigh), "model-4-alpha-high"),
            (Some(Effort::Max), "model-4-alpha-high"),
        ] {
            assert_eq!(sent_id(&source, "model-4-alpha", effort), expected);
        }
        assert_eq!(
            sent_id(&source, "model-4-alpha-high", Some(Effort::Low)),
            "model-4-alpha-high"
        );
        assert!(resolver(&source, "model-4-beta-low").is_none());
    }

    #[test]
    fn d246_family_display_name_drops_a_trailing_effort_word() {
        let source = vec![
            ModelSpec::new("model-4-alpha-low", Some("Alpha Four (Low)".to_owned()))
                .expect("model"),
            ModelSpec::new("model-4-alpha-high", Some("Alpha Four High".to_owned()))
                .expect("model"),
            ModelSpec::new("model-4-beta", Some("Beta Four".to_owned())).expect("model"),
            ModelSpec::new("model-4-beta-low", Some("Beta Four Low".to_owned())).expect("model"),
            ModelSpec::new("model-4-beta-high", Some("Beta Four (High)".to_owned()))
                .expect("model"),
            ModelSpec::new("model-9-gamma-low", Some("model-9-gamma-low".to_owned()))
                .expect("model"),
            ModelSpec::new("model-9-gamma-high", Some("Gamma Nine Special".to_owned()))
                .expect("model"),
        ];
        let (folded, _) = fold(&source);
        let names = folded
            .iter()
            .map(|model| (model.id(), model.display_name().unwrap_or("").to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("model-4-alpha", "Alpha Four".to_owned()),
                ("model-4-beta", "Beta Four".to_owned()),
                ("model-9-gamma", "model-9-gamma".to_owned()),
            ]
        );
    }

    #[test]
    fn d246_bare_id_is_the_default_and_ties_go_up() {
        let source = models(&["model-4-alpha-low", "model-4-alpha", "model-4-alpha-High"]);
        let (folded, _) = fold(&source);
        assert_eq!(folded.len(), 1);
        assert_eq!(folded[0].efforts(), ["low", "high"]);
        assert_eq!(sent_id(&source, "model-4-alpha", None), "model-4-alpha");
        assert_eq!(
            sent_id(&source, "model-4-alpha", Some(Effort::Medium)),
            "model-4-alpha-High"
        );
    }

    #[test]
    fn d246_conflicting_case_variants_do_not_fold() {
        let source = models(&[
            "Model-4-alpha-low",
            "Model-4-alpha-HIGH",
            "Model-4-alpha-high",
        ]);
        assert_eq!(fold(&source).0, source);
    }
}
