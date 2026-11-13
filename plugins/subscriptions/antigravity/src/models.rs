//! Antigravity's account catalogs, effort families, and tier fallback.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, HostContext, ModelList, ModelSpec, PluginError, SecretString};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::oauth::{SecretObject, TokenCache};
use hofund_plugin_sdk::time::parse_rfc3339;

use crate::{Endpoints, client, project, tokens};

const REFUSED_FOR: Duration = Duration::from_secs(24 * 60 * 60);
const TIERS_FOR: Duration = Duration::from_secs(60 * 60);
const ACCOUNT_LIST_TIMEOUT: Duration = Duration::from_secs(8);
const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
const EFFORT_WORDS: &[(&str, &str)] = &[
    ("extra-high", "xhigh"),
    ("extra-low", "minimal"),
    ("xhigh", "xhigh"),
    ("minimal", "minimal"),
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
];
const HIDDEN: &[&str] = &[
    "chat_20706",
    "chat_23310",
    "tab_flash_lite_preview",
    "tab_jump_flash_lite_preview",
    "gemini-2.5-flash-thinking",
    "gemini-2.5-pro",
];
const FALLBACK: &[(&str, &str)] = &[
    ("gemini-pro-agent", "Gemini 3.1 Pro (High)"),
    ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
    ("gemini-3-flash", "Gemini 3 Flash"),
    ("gemini-3.1-flash-lite", "Gemini 3.1 Flash Lite"),
    ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

#[derive(Clone, Debug)]
pub(crate) struct RawModel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) context: u64,
    pub(crate) output: u64,
    pub(crate) images: Option<bool>,
    pub(crate) remaining: Option<f64>,
    pub(crate) resets: Option<SystemTime>,
    picker: usize,
    enum_id: String,
}

impl RawModel {
    fn spec(&self) -> Result<ModelSpec, PluginError> {
        let name = if self.name.is_empty() {
            fallback_name(&self.id).unwrap_or_else(|| self.id.clone())
        } else {
            self.name.clone()
        };
        Ok(ModelSpec::new(self.id.clone(), Some(name))?
            .with_context(self.context)
            .with_max_output(self.output)
            .with_image_input(Some(
                self.images
                    .unwrap_or_else(|| !self.id.starts_with("gpt-oss")),
            )))
    }
}

#[derive(Clone)]
struct TierReading {
    tier: String,
    paid: String,
    at: SystemTime,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct RefusedKey {
    account: AccountId,
    project: String,
    tier: String,
}

#[derive(Default)]
struct State {
    listings: HashMap<AccountId, Vec<RawModel>>,
    tiers: HashMap<AccountId, TierReading>,
    refused: HashMap<RefusedKey, SystemTime>,
}

/// Per-instance catalogs and tier readings, shared by lists, quota, and calls.
#[derive(Default)]
pub(crate) struct Catalog {
    state: Mutex<State>,
}

impl Catalog {
    pub(crate) fn forget(&self, account: &AccountId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.listings.remove(account);
        state.tiers.remove(account);
        state.refused.retain(|key, _| &key.account != account);
    }

    pub(crate) fn forget_project(&self, account: &AccountId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tiers.remove(account);
        state.refused.retain(|key, _| &key.account != account);
    }

    pub(crate) fn own(&self, account: &AccountId) -> Vec<RawModel> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .listings
            .get(account)
            .cloned()
            .unwrap_or_default()
    }

    /// Fetches every stored account and merges its list in account order. A
    /// failed account keeps its last good list; a healthy account still lists.
    pub(crate) async fn pool(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        oauth: &client::OauthClient,
        tokens: &TokenCache<SecretString>,
        configured: Option<&str>,
    ) -> Result<ModelList, PluginError> {
        let accounts = host.accounts().list().await?;
        let mut merged = Vec::new();
        let mut last_error = None;
        for account in accounts {
            let id = account.id().clone();
            let result = hofund_plugin_sdk::timeout(
                ACCOUNT_LIST_TIMEOUT,
                self.fetch(host, endpoints, oauth, tokens, &id, configured),
            )
            .await
            .unwrap_or_else(|| Err(PluginError::timeout("Antigravity model listing timed out")));
            match result {
                Ok(raw) => {
                    self.state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .listings
                        .insert(id, raw.clone());
                    merged = merge(merged, collapse(&raw)?);
                }
                Err(error) => {
                    last_error = Some(error);
                    let cached = self.own(&id);
                    if !cached.is_empty() {
                        merged = merge(merged, collapse(&cached)?);
                    }
                }
            }
        }
        if merged.is_empty()
            && let Some(error) = last_error
        {
            return Err(error);
        }
        if merged.is_empty() {
            return Err(PluginError::auth(
                "no Antigravity sign-in is stored for this subscription",
            ));
        }
        Ok(ModelList::new(merged, None))
    }

    /// Reads one account's catalog and retains it for variant selection.
    pub(crate) async fn read(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        oauth: &client::OauthClient,
        tokens: &TokenCache<SecretString>,
        account: &AccountId,
        configured: Option<&str>,
    ) -> Result<Vec<RawModel>, PluginError> {
        let raw = self
            .fetch(host, endpoints, oauth, tokens, account, configured)
            .await?;
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .listings
            .insert(account.clone(), raw.clone());
        Ok(raw)
    }

    async fn fetch(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        oauth: &client::OauthClient,
        cache: &TokenCache<SecretString>,
        account: &AccountId,
        configured: Option<&str>,
    ) -> Result<Vec<RawModel>, PluginError> {
        let access = tokens::access(host, endpoints, oauth, cache, account).await?;
        let project_id = project::resolve(host, endpoints, account, &access, configured).await?;
        let (current, paid) = self.tiers(host, endpoints, account, &access).await;
        let tiers = self.ask_tiers(host, account, &project_id, &current, &paid);
        let mut last_error = None;
        for tier in tiers {
            let mut body = json!({"project": project_id});
            if !tier.is_empty() {
                body["entitlement"] = json!({"userTier": tier});
            }
            match project::call(
                host,
                &endpoints.code_assist,
                &access,
                "fetchAvailableModels",
                &body,
                &[],
            )
            .await
            {
                Ok(reply) => return parse(&reply),
                Err(error) => {
                    if !tier.is_empty() && error.status() == Some(400) {
                        self.state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .refused
                            .insert(
                                RefusedKey {
                                    account: account.clone(),
                                    project: project_id.clone(),
                                    tier,
                                },
                                host.clock().now(),
                            );
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            PluginError::status_error(502, "Antigravity returned no model list")
        }))
    }

    async fn tiers(
        &self,
        host: &HostContext,
        endpoints: &Endpoints,
        account: &AccountId,
        access: &SecretString,
    ) -> (String, String) {
        let now = host.clock().now();
        if let Some(reading) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .get(account)
            .cloned()
            && now.duration_since(reading.at).unwrap_or_default() < TIERS_FOR
        {
            return (reading.tier, reading.paid);
        }
        let stored = host.accounts().secret(account).await.ok().flatten();
        let stored = stored.and_then(|secret| SecretObject::parse(&secret).ok());
        if !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .contains_key(account)
            && let Some(blob) = &stored
            && (blob.text(project::TIER).is_some() || blob.text(project::PAID).is_some())
        {
            let reading = TierReading {
                tier: blob.text(project::TIER).unwrap_or_default().to_owned(),
                paid: blob.text(project::PAID).unwrap_or_default().to_owned(),
                at: now,
            };
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tiers
                .insert(account.clone(), reading.clone());
            return (reading.tier, reading.paid);
        }

        let loaded = project::call(
            host,
            &endpoints.load_code_assist,
            access,
            "loadCodeAssist",
            &json!({"metadata": {"ideType": "ANTIGRAVITY"}}),
            &[],
        )
        .await
        .map(|reply| project::tier_ids(&reply));
        let previous = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .get(account)
            .cloned();
        let (tier, paid) = match (loaded, previous, stored) {
            (Ok(ids), _, _) => ids,
            (Err(_), Some(reading), _) => (reading.tier, reading.paid),
            (Err(_), None, Some(blob)) => (
                blob.text(project::TIER).unwrap_or_default().to_owned(),
                blob.text(project::PAID).unwrap_or_default().to_owned(),
            ),
            (Err(_), None, None) => (String::new(), String::new()),
        };
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tiers
            .insert(
                account.clone(),
                TierReading {
                    tier: tier.clone(),
                    paid: paid.clone(),
                    at: now,
                },
            );
        (tier, paid)
    }

    fn ask_tiers(
        &self,
        host: &HostContext,
        account: &AccountId,
        project: &str,
        current: &str,
        paid: &str,
    ) -> Vec<String> {
        let now = host.clock().now();
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out = Vec::new();
        for tier in [paid, current] {
            if tier.is_empty()
                || matches!(tier, "free-tier" | "legacy-tier")
                || out.iter().any(|known| known == tier)
            {
                continue;
            }
            let refused = state
                .refused
                .get(&RefusedKey {
                    account: account.clone(),
                    project: project.to_owned(),
                    tier: tier.to_owned(),
                })
                .is_some_and(|at| now.duration_since(*at).unwrap_or_default() < REFUSED_FOR);
            if !refused {
                out.push(tier.to_owned());
            }
        }
        out.push(String::new());
        out
    }

    pub(crate) fn sent_id(&self, account: &AccountId, model: &str, effort: Option<&str>) -> String {
        sent_id(&self.own(account), model, effort)
    }
}

fn fallback_name(id: &str) -> Option<String> {
    FALLBACK
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, name)| (*name).to_owned())
}

fn parse(reply: &Value) -> Result<Vec<RawModel>, PluginError> {
    let models = reply
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for (id, value) in models {
        if HIDDEN.contains(&id.as_str())
            || id.starts_with("chat_")
            || id.starts_with("tab_")
            || value.get("disabled").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let quota = value.get("quotaInfo");
        let remaining = quota.map(|quota| {
            quota
                .get("remainingFraction")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        });
        let resets = quota
            .and_then(|quota| quota.get("resetTime"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        out.push(RawModel {
            id,
            name: value
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            context: value
                .get("maxTokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            output: value
                .get("maxOutputTokens")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            images: value.get("supportsImages").and_then(Value::as_bool),
            remaining,
            resets,
            picker: 0,
            enum_id: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        });
    }
    out.sort_by(|left, right| left.id.cmp(&right.id));

    let spelled: Vec<String> = out
        .iter()
        .filter(|model| name_spells(&model.name, &model.id))
        .map(|model| model.name.clone())
        .collect();
    for model in &mut out {
        if model.name.is_empty() || name_spells(&model.name, &model.id) {
            continue;
        }
        if spelled.contains(&model.name) || enum_names(&model.enum_id, &model.id) {
            model.name = fallback_name(&model.id).unwrap_or_default();
        }
    }

    let mut rank = HashMap::new();
    for sort in reply
        .get("agentModelSorts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for group in sort
            .get("groups")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for id in group
                .get("modelIds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !rank.contains_key(id) {
                    let next = rank.len() + 1;
                    rank.insert(id.to_owned(), next);
                }
            }
        }
    }
    for model in &mut out {
        model.picker = rank.get(&model.id).copied().unwrap_or_default();
    }
    out.sort_by_key(|model| (model.picker == 0, model.picker));
    Ok(out)
}

fn split_id(id: &str) -> (String, String) {
    for (word, level) in EFFORT_WORDS {
        let suffix = format!("-{word}");
        if let Some(family) = id.strip_suffix(&suffix)
            && !family.is_empty()
        {
            return (family.to_owned(), (*level).to_owned());
        }
    }
    (id.to_owned(), String::new())
}

fn name_effort(name: &str) -> (String, String) {
    let Some(open) = name.rfind(" (") else {
        return (name.to_owned(), String::new());
    };
    if !name.ends_with(')') {
        return (name.to_owned(), String::new());
    }
    let level = match name[open + 2..name.len() - 1].to_ascii_lowercase().as_str() {
        "minimal" | "extra low" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "extra high" | "xhigh" => "xhigh",
        _ => return (name.to_owned(), String::new()),
    };
    (name[..open].trim().to_owned(), level.to_owned())
}

fn name_spells(name: &str, id: &str) -> bool {
    let (name, _) = name_effort(name);
    let spelled = name.split_whitespace().collect::<Vec<_>>().join("-");
    !spelled.is_empty() && spelled.eq_ignore_ascii_case(&split_id(id).0)
}

fn enum_names(enum_id: &str, id: &str) -> bool {
    let Some(enum_id) = enum_id.strip_prefix("MODEL_GOOGLE_") else {
        return false;
    };
    !enum_id.is_empty() && enum_id.eq_ignore_ascii_case(&id.replace(['-', '.'], "_"))
}

struct Family<'a> {
    id: String,
    variants: Vec<&'a RawModel>,
    efforts: Vec<String>,
}

fn families(raw: &[RawModel]) -> Vec<Family<'_>> {
    let mut out: Vec<Family<'_>> = Vec::new();
    for model in raw {
        let (id, effort) = split_id(&model.id);
        let index = match out.iter().position(|family| family.id == id) {
            Some(index) => index,
            None => {
                out.push(Family {
                    id,
                    variants: Vec::new(),
                    efforts: Vec::new(),
                });
                out.len() - 1
            }
        };
        out[index].variants.push(model);
        out[index].efforts.push(effort);
    }
    for family in &mut out {
        let mut named = Vec::new();
        for model in &family.variants {
            let (_, effort) = name_effort(&model.name);
            if !effort.is_empty() && !named.contains(&effort) {
                named.push(effort);
            }
        }
        if named.len() == family.variants.len() {
            for (index, model) in family.variants.iter().enumerate() {
                family.efforts[index] = name_effort(&model.name).1;
            }
        }
    }
    out
}

fn by_effort(family: &Family<'_>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (model, effort) in family.variants.iter().zip(&family.efforts) {
        out.entry(effort.clone())
            .or_insert_with(|| model.id.clone());
    }
    if !out.contains_key("") {
        if let Some(high) = out.get("high") {
            out.insert(String::new(), high.clone());
        } else {
            for level in LEVELS.iter().rev() {
                if let Some(id) = out.get(*level) {
                    out.insert(String::new(), id.clone());
                    break;
                }
            }
        }
    }
    out
}

pub(crate) fn collapse(raw: &[RawModel]) -> Result<Vec<ModelSpec>, PluginError> {
    if raw.is_empty() {
        return FALLBACK
            .iter()
            .map(|(id, name)| {
                Ok(ModelSpec::new(*id, Some((*name).to_owned()))?
                    .with_image_input(Some(!id.starts_with("gpt-oss"))))
            })
            .collect();
    }
    let families = families(raw);
    let mut out = Vec::new();
    for family in &families {
        if family.variants.len() == 1 {
            out.push(family.variants[0].spec()?);
            continue;
        }
        let default = by_effort(family).remove("").unwrap_or_default();
        let Some(model) = family
            .variants
            .iter()
            .find(|model| model.id == default)
            .map(|model| (*model).clone())
        else {
            continue;
        };
        let mut spec = model.spec()?;
        let (mut name, _) = name_effort(spec.display_name().unwrap_or_default());
        if name.is_empty() || name == default {
            name = family.id.clone();
        }
        let context = family
            .variants
            .iter()
            .map(|model| model.context)
            .filter(|size| *size > 0)
            .min()
            .unwrap_or_default();
        let output = family
            .variants
            .iter()
            .map(|model| model.output)
            .filter(|size| *size > 0)
            .min()
            .unwrap_or_default();
        let images = family.variants.iter().all(|model| {
            model
                .images
                .unwrap_or_else(|| !model.id.starts_with("gpt-oss"))
        });
        let efforts = LEVELS
            .iter()
            .filter(|level| family.efforts.iter().any(|known| known == *level))
            .map(|level| (*level).to_owned())
            .collect();
        spec = ModelSpec::new(family.id.clone(), Some(name))?
            .with_context(context)
            .with_max_output(output)
            .with_image_input(Some(images))
            .with_efforts(efforts);
        out.push(spec);
    }

    let base: HashMap<String, ModelSpec> = out
        .iter()
        .map(|model| (model.id().to_owned(), model.clone()))
        .collect();
    for model in &mut out {
        let Some(family) = model.id().strip_suffix("-tiered").map(str::to_owned) else {
            continue;
        };
        let Some(model_base) = base.get(&family) else {
            continue;
        };
        if model.efforts().is_empty() {
            *model = model.clone().with_efforts(model_base.efforts().to_vec());
        }
        if model.display_name().is_some_and(|name| name == model.id())
            && model_base.display_name().is_some_and(|name| name != family)
        {
            let name = format!("{} (Tiered)", model_base.display_name().unwrap_or_default());
            *model = ModelSpec::new(model.id().to_owned(), Some(name))?
                .with_context(model.context())
                .with_max_output(model.max_output())
                .with_image_input(model.image_input())
                .with_efforts(model.efforts().to_vec());
        }
    }
    Ok(out)
}

pub(crate) fn merge(mut out: Vec<ModelSpec>, models: Vec<ModelSpec>) -> Vec<ModelSpec> {
    let mut next = 0;
    for model in models {
        if let Some(index) = out.iter().position(|known| known.id() == model.id()) {
            let known = out[index].clone();
            let context = least_positive(known.context(), model.context());
            let output = least_positive(known.max_output(), model.max_output());
            let images = match (known.image_input(), model.image_input()) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            out[index] = known
                .with_context(context)
                .with_max_output(output)
                .with_image_input(images);
            next = next.max(index + 1);
        } else {
            out.insert(next, model);
            next += 1;
        }
    }
    out
}

fn least_positive(left: u64, right: u64) -> u64 {
    match (left, right) {
        (0, right) => right,
        (left, 0) => left,
        (left, right) => left.min(right),
    }
}

pub(crate) fn sent_id(raw: &[RawModel], model: &str, effort: Option<&str>) -> String {
    let families = families(raw);
    let mut model = model;
    if let Some(family) = families.iter().find(|family| {
        family.variants.len() > 1
            && family
                .variants
                .iter()
                .any(|variant| variant.id == model && variant.id != family.id)
    }) {
        if effort.is_none() {
            return model.to_owned();
        }
        model = &family.id;
    }
    let Some(family) = families
        .iter()
        .find(|family| family.id == model && family.variants.len() > 1)
    else {
        return model.to_owned();
    };
    let variants = by_effort(family);
    let Some(effort) = effort else {
        return variants
            .get("")
            .cloned()
            .unwrap_or_else(|| model.to_owned());
    };
    let levels: Vec<&str> = LEVELS
        .iter()
        .copied()
        .filter(|level| variants.contains_key(*level))
        .collect();
    let nearest = nearest_effort(effort, &levels);
    variants
        .get(&nearest)
        .or_else(|| variants.get(""))
        .cloned()
        .unwrap_or_else(|| model.to_owned())
}

fn nearest_effort(wanted: &str, levels: &[&str]) -> String {
    let at = LEVELS.iter().position(|level| *level == wanted);
    if levels.is_empty() || at.is_none() || wanted.is_empty() {
        return wanted.to_owned();
    }
    let at = at.unwrap_or_default();
    if levels.contains(&wanted) {
        return wanted.to_owned();
    }
    let mut best = wanted;
    let mut distance = LEVELS.len();
    for level in levels {
        let Some(index) = LEVELS.iter().position(|known| known == level) else {
            continue;
        };
        if *level == "none" {
            continue;
        }
        let difference = index.abs_diff(at);
        if difference < distance || (difference == distance && index > at) {
            best = level;
            distance = difference;
        }
    }
    best.to_owned()
}

#[cfg(test)]
mod tests;
