//! Live model catalog: picker, usable list, and admin scope merge.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hofund_plugin_api::{HostContext, ModelList, ModelSpec, PluginError, SecretString};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;

use crate::connect;
use crate::endpoints::Endpoints;
use crate::models;
use crate::picker::ModelChoice;
use crate::picker_model::{
    PickerModel, Variant, clean, default_variant, param_defs, parse_all, values_of,
};
use crate::proto::ModelParameter;

pub(crate) const DEFAULT_CONTEXT: u64 = 200_000;
const MODELS_KEEP: Duration = Duration::from_secs(10 * 60);
const LIST_RETRY: Duration = Duration::from_secs(5 * 60);
const ANTHROPIC_VENDOR: &str = "MODEL_VENDOR_ID_ANTHROPIC";
const NO_ID: &str = "\0no-id";

const LEVEL_RANK: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const EFFORT_PARAMS: &[&str] = &["effort", "reasoning_effort", "reasoning"];

/// One offered model row after catalog merge.
#[derive(Clone, Debug)]
pub(crate) struct ListEntry {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) run: String,
    pub(crate) context: u64,
    pub(crate) size: String,
    pub(crate) pid: String,
    pub(crate) levels: HashMap<String, String>,
    pub(crate) fast: bool,
    pub(crate) variants: Vec<Variant>,
    pub(crate) def: Option<Variant>,
    pub(crate) partial: bool,
    pub(crate) model: PickerModel,
    pub(crate) fam: Vec<UsableMember>,
    pub(crate) ids: Option<Arc<HashSet<String>>>,
    pub(crate) usable_only: bool,
    pub(crate) offers_fast: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct UsableMember {
    pub(crate) id: String,
    pub(crate) effort: String,
    pub(crate) fast: bool,
    pub(crate) max_mode: bool,
    pub(crate) name: String,
}

#[derive(Clone)]
struct CachedList {
    at: Duration,
    entries: Vec<ListEntry>,
}

#[derive(Default)]
struct ListingCache {
    lists: HashMap<String, CachedList>,
    list_failed: HashMap<String, Duration>,
    in_flight: HashSet<String>,
}

fn cache() -> &'static Mutex<ListingCache> {
    static CACHE: OnceLock<Mutex<ListingCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ListingCache::default()))
}

/// Clears listing cache state for isolated tests.
#[cfg(test)]
#[doc(hidden)]
pub fn clear_listing_cache_for_tests() {
    let mut guard = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.lists.clear();
    guard.list_failed.clear();
    guard.in_flight.clear();
}

fn token_hash(token: &SecretString) -> String {
    let digest = hofund_plugin_sdk::digest::sha256(secret_text(token).as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(out, "{byte:02x}").expect("hex");
    }
    out
}

/// Blocking refresh: returns the merged catalog for `token`.
pub(crate) async fn usable(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Result<Vec<ListEntry>, PluginError> {
    let h = token_hash(token);
    let now = host.clock().monotonic();
    {
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = guard.lists.get(&h)
            && now.saturating_sub(entry.at) < MODELS_KEEP
        {
            return Ok(entry.entries.clone());
        }
    }
    listing(host, endpoints, token, &h).await
}

/// Stale-while-revalidate list for request-time variant picking.
pub(crate) async fn list_for(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Vec<ListEntry> {
    let h = token_hash(token);
    let now = host.clock().monotonic();
    let stale = {
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.lists.get(&h).cloned()
    };
    if let Some(entry) = &stale
        && now.saturating_sub(entry.at) < MODELS_KEEP
    {
        return entry.entries.clone();
    }
    let blocked = {
        let guard = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .list_failed
            .get(&h)
            .is_some_and(|failed| now.saturating_sub(*failed) < LIST_RETRY)
    };
    if let Some(entry) = stale {
        let host = host.clone();
        let endpoints = endpoints.clone();
        let token = token.clone();
        let h_clone = h.clone();
        let _ = host.clone().scheduler().run_once(
            "cursor-model-list",
            Box::pin(async move {
                let _ = listing(&host, &endpoints, &token, &h_clone).await;
            }),
        );
        return entry.entries;
    }
    if blocked {
        return Vec::new();
    }
    listing(host, endpoints, token, &h)
        .await
        .unwrap_or_default()
}

async fn listing(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
    h: &str,
) -> Result<Vec<ListEntry>, PluginError> {
    loop {
        let leader = {
            let mut guard = cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.in_flight.contains(h) {
                false
            } else {
                guard.in_flight.insert(h.to_owned());
                true
            }
        };
        if leader {
            break;
        }
        let now = host.clock().monotonic();
        let cached = {
            let guard = cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .lists
                .get(h)
                .filter(|entry| now.saturating_sub(entry.at) < MODELS_KEEP)
                .map(|entry| entry.entries.clone())
        };
        if let Some(entries) = cached {
            return Ok(entries);
        }
        let failed = {
            let guard = cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.list_failed.contains_key(h) && !guard.in_flight.contains(h)
        };
        if failed {
            return Err(PluginError::config("Cursor listed no models"));
        }
        hofund_plugin_sdk::sleep(Duration::from_millis(1)).await;
    }
    let result = fetch_listing(host, endpoints, token, h).await;
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .in_flight
        .remove(h);
    result
}

async fn fetch_listing(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
    h: &str,
) -> Result<Vec<ListEntry>, PluginError> {
    let now = host.clock().monotonic();
    let listed = usable_list(host, endpoints, token).await;
    let catalog = catalog_list(host, endpoints, token).await;
    let picker_result = connect::picker_models(host, endpoints, token).await;
    let picker_body = match picker_result {
        Ok(body) => body,
        Err(error) => {
            cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .list_failed
                .insert(h.to_owned(), now);
            return Err(error);
        }
    };
    let picker: Vec<PickerModel> = parse_all(&picker_body);
    let ids: HashSet<String> = listed
        .iter()
        .flat_map(|row| [row.id.as_str(), row.display_id.as_deref().unwrap_or("")])
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    let merged_picker = if ids.is_empty() {
        picker.clone()
    } else {
        let extra = from_catalog(&picker, &catalog, &ids);
        let mut all = picker;
        all.extend(extra);
        all
    };
    let ids_arc = if ids.is_empty() {
        None
    } else {
        Some(Arc::new(ids))
    };
    let raw = models_of(&merged_picker, ids_arc.as_deref(), &listed);
    if raw.is_empty() {
        cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list_failed
            .insert(h.to_owned(), now);
        return Err(PluginError::config("Cursor listed no models"));
    }
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lists
        .insert(
            h.to_owned(),
            CachedList {
                at: now,
                entries: raw.clone(),
            },
        );
    cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_failed
        .remove(h);
    Ok(raw)
}

#[derive(Clone, Debug)]
struct UsableRow {
    id: String,
    display_id: Option<String>,
    display_name: String,
    max_mode: bool,
}

async fn usable_list(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Vec<UsableRow> {
    let Ok(body) = connect::usable_models(host, endpoints, token).await else {
        return Vec::new();
    };
    body.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let id = row.get("modelId").and_then(Value::as_str)?.to_owned();
            if id.is_empty() {
                return None;
            }
            Some(UsableRow {
                id,
                display_id: row
                    .get("displayModelId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                display_name: row
                    .get("displayName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                max_mode: row.get("maxMode").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

async fn catalog_list(
    host: &HostContext,
    endpoints: &Endpoints,
    token: &SecretString,
) -> Vec<PickerModel> {
    let Ok(body) = connect::catalog_models(host, endpoints, token).await else {
        return Vec::new();
    };
    if !body.get("models").is_some_and(Value::is_array) {
        return Vec::new();
    }
    parse_all(&body)
}

/// Catalog models the picker lacks, Anthropic only, with inferred slugs.
#[doc(hidden)]
pub fn from_catalog(
    picker: &[PickerModel],
    catalog: &[PickerModel],
    ids: &HashSet<String>,
) -> Vec<PickerModel> {
    let have: HashSet<&str> = picker.iter().map(|model| model.name.as_str()).collect();
    let mut out = Vec::new();
    for model in catalog {
        if model.name.is_empty()
            || have.contains(model.name.as_str())
            || model.is_hidden
            || model.name == "default"
        {
            continue;
        }
        if model.vendor_id.as_deref() != Some(ANTHROPIC_VENDOR) {
            continue;
        }
        if model.is_chat_only
            || model.only_supports_cmd_k
            || !model.supports_agent
            || crate::picker_model::PICKER_SKIP.contains(&model.name.as_str())
        {
            continue;
        }
        let full = with_slugs(model);
        if full.variants.iter().any(|variant| {
            variant
                .legacy_slug
                .as_deref()
                .is_some_and(|slug| ids.contains(slug))
        }) {
            out.push(full);
        }
    }
    out
}

fn with_slugs(model: &PickerModel) -> PickerModel {
    let defined: HashSet<&str> = model.definitions.iter().map(|d| d.id.as_str()).collect();
    let variants: Vec<Variant> = model
        .variants
        .iter()
        .filter(|variant| !variant.values.is_empty())
        .map(|variant| {
            let values: Vec<ModelParameter> = variant
                .values
                .iter()
                .filter(|parameter| defined.contains(parameter.id.as_str()))
                .cloned()
                .collect();
            Variant {
                values,
                ..variant.clone()
            }
        })
        .collect();
    let mut full = model.clone();
    full.variants = variants
        .into_iter()
        .map(|variant| {
            if variant.legacy_slug.is_some() {
                return variant;
            }
            let slug = slug_for(model, &variant);
            Variant {
                legacy_slug: Some(if slug.is_empty() {
                    NO_ID.to_owned()
                } else {
                    slug
                }),
                ..variant
            }
        })
        .collect();
    full
}

/// Infers legacySlug for catalog variants that omit it.
#[doc(hidden)]
pub fn slug_for(model: &PickerModel, variant: &Variant) -> String {
    if let Some(slug) = variant.legacy_slug.as_ref().filter(|slug| !slug.is_empty()) {
        return slug.clone();
    }
    let have = values_of(variant);
    let effort = effort_level(&have);
    let effort = normalized_level(&effort);
    let effort_at: Option<&str> = if effort.is_empty() {
        None
    } else {
        Some(effort)
    };
    let mut hits: Vec<String> = model
        .legacy_slugs
        .iter()
        .filter(|id| !id.is_empty())
        .filter(|id| slug_matches(id, &have, effort_at))
        .cloned()
        .collect();
    if hits.len() == 1 {
        return hits.remove(0);
    }
    if let Some(at) = effort_at
        && let Some(def) = default_variant(model)
    {
        let def_level = effort_level(&values_of(def));
        let def_effort = normalized_level(&def_level);
        if def_effort == at {
            let hits: Vec<String> = model
                .legacy_slugs
                .iter()
                .filter(|id| slug_matches(id, &have, None))
                .cloned()
                .collect();
            if hits.len() == 1 {
                return hits[0].clone();
            }
        }
    }
    String::new()
}

fn slug_matches(id: &str, have: &HashMap<String, String>, effort: Option<&str>) -> bool {
    let (family, id_effort, fast) = family_of(id);
    (effort.is_none() || id_effort == effort.unwrap_or(""))
        && (!have.contains_key("thinking")
            || family.ends_with("-thinking") == (have.get("thinking") == Some(&"true".to_owned())))
        && (!have.contains_key("fast") || fast == (have.get("fast") == Some(&"true".to_owned())))
}

fn effort_level(have: &HashMap<String, String>) -> String {
    for key in EFFORT_PARAMS {
        if let Some(value) = have.get(*key) {
            return value.replace("extra-high", "xhigh");
        }
    }
    String::new()
}

fn family_of(id: &str) -> (String, String, bool) {
    let (family, effort) = models::split_id(id);
    let fast = family.len() > "-fast".len() && family.ends_with("-fast");
    let family = if fast {
        family.strip_suffix("-fast").unwrap_or(&family).to_owned()
    } else {
        family
    };
    (family, effort, fast)
}

fn families_of(listed: &[UsableRow]) -> HashMap<String, Vec<UsableMember>> {
    let mut fams: HashMap<String, Vec<UsableMember>> = HashMap::new();
    for row in listed {
        let (family, effort, fast) = family_of(&row.id);
        fams.entry(family).or_default().push(UsableMember {
            id: row.id.clone(),
            effort,
            fast,
            max_mode: row.max_mode,
            name: clean(&row.display_name),
        });
    }
    fams
}

fn from_usable<'a>(fam: &'a [UsableMember], effort: &str, fast: bool) -> &'a UsableMember {
    let effort = if effort == "off" { "none" } else { effort };
    let mut vs: Vec<&UsableMember> = fam.iter().filter(|u| u.fast == fast).collect();
    if vs.is_empty() {
        vs = fam.iter().collect();
    }
    let at = if effort.is_empty() {
        String::new()
    } else {
        fit_effort(
            effort,
            &vs.iter()
                .filter_map(|u| (!u.effort.is_empty()).then_some(u.effort.as_str()))
                .collect::<Vec<_>>(),
        )
    };
    vs.iter()
        .find(|u| !at.is_empty() && u.effort == at)
        .or_else(|| vs.iter().find(|u| u.effort.is_empty()))
        .or_else(|| vs.iter().find(|u| u.effort == "medium"))
        .or_else(|| vs.first())
        .copied()
        .expect("non-empty family")
}

const EFFORT_WORDS: &[(&str, &[&str])] = &[
    ("none", &["None"]),
    ("minimal", &["Minimal"]),
    ("low", &["Low"]),
    ("medium", &["Medium"]),
    ("high", &["High"]),
    ("xhigh", &["Extra High", "XHigh"]),
    ("max", &["Max"]),
];

fn usable_title(member: &UsableMember) -> String {
    let mut words: Vec<String> = member.name.split_whitespace().map(str::to_owned).collect();
    let mut cut_last = |phrase: &str| -> bool {
        let lowered = phrase.to_ascii_lowercase();
        let parts: Vec<&str> = lowered.split_whitespace().collect();
        for start in (1..words.len()).rev() {
            if start + parts.len() > words.len() {
                continue;
            }
            if parts.iter().enumerate().all(|(i, word)| {
                words
                    .get(start + i)
                    .is_some_and(|w| w.to_ascii_lowercase() == *word)
            }) {
                words.drain(start..start + parts.len());
                return true;
            }
        }
        false
    };
    let member_effort = member.effort.as_str();
    for (_, phrases) in EFFORT_WORDS
        .iter()
        .filter(|(level, _)| *level == member_effort)
    {
        for phrase in *phrases {
            if cut_last(phrase) {
                break;
            }
        }
    }
    if member.fast {
        cut_last("fast");
    }
    let title = words
        .iter()
        .enumerate()
        .filter(|(i, word)| *i == 0 || (!word.eq_ignore_ascii_case("1m") && *word != "(1M)"))
        .map(|(_, word)| word.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        member.name.clone()
    } else {
        title
    }
}

fn served(model: &PickerModel, ids: Option<&HashSet<String>>) -> Vec<Variant> {
    let variants = model.variants.clone();
    let Some(ids) = ids else {
        return variants;
    };
    let got: Vec<Variant> = variants
        .iter()
        .filter(|variant| {
            variant
                .legacy_slug
                .as_deref()
                .is_none_or(|slug| ids.contains(slug))
        })
        .cloned()
        .collect();
    if got.iter().any(|variant| variant.legacy_slug.is_some()) {
        got
    } else {
        variants
    }
}

fn efforts_of_defs(defs: &HashMap<String, Vec<String>>) -> (String, HashMap<String, String>) {
    let pid = EFFORT_PARAMS
        .iter()
        .find(|key| defs.get(**key).is_some_and(|values| !values.is_empty()))
        .map(|key| key.to_string())
        .unwrap_or_default();
    if pid.is_empty() {
        if defs.get("thinking").is_some_and(|values| {
            values.contains(&"true".to_owned()) && values.contains(&"false".to_owned())
        }) {
            let mut levels = HashMap::new();
            levels.insert("none".to_owned(), "false".to_owned());
            levels.insert("high".to_owned(), "true".to_owned());
            return ("thinking".to_owned(), levels);
        }
        return (String::new(), HashMap::new());
    }
    let mut levels = HashMap::new();
    for value in defs.get(&pid).into_iter().flatten() {
        let level = if value == "extra-high" {
            "xhigh"
        } else {
            value.as_str()
        };
        if LEVEL_RANK.contains(&level) {
            levels.insert(level.to_owned(), value.clone());
        }
    }
    (pid, levels)
}

fn models_of(
    picker: &[PickerModel],
    ids: Option<&HashSet<String>>,
    listed: &[UsableRow],
) -> Vec<ListEntry> {
    let fams = families_of(listed);
    let mut out = Vec::new();
    let mut used = HashSet::new();
    let mut named = HashSet::new();
    for model in picker {
        if model.name.is_empty() {
            continue;
        }
        for alias in model
            .legacy_slugs
            .iter()
            .chain(model.id_aliases.iter())
            .chain(model.variants.iter().filter_map(|v| v.legacy_slug.as_ref()))
        {
            named.insert(model.name.clone());
            named.insert(alias.clone());
        }
        named.insert(model.name.clone());
    }
    for model in picker {
        if !model.visible_in_picker() {
            continue;
        }
        let defs = param_defs(model);
        let variants = served(model, ids);
        let partial = variants.len() < model.variants.len();
        let has = |pid: &str, value: &str| {
            !partial
                || !variants
                    .iter()
                    .any(|variant| values_of(variant).contains_key(pid))
                || variants
                    .iter()
                    .any(|variant| values_of(variant).get(pid) == Some(&value.to_owned()))
        };
        let def = default_variant(model).cloned();
        let (pid, all_levels) = efforts_of_defs(&defs);
        let levels: HashMap<String, String> = all_levels
            .into_iter()
            .filter(|(_, value)| has(&pid, value))
            .collect();
        let own = family_of(
            def.as_ref()
                .and_then(|v| v.legacy_slug.as_deref())
                .unwrap_or(&model.name),
        )
        .0;
        let bare = own.strip_suffix("-thinking").unwrap_or(&own).to_owned();
        let fam = fams
            .get(&own)
            .or_else(|| fams.get(&bare))
            .or_else(|| fams.get(&(bare.clone() + "-thinking")))
            .cloned()
            .unwrap_or_default();
        let title = model.title();
        let fast = defs.get("fast").is_some_and(|values| !values.is_empty());
        let base = EntryBase {
            run: model.name.clone(),
            pid: pid.clone(),
            levels: levels.clone(),
            fast,
            variants: variants.clone(),
            def: def.clone(),
            partial,
            model: model.clone(),
            fam: fam.clone(),
            ids: ids.map(|set| Arc::new(set.clone())),
        };
        let contexts: Vec<String> = defs
            .get("context")
            .into_iter()
            .flatten()
            .filter(|context| has("context", context))
            .cloned()
            .collect();
        if !contexts.is_empty() {
            for context in contexts {
                let id = format!("{}@{context}", model.name);
                if used.contains(&id) {
                    continue;
                }
                used.insert(id.clone());
                out.push(ListEntry {
                    id: id.clone(),
                    name: format!("{title} @ {context}"),
                    run: base.run.clone(),
                    context: tokens_of(context.as_str()).max(DEFAULT_CONTEXT),
                    size: context.clone(),
                    pid: base.pid.clone(),
                    levels: base.levels.clone(),
                    fast: base.fast,
                    variants: base.variants.clone(),
                    def: base.def.clone(),
                    partial: base.partial,
                    model: base.model.clone(),
                    fam: base.fam.clone(),
                    ids: base.ids.clone(),
                    usable_only: false,
                    offers_fast: fast_at(&base.variants, Some(context.as_str())),
                });
            }
        } else if !used.contains(&model.name) {
            used.insert(model.name.clone());
            let limit = if needs_max(model, def.as_ref()) {
                model
                    .context_token_limit_for_max_mode
                    .max(model.context_token_limit)
            } else {
                model.context_token_limit
            };
            let id = if model.name == "default" {
                "auto".to_owned()
            } else {
                model.name.clone()
            };
            out.push(ListEntry {
                id,
                name: title,
                run: base.run,
                context: if limit > 0 { limit } else { DEFAULT_CONTEXT },
                size: String::new(),
                pid: base.pid,
                levels: base.levels,
                fast: base.fast,
                variants: base.variants.clone(),
                def: base.def,
                partial: base.partial,
                model: base.model,
                fam: base.fam,
                ids: base.ids,
                usable_only: false,
                offers_fast: fast_at(&base.variants, None),
            });
        }
    }
    for (family, fam) in &fams {
        if fam.iter().any(|u| named.contains(&u.id))
            || named.contains(family)
            || used.contains(family)
        {
            continue;
        }
        used.insert(family.clone());
        let levels: HashMap<String, String> = fam
            .iter()
            .filter(|u| LEVEL_RANK.contains(&u.effort.as_str()))
            .map(|u| (u.effort.clone(), u.effort.clone()))
            .collect();
        let def = from_usable(fam, "", false);
        let title = usable_title(def);
        out.push(ListEntry {
            id: family.clone(),
            name: title.clone(),
            run: family.clone(),
            context: DEFAULT_CONTEXT,
            size: String::new(),
            pid: String::new(),
            levels,
            fast: false,
            variants: Vec::new(),
            def: None,
            partial: false,
            model: PickerModel {
                name: family.clone(),
                client_display_name: Some(title),
                legacy_slugs: Vec::new(),
                id_aliases: Vec::new(),
                context_token_limit: 0,
                context_token_limit_for_max_mode: 0,
                supports_non_max_mode: Some(true),
                is_hidden: false,
                is_chat_only: false,
                only_supports_cmd_k: false,
                supports_agent: true,
                vendor_id: None,
                definitions: Vec::new(),
                variants: Vec::new(),
            },
            fam: fam.clone(),
            ids: ids.map(|set| Arc::new(set.clone())),
            usable_only: true,
            offers_fast: fam.iter().any(|u| u.fast),
        });
    }
    out
}

struct EntryBase {
    run: String,
    pid: String,
    levels: HashMap<String, String>,
    fast: bool,
    variants: Vec<Variant>,
    def: Option<Variant>,
    partial: bool,
    model: PickerModel,
    fam: Vec<UsableMember>,
    ids: Option<Arc<HashSet<String>>>,
}

fn fast_at(variants: &[Variant], size: Option<&str>) -> bool {
    variants.iter().any(|variant| {
        let have = values_of(variant);
        have.get("fast") == Some(&"true".to_owned())
            && size.is_none_or(|value| {
                !have.contains_key("context") || have.get("context") == Some(&value.to_owned())
            })
    })
}

fn needs_max(model: &PickerModel, variant: Option<&Variant>) -> bool {
    model.supports_non_max_mode == Some(false) || variant.is_some_and(|v| v.is_max)
}

pub(crate) fn tokens_of(value: &str) -> u64 {
    let value = value.trim();
    let split = value
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
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
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| (n * multiplier).round() as u64)
        .unwrap_or(0)
}

fn fit_effort(want: &str, levels: &[&str]) -> String {
    let want = if want == "ultra" {
        "max"
    } else if want == "off" {
        "none"
    } else {
        want
    };
    if levels.is_empty() || levels.contains(&want) {
        return want.to_owned();
    }
    let Some(at) = LEVEL_RANK.iter().position(|level| *level == want) else {
        return String::new();
    };
    let mut best = "";
    let mut distance = LEVEL_RANK.len();
    for level in levels {
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

fn normalized_level(value: &str) -> &str {
    if value == "extra-high" {
        "xhigh"
    } else {
        value
    }
}

/// Resolves how a chat names its model for the agent API.
pub(crate) fn request(raw: &[ListEntry], model: &str, effort: &str, fast: bool) -> ModelChoice {
    if let Some(entry) = raw.iter().find(|entry| entry.id == model) {
        return pick(entry, effort, fast, None);
    }
    if let Some(bare) = raw
        .iter()
        .find(|entry| entry.id == model.rsplit_once('@').map_or(model, |(base, _)| base))
        && model != bare.id
        && bare.size.is_empty()
    {
        let mut choice = pick(bare, effort, fast, None);
        let size = model.rsplit('@').next().unwrap_or("");
        if tokens_of(size) > bare.context {
            choice.max_mode = true;
        }
        return choice;
    }
    if let Some(old) = named(raw, model) {
        let mut choice = pick(
            &old.entry,
            if effort.is_empty() {
                old.effort.as_str()
            } else {
                effort
            },
            fast || old.fast,
            old.seed.as_ref(),
        );
        if old.force_max {
            choice.max_mode = true;
        }
        return choice;
    }
    ModelChoice {
        id: if model == "auto" {
            "default".to_owned()
        } else {
            model.to_owned()
        },
        parameters: Vec::new(),
        max_mode: false,
    }
}

struct NamedMatch {
    entry: ListEntry,
    seed: Option<Variant>,
    effort: String,
    fast: bool,
    force_max: bool,
}

fn named(raw: &[ListEntry], id: &str) -> Option<NamedMatch> {
    let unique: Vec<&ListEntry> = {
        let mut seen = HashSet::new();
        raw.iter()
            .filter(|entry| seen.insert(entry.model.name.clone()))
            .collect()
    };
    let at = |entry: &ListEntry, variant: &Variant| {
        let want_context = values_of(variant)
            .get("context")
            .cloned()
            .unwrap_or_default();
        raw.iter()
            .find(|e| e.model.name == entry.model.name && e.size == want_context)
            .cloned()
            .unwrap_or_else(|| entry.clone())
    };
    let as_match = |entry: &ListEntry, vs: &[Variant], effort: &str, fast: bool| {
        let def = entry.def.as_ref().expect("default variant");
        let seed = nearest_variant(vs, &def.values);
        let entry = at(entry, &seed);
        NamedMatch {
            entry,
            seed: Some(seed),
            effort: effort.to_owned(),
            fast,
            force_max: false,
        }
    };

    for entry in &unique {
        let vs: Vec<&Variant> = entry
            .variants
            .iter()
            .filter(|v| v.legacy_slug.as_deref() == Some(id))
            .collect();
        if !vs.is_empty() {
            let owned: Vec<Variant> = vs.iter().map(|v| (*v).clone()).collect();
            let seed = nearest_variant(&owned, &[]);
            let fast = values_of(&seed).get("fast") == Some(&"true".to_owned());
            return Some(NamedMatch {
                entry: at(entry, &seed),
                seed: Some(seed),
                effort: String::new(),
                fast,
                force_max: false,
            });
        }
    }
    let (family, effort) = models::split_id(id);
    for entry in &unique {
        let vs: Vec<Variant> = entry
            .variants
            .iter()
            .filter(|v| {
                v.legacy_slug
                    .as_deref()
                    .is_some_and(|slug| models::split_id(slug).0 == family)
            })
            .cloned()
            .collect();
        if !vs.is_empty() {
            return Some(as_match(entry, &vs, &effort, false));
        }
    }
    for entry in &unique {
        if entry.usable_only {
            continue;
        }
        let aliases: Vec<&str> = std::iter::once(entry.model.name.as_str())
            .chain(entry.model.legacy_slugs.iter().map(String::as_str))
            .chain(entry.model.id_aliases.iter().map(String::as_str))
            .collect();
        if aliases.contains(&id) {
            let def = entry.def.clone().expect("default variant");
            return Some(NamedMatch {
                entry: at(entry, &def),
                seed: None,
                effort: String::new(),
                fast: false,
                force_max: false,
            });
        }
    }
    let of = family_of(id);
    for entry in &unique {
        if !entry.usable_only {
            continue;
        }
        if let Some(u) = entry.fam.iter().find(|u| u.id == id) {
            return Some(NamedMatch {
                entry: (*entry).clone(),
                seed: None,
                effort: u.effort.clone(),
                fast: u.fast,
                force_max: false,
            });
        }
        if entry.id == of.0 {
            return Some(NamedMatch {
                entry: (*entry).clone(),
                seed: None,
                effort: of.1.clone(),
                fast: of.2,
                force_max: false,
            });
        }
    }
    let loose = |family: &str| -> String {
        family
            .strip_suffix("-thinking")
            .unwrap_or(family)
            .to_owned()
    };
    let key = loose(&of.0);
    for entry in &unique {
        if entry.usable_only {
            if loose(&entry.id) == key.as_str() {
                return Some(NamedMatch {
                    entry: (*entry).clone(),
                    seed: None,
                    effort: of.1.clone(),
                    fast: of.2,
                    force_max: false,
                });
            }
            continue;
        }
        let vs: Vec<Variant> = entry
            .variants
            .iter()
            .filter(|v| {
                v.legacy_slug
                    .as_deref()
                    .is_some_and(|slug| loose(&family_of(slug).0) == key.as_str())
            })
            .cloned()
            .collect();
        if !vs.is_empty() {
            return Some(as_match(entry, &vs, &of.1, of.2));
        }
    }
    if let Some((base, size)) = id.rsplit_once('@') {
        let want = tokens_of(size);
        let same: Vec<&ListEntry> = raw
            .iter()
            .filter(|e| e.run == base && !e.size.is_empty())
            .collect();
        if !same.is_empty() {
            let near = same
                .iter()
                .min_by_key(|e| tokens_of(&e.size).abs_diff(want))
                .copied()
                .expect("non-empty");
            return Some(NamedMatch {
                entry: (*near).clone(),
                seed: None,
                effort: String::new(),
                fast: false,
                force_max: false,
            });
        }
        if let Some(old) = named(raw, base) {
            let sizes: Vec<&ListEntry> = raw
                .iter()
                .filter(|e| !e.size.is_empty() && e.model.name == old.entry.model.name)
                .collect();
            if !old.entry.size.is_empty() && !sizes.is_empty() {
                let near = sizes
                    .iter()
                    .min_by_key(|e| tokens_of(&e.size).abs_diff(want))
                    .copied()
                    .expect("sizes");
                return Some(NamedMatch {
                    entry: (*near).clone(),
                    seed: old.seed,
                    effort: old.effort,
                    fast: old.fast,
                    force_max: false,
                });
            }
            let force_max = !old.entry.size.is_empty() && want > old.entry.context;
            return Some(NamedMatch {
                entry: old.entry,
                seed: old.seed,
                effort: old.effort,
                fast: old.fast,
                force_max,
            });
        }
    }
    None
}

fn nearest_variant(variants: &[Variant], want: &[ModelParameter]) -> Variant {
    let want_map: HashMap<String, String> = want
        .iter()
        .map(|p| (p.id.clone(), p.value.clone()))
        .collect();
    let mut best = variants.first().cloned().unwrap_or_default();
    let mut most = -1_i32;
    for variant in variants {
        let have = values_of(variant);
        let mut score = want_map
            .iter()
            .filter(|(k, v)| have.get(*k) == Some(v))
            .count() as i32;
        if have.get("context") == want_map.get("context") {
            score += 100;
        }
        if score > most {
            best = variant.clone();
            most = score;
        }
    }
    best
}

fn pick(entry: &ListEntry, effort: &str, fast: bool, seed: Option<&Variant>) -> ModelChoice {
    if entry.usable_only {
        let member = from_usable(&entry.fam, effort, fast);
        return ModelChoice {
            id: member.id.clone(),
            parameters: Vec::new(),
            max_mode: member.max_mode,
        };
    }
    let def = entry.def.as_ref();
    let mut want: HashMap<String, String> = seed.or(def).map(values_of).unwrap_or_default();
    if !entry.size.is_empty() {
        want.insert("context".to_owned(), entry.size.clone());
    }
    let effort = if effort == "off" { "none" } else { effort };
    if !entry.pid.is_empty()
        && entry.pid != "thinking"
        && effort == "none"
        && !entry.levels.contains_key("none")
        && want.contains_key("thinking")
    {
        want.insert("thinking".to_owned(), "false".to_owned());
    } else if !entry.pid.is_empty() {
        let at = if effort.is_empty() {
            String::new()
        } else {
            fit_effort(
                effort,
                &entry.levels.keys().map(String::as_str).collect::<Vec<_>>(),
            )
        };
        if let Some(value) = entry.levels.get(&at) {
            want.insert(entry.pid.clone(), value.clone());
            if seed.is_none()
                && entry.pid != "thinking"
                && want.contains_key("thinking")
                && !at.is_empty()
            {
                want.insert("thinking".to_owned(), "true".to_owned());
            }
        }
    }
    if entry.fast {
        want.insert(
            "fast".to_owned(),
            if fast { "true" } else { "false" }.to_owned(),
        );
    }
    let selected: Option<Variant> = entry
        .variants
        .iter()
        .find(|variant| same_variant(&values_of(variant), &want))
        .cloned()
        .or_else(|| seed.and_then(|_| branch(entry, &want)))
        .or_else(|| {
            entry
                .partial
                .then(|| nearest_variant(&entry.variants, &want_to_params(&want)))
        });
    let params = if let Some(selected) = &selected {
        let mut merged = want.clone();
        merged.extend(values_of(selected));
        want_to_params(&merged)
    } else {
        want_to_params(&want)
    };
    let max_mode = selected
        .as_ref()
        .map(|v| needs_max(&entry.model, Some(v)))
        .unwrap_or_else(|| {
            needs_max(&entry.model, def)
                || tokens_of(&entry.size) > entry.model.context_token_limit
                    && entry.model.context_token_limit > 0
        });
    let id = selected
        .as_ref()
        .and_then(|v| v.legacy_slug.clone())
        .or_else(|| def.and_then(|d| d.legacy_slug.clone()))
        .unwrap_or_else(|| entry.run.clone());
    if let Some(ids) = &entry.ids
        && !ids.contains(&id)
        && !entry.fam.is_empty()
    {
        let level = if !entry.pid.is_empty() && entry.pid != "thinking" {
            params
                .iter()
                .find(|p| p.id == entry.pid)
                .map(|p| p.value.replace("extra-high", "xhigh"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let member = from_usable(
            &entry.fam,
            if effort.is_empty() {
                level.as_str()
            } else {
                effort
            },
            fast,
        );
        return ModelChoice {
            id: member.id.clone(),
            parameters: Vec::new(),
            max_mode: max_mode || member.max_mode,
        };
    }
    ModelChoice {
        id,
        parameters: filter_defined_params(entry, params),
        max_mode,
    }
}

fn want_to_params(want: &HashMap<String, String>) -> Vec<ModelParameter> {
    want.iter()
        .map(|(id, value)| ModelParameter {
            id: id.clone(),
            value: value.clone(),
        })
        .collect()
}

fn filter_defined_params(entry: &ListEntry, params: Vec<ModelParameter>) -> Vec<ModelParameter> {
    let defined: HashSet<&str> = entry
        .model
        .definitions
        .iter()
        .map(|definition| definition.id.as_str())
        .collect();
    params
        .into_iter()
        .filter(|parameter| defined.contains(parameter.id.as_str()))
        .collect()
}

fn same_variant(have: &HashMap<String, String>, want: &HashMap<String, String>) -> bool {
    for key in have.keys().chain(want.keys()).collect::<HashSet<_>>() {
        match (have.get(key), want.get(key)) {
            (Some(a), Some(b)) if a != b => return false,
            _ => {}
        }
    }
    true
}

fn branch(entry: &ListEntry, want: &HashMap<String, String>) -> Option<Variant> {
    if entry.pid.is_empty() || entry.pid == "thinking" {
        return None;
    }
    let def = values_of(entry.def.as_ref()?);
    let wanted_level = want.get(&entry.pid).map(|v| normalized_level(v).to_owned());
    let named = entry.variants.iter().any(|variant| {
        values_of(variant)
            .get(&entry.pid)
            .map(|v| normalized_level(v).to_owned())
            == wanted_level
    });
    if !named
        && want.iter().all(|(key, value)| {
            *key == entry.pid
                || *key == "context"
                || def.get(key).is_none_or(|def_value| def_value == value)
        })
    {
        return None;
    }
    let candidates: Vec<&Variant> = entry
        .variants
        .iter()
        .filter(|variant| {
            values_of(variant).contains_key(&entry.pid)
                && want.iter().all(|(key, value)| {
                    key == &entry.pid
                        || values_of(variant).get(key).is_none_or(|held| held == value)
                })
        })
        .collect();
    let levels: Vec<String> = candidates
        .iter()
        .filter_map(|variant| values_of(variant).get(&entry.pid).cloned())
        .map(|value| normalized_level(&value).to_owned())
        .collect();
    let at = fit_effort(
        want.get(&entry.pid)
            .map(|v| normalized_level(v))
            .unwrap_or(""),
        &levels.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    candidates
        .into_iter()
        .find(|variant| {
            values_of(variant)
                .get(&entry.pid)
                .map(|v| normalized_level(v))
                == Some(at.as_str())
        })
        .cloned()
}

/// Builds the API model list from merged entries.
pub(crate) fn model_list(entries: &[ListEntry]) -> Result<ModelList, PluginError> {
    let specs = entries
        .iter()
        .map(|entry| -> Result<ModelSpec, PluginError> {
            let efforts = LEVEL_RANK
                .iter()
                .filter(|level| entry.levels.contains_key(**level))
                .map(|level| (*level).to_owned())
                .collect::<Vec<_>>();
            let mut spec =
                ModelSpec::new(&entry.id, Some(entry.name.clone()))?.with_context(entry.context);
            if entry.offers_fast {
                spec = spec.with_service_tiers(vec!["fast".to_owned()]);
            }
            if !efforts.is_empty() {
                spec = spec.with_efforts(efforts);
            }
            Ok(spec)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ModelList::new(specs, None))
}
