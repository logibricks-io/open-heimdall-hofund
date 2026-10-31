//! The active-provider model layout: one key names the provider the agent
//! uses and each provider keeps its model in an entry of a providers map,
//! with an older layout of two flat keys.

use hofund_plugin_api::AgentActiveProvider;
use yaml_rust2::Yaml;

use crate::AgentError;
use crate::edit::{FileKind, get, string_at, yaml_key, yaml_root};

/// Every key a clear or a disconnect removes: the provider pick, both flat
/// keys, and hofund's entry. Other providers' entries stay.
pub(crate) fn clear_keys(layout: &AgentActiveProvider) -> [&'static str; 4] {
    [
        layout.key(),
        layout.legacy_provider(),
        layout.legacy_model(),
        layout.entry(),
    ]
}

/// The providers map holding hofund's entry.
fn providers_map(layout: &AgentActiveProvider) -> &'static str {
    layout
        .entry()
        .rsplit_once('.')
        .map_or(layout.entry(), |(map, _)| map)
}

/// Whether the file already uses the current layout: it has the provider key
/// or the providers map.
pub(crate) fn current_layout(
    layout: &AgentActiveProvider,
    kind: FileKind,
    text: &str,
) -> Result<bool, AgentError> {
    let keys = [layout.key(), providers_map(layout)];
    if kind == FileKind::Yaml {
        return Ok(match yaml_root(text)? {
            Yaml::Hash(hash) => keys.iter().any(|key| hash.contains_key(&yaml_key(key))),
            _ => false,
        });
    }
    for key in keys {
        if get(kind, text, key)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The model the agent uses when its provider is hofund.
pub(crate) fn configured_model(
    layout: &AgentActiveProvider,
    model_key: &str,
    kind: FileKind,
    text: &str,
) -> Result<Option<String>, AgentError> {
    let provider = string_at(kind, text, layout.key())?
        .filter(|provider| !provider.is_empty())
        .or(string_at(kind, text, layout.legacy_provider())?)
        .unwrap_or_default();
    if provider != layout.value() {
        return Ok(None);
    }
    Ok(string_at(kind, text, model_key)?
        .filter(|model| !model.is_empty())
        .or(string_at(kind, text, layout.legacy_model())?))
}
