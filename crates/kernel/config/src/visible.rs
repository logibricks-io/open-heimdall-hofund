//! Per-agent model visibility settings and catalog filtering.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::Deserialize;

use crate::{Config, ConfigError, Route, Upstream, UpstreamTarget};

const GROUP_PREFIX: &str = "group/";

/// One route model as visibility logic sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    /// Route id the client sends.
    pub id: String,
    /// Names a visibility allow-list may mention (family, provider id, group id, …).
    pub names: Vec<String>,
}

/// Per-agent visibility settings loaded from `[agents]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentModels {
    /// Agent id to visibility name lists.
    pub visible: BTreeMap<String, Vec<String>>,
    /// Agent id to hidden route model ids.
    pub hidden_models: BTreeMap<String, Vec<String>>,
    /// Agent id to picked route model ids when `picked_only` names the agent.
    pub picked_models: BTreeMap<String, Vec<String>>,
    /// Agents shown only models listed in `picked_models`.
    pub picked_only: BTreeSet<String>,
}

/// Published visibility state shared with the gateway.
#[derive(Clone)]
pub struct VisibleCatalog {
    /// Configuration snapshot.
    pub config: Arc<Config>,
    /// Metadata for each exposed route id.
    pub index: BTreeMap<String, CatalogEntry>,
    /// Agent visibility settings.
    pub settings: AgentModels,
}

impl VisibleCatalog {
    /// Builds a catalog for the given exposed route ids.
    #[must_use]
    pub fn build(config: Arc<Config>, route_ids: &[String]) -> Self {
        Self {
            config: Arc::clone(&config),
            index: build_catalog_index(&config, route_ids),
            settings: config.agents.clone(),
        }
    }

    /// Filters `route_ids` for `agent_id`, keeping `active_model` visible when set.
    #[must_use]
    pub fn models_for_agent(
        &self,
        route_ids: &[String],
        agent_id: Option<&str>,
        active_model: Option<&str>,
    ) -> (Vec<String>, Vec<String>) {
        catalog_for_agent(
            &self.settings,
            &self.index,
            route_ids,
            agent_id,
            active_model,
        )
    }
}

/// Builds entry metadata for each id in `route_ids`.
#[must_use]
pub fn build_catalog_index(
    config: &Config,
    route_ids: &[String],
) -> BTreeMap<String, CatalogEntry> {
    let mut out = BTreeMap::new();
    for id in route_ids {
        if let Some(entry) = entry_for_id(config, id) {
            out.insert(id.clone(), entry);
        }
    }
    out
}

/// Returns the route ids an agent is shown and those kept from it.
#[must_use]
pub fn catalog_for_agent(
    settings: &AgentModels,
    index: &BTreeMap<String, CatalogEntry>,
    route_ids: &[String],
    agent_id: Option<&str>,
    active_model: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let agent = agent_id.map(str::trim).filter(|id| !id.is_empty());
    let listed = listed_ids(settings, index, route_ids, agent);
    let off = model_off(settings, agent);
    let active = active_model.map(str::trim).filter(|id| !id.is_empty());
    let mut shown = Vec::new();
    let mut hidden = Vec::new();
    for id in listed {
        if off(&id) && active != Some(id.as_str()) {
            hidden.push(id);
        } else {
            shown.push(id);
        }
    }
    (shown, hidden)
}

fn listed_ids(
    settings: &AgentModels,
    index: &BTreeMap<String, CatalogEntry>,
    route_ids: &[String],
    agent: Option<&str>,
) -> Vec<String> {
    let Some(agent) = agent.map(normalize_agent_id) else {
        return route_ids
            .iter()
            .filter(|id| index.contains_key(id.as_str()))
            .cloned()
            .collect();
    };
    let names = settings.visible.get(&agent);
    route_ids
        .iter()
        .filter(|id| index.contains_key(id.as_str()))
        .filter(|id| {
            names.is_none_or(|names| shows(names, index.get(id.as_str()).expect("indexed")))
        })
        .cloned()
        .collect()
}

fn model_off<'a>(settings: &'a AgentModels, agent: Option<&str>) -> Box<dyn Fn(&str) -> bool + 'a> {
    let agent = agent.map(normalize_agent_id);
    Box::new(move |id: &str| {
        let Some(agent) = agent.as_deref() else {
            return false;
        };
        if settings.picked_only.contains(agent) {
            let picked = settings.picked_models.get(agent);
            return !picked.is_some_and(|ids| ids.iter().any(|entry| entry == id));
        }
        settings
            .hidden_models
            .get(agent)
            .is_some_and(|ids| ids.iter().any(|entry| entry == id))
    })
}

/// Reports whether `names` includes any of `entry`'s visibility names.
#[must_use]
pub fn shows(names: &[String], entry: &CatalogEntry) -> bool {
    names.iter().any(|name| {
        entry
            .names
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(name))
    })
}

fn entry_for_id(config: &Config, id: &str) -> Option<CatalogEntry> {
    let route = config.routes.iter().find(|route| route.model == id)?;
    Some(CatalogEntry {
        id: id.to_owned(),
        names: entry_names(config, route),
    })
}

fn entry_names(config: &Config, route: &Route) -> Vec<String> {
    if let Some(group) = route.model.strip_prefix(GROUP_PREFIX) {
        let mut names = Vec::new();
        if let Some(family) = route.family.as_deref().filter(|value| !value.is_empty()) {
            names.push(family.to_owned());
        }
        names.push(group.to_owned());
        names.push(format!("{GROUP_PREFIX}{group}"));
        if route.model != format!("{GROUP_PREFIX}{group}") {
            names.push(route.model.clone());
        }
        return names;
    }
    let mut names = Vec::new();
    if let Some(family) = route.family.as_deref().filter(|value| !value.is_empty()) {
        names.push(family.to_owned());
    } else if let Some(provider) = primary_provider(config, route) {
        if let Some(family) = provider.family.as_deref().filter(|value| !value.is_empty()) {
            names.push(family.to_owned());
        }
        names.push(provider.id.clone());
        return names;
    }
    if let Some(id) = primary_target_id(route) {
        names.push(id);
    }
    names
}

fn primary_provider<'a>(config: &'a Config, route: &Route) -> Option<&'a crate::Provider> {
    let id = route.upstreams.iter().find_map(Upstream::provider)?;
    config.providers.iter().find(|provider| provider.id == id)
}

fn primary_target_id(route: &Route) -> Option<String> {
    route
        .upstreams
        .first()
        .map(|upstream| match &upstream.target {
            UpstreamTarget::Provider(id) | UpstreamTarget::Subscription(id) => id.clone(),
            UpstreamTarget::Group(id) => format!("{GROUP_PREFIX}{id}"),
        })
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAgentModels {
    #[serde(default)]
    pub visible: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub hidden_models: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub picked_models: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub picked_only: Vec<String>,
}

pub(crate) fn agent_models_of(raw: RawAgentModels) -> Result<AgentModels, ConfigError> {
    let visible = normalize_agent_map(raw.visible, "agents.visible")?;
    let hidden_models = normalize_agent_map(raw.hidden_models, "agents.hidden_models")?;
    let picked_models = normalize_agent_map(raw.picked_models, "agents.picked_models")?;
    let mut picked_only = BTreeSet::new();
    for agent in raw.picked_only {
        let agent = normalize_agent_key(&agent, "agents.picked_only")?;
        picked_only.insert(agent);
    }
    Ok(AgentModels {
        visible,
        hidden_models,
        picked_models,
        picked_only,
    })
}

fn normalize_agent_map(
    input: BTreeMap<String, Vec<String>>,
    field: &str,
) -> Result<BTreeMap<String, Vec<String>>, ConfigError> {
    let mut out = BTreeMap::new();
    for (agent, values) in input {
        let agent = normalize_agent_key(&agent, field)?;
        let mut cleaned = Vec::new();
        for value in values {
            let value = value.trim();
            if value.is_empty() || cleaned.iter().any(|seen: &String| seen == value) {
                continue;
            }
            cleaned.push(value.to_owned());
        }
        if cleaned.is_empty() {
            continue;
        }
        out.insert(agent, cleaned);
    }
    Ok(out)
}

fn normalize_agent_key(agent: &str, field: &str) -> Result<String, ConfigError> {
    let agent = normalize_agent_id(agent);
    if agent.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "{field} has an empty agent id"
        )));
    }
    if !super::kebab(&agent) {
        return Err(ConfigError::Invalid(format!(
            "{field} agent `{agent}` is not kebab-case"
        )));
    }
    Ok(agent)
}

fn normalize_agent_id(agent: &str) -> String {
    agent.trim().to_ascii_lowercase()
}

/// Replaces or clears `[agents.visible]` for one agent.
///
/// # Errors
///
/// Returns [`ConfigError`] when the document is invalid.
pub fn edit_agent_visible(
    text: &str,
    agent: &str,
    names: &[String],
) -> Result<String, ConfigError> {
    edit_agent_list(text, "visible", agent, names)
}

/// Replaces or clears `[agents.hidden_models]` for one agent.
///
/// # Errors
///
/// Returns [`ConfigError`] when the document is invalid.
pub fn edit_agent_hidden_models(
    text: &str,
    agent: &str,
    ids: &[String],
) -> Result<String, ConfigError> {
    edit_agent_list(text, "hidden_models", agent, ids)
}

/// Replaces or clears `[agents.picked_models]` for one agent.
///
/// # Errors
///
/// Returns [`ConfigError`] when the document is invalid.
pub fn edit_agent_picked_models(
    text: &str,
    agent: &str,
    ids: &[String],
) -> Result<String, ConfigError> {
    edit_agent_list(text, "picked_models", agent, ids)
}

/// Adds or removes an agent from `[agents].picked_only`.
///
/// # Errors
///
/// Returns [`ConfigError`] when the document is invalid.
pub fn edit_agent_picked_only(text: &str, agent: &str, only: bool) -> Result<String, ConfigError> {
    use toml_edit::{DocumentMut, Item, Table};

    let agent = normalize_agent_key(agent, "agents.picked_only")?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let agents = document
        .entry("agents")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::Invalid("agents is not a table".to_owned()))?;
    let mut list = agents
        .get("picked_only")
        .and_then(Item::as_array)
        .cloned()
        .unwrap_or_default();
    list.retain(|value| value.as_str() != Some(agent.as_str()));
    if only {
        list.push(agent.as_str());
    }
    if list.is_empty() {
        agents.remove("picked_only");
    } else {
        agents.insert("picked_only", Item::Value(list.into()));
    }
    Ok(document.to_string())
}

fn edit_agent_list(
    text: &str,
    field: &str,
    agent: &str,
    values: &[String],
) -> Result<String, ConfigError> {
    use toml_edit::{Array, DocumentMut, Item, Table};

    let agent = normalize_agent_key(agent, &format!("agents.{field}"))?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let agents = document
        .entry("agents")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::Invalid("agents is not a table".to_owned()))?;
    let table = agents
        .entry(field)
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::Invalid(format!("agents.{field} is not a table")))?;
    if values.is_empty() {
        table.remove(&agent);
    } else {
        let mut array = Array::new();
        for entry in values {
            array.push(entry.as_str());
        }
        table.insert(&agent, Item::Value(array.into()));
    }
    if table.is_empty() {
        agents.remove(field);
    }
    Ok(document.to_string())
}

pub(crate) fn validated_family(
    kind: &str,
    id: &str,
    family: Option<&str>,
) -> Result<Option<String>, ConfigError> {
    let Some(family) = family.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if !super::kebab(family) {
        return Err(ConfigError::Invalid(format!(
            "{kind} `{id}` family `{family}` is not kebab-case"
        )));
    }
    Ok(Some(family.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{VisibleCatalog, build_catalog_index, catalog_for_agent, shows};
    use crate::load;
    use std::sync::Arc;

    fn sample_config() -> crate::Config {
        load(
            r#"
listen = "127.0.0.1:3425"
[[providers]]
id = "relay"
family = "relay"
protocol = "openai-chat"
base_url = "http://127.0.0.1:1"
api_key = "k"
[[providers]]
id = "other"
protocol = "openai-chat"
base_url = "http://127.0.0.1:2"
api_key = "k"
[[routes]]
model = "relay/a"
upstreams = ["relay"]
[[routes]]
model = "other/b"
upstreams = ["other"]
[[routes]]
model = "group/work"
family = "teams"
upstreams = [{ group = "inner" }]
[[routes]]
model = "group/inner"
upstreams = ["relay"]
[agents.visible]
codex = ["relay", "teams"]
[agents.hidden_models]
codex = ["other/b"]
"#,
        )
        .expect("config")
    }

    #[test]
    fn g2_1_build_catalog_index_collects_visibility_names() {
        let config = sample_config();
        let ids = vec![
            "relay/a".to_owned(),
            "other/b".to_owned(),
            "group/work".to_owned(),
        ];
        let index = build_catalog_index(&config, &ids);
        assert!(shows(
            &["relay".to_owned()],
            index.get("relay/a").expect("relay/a")
        ));
        assert!(shows(
            &["teams".to_owned()],
            index.get("group/work").expect("group/work")
        ));
        assert!(shows(
            &["work".to_owned()],
            index.get("group/work").expect("group/work")
        ));
    }

    #[test]
    fn g2_2_catalog_for_agent_honors_visible_and_hidden() {
        let config = sample_config();
        let ids = vec![
            "relay/a".to_owned(),
            "other/b".to_owned(),
            "group/work".to_owned(),
        ];
        let index = build_catalog_index(&config, &ids);
        let (shown, hidden) = catalog_for_agent(&config.agents, &index, &ids, Some("codex"), None);
        assert_eq!(shown, ["relay/a", "group/work"]);
        assert!(hidden.is_empty());
        let mut wide = config.clone();
        wide.agents.visible.clear();
        let (shown, hidden) = catalog_for_agent(&wide.agents, &index, &ids, Some("codex"), None);
        assert_eq!(shown, ["relay/a", "group/work"]);
        assert_eq!(hidden, ["other/b"]);
    }

    #[test]
    fn g2_3_active_model_is_never_hidden() {
        let mut config = sample_config();
        config.agents.visible.clear();
        let ids = vec!["other/b".to_owned()];
        let index = build_catalog_index(&config, &ids);
        let (shown, hidden) =
            catalog_for_agent(&config.agents, &index, &ids, Some("codex"), Some("other/b"));
        assert_eq!(shown, ["other/b"]);
        assert!(hidden.is_empty());
    }

    #[test]
    fn g2_4_picked_only_limits_to_picked_models() {
        let mut config = sample_config();
        config.agents.picked_only.insert("codex".to_owned());
        config.agents.picked_models.insert(
            "codex".to_owned(),
            vec!["relay/a".to_owned(), "missing".to_owned()],
        );
        config.agents.hidden_models.clear();
        let ids = vec![
            "relay/a".to_owned(),
            "other/b".to_owned(),
            "group/work".to_owned(),
        ];
        let index = build_catalog_index(&config, &ids);
        let (shown, hidden) = catalog_for_agent(&config.agents, &index, &ids, Some("codex"), None);
        assert_eq!(shown, ["relay/a"]);
        assert_eq!(hidden, ["group/work"]);
    }

    #[test]
    fn g2_5_visible_catalog_filters_route_lists() {
        let config = Arc::new(sample_config());
        let ids = vec![
            "relay/a".to_owned(),
            "other/b".to_owned(),
            "group/work".to_owned(),
        ];
        let catalog = VisibleCatalog::build(config, &ids);
        let (shown, _) = catalog.models_for_agent(&ids, Some("codex"), None);
        assert_eq!(shown, ["relay/a", "group/work"]);
    }

    #[test]
    fn g2_6_agent_models_validation_normalizes_agent_ids() {
        let err = load(
            r#"
listen = "127.0.0.1:3425"
[[providers]]
id = "p"
protocol = "openai-chat"
base_url = "http://127.0.0.1:1"
api_key = "k"
[[routes]]
model = "m"
upstreams = ["p"]
[agents.visible]
Codex = ["p"]
"#,
        )
        .expect("loads mixed case key");
        assert!(err.agents.visible.contains_key("codex"));
    }
}
