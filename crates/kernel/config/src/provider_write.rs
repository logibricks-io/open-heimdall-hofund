//! Format-preserving provider create, patch, and delete edits.

use toml_edit::{Array, DocumentMut, Formatted, InlineTable, Item, Table, Value, value};

use crate::{ConfigError, load, parse_fallback, provider_id_valid};

const FORBIDDEN_OPTION_KEYS: &[&str] = &["id", "preset", "api_key", "protocol", "base_url"];

/// Validates a provider `base_url` before it is stored.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when the URL is empty, malformed, or embeds credentials.
pub fn validate_provider_base_url(url: &str) -> Result<(), ConfigError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid(
            "base_url must not be empty".to_owned(),
        ));
    }
    if trimmed.contains('@') {
        if let Some(after_scheme) = trimmed.split("://").nth(1) {
            if after_scheme.contains('@') {
                return Err(ConfigError::Invalid(
                    "base_url must not embed credentials".to_owned(),
                ));
            }
        } else {
            return Err(ConfigError::Invalid(
                "base_url must not embed credentials".to_owned(),
            ));
        }
    }
    if !trimmed.contains("://") {
        return Err(ConfigError::Invalid(
            "base_url must include a scheme and host".to_owned(),
        ));
    }
    Ok(())
}

/// Creates one provider table from a preset-driven add request.
///
/// # Errors
///
/// Returns [`ConfigError`] when the document or ids are invalid.
#[allow(clippy::too_many_arguments)]
pub fn edit_provider_create(
    text: &str,
    id: &str,
    preset: &str,
    name: Option<&str>,
    base_url: Option<&str>,
    api_key: Option<&str>,
    family: Option<&str>,
    allowed_options: &[&str],
    options: &[(String, String)],
) -> Result<String, ConfigError> {
    if !provider_id_valid(id) {
        return Err(ConfigError::Invalid(format!(
            "provider id `{id}` is not valid kebab-case"
        )));
    }
    let config = load(text)?;
    if config.providers.iter().any(|provider| provider.id == id) {
        return Err(ConfigError::Invalid(format!(
            "provider `{id}` already exists"
        )));
    }
    for (key, _) in options {
        validate_option_key(key, allowed_options)?;
    }
    if let Some(url) = base_url.filter(|url| !url.trim().is_empty()) {
        validate_provider_base_url(url)?;
    }
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let providers = document
        .entry("providers")
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let array = providers
        .as_array_of_tables_mut()
        .ok_or_else(|| ConfigError::Syntax("providers is not an array".to_owned()))?;
    let mut table = Table::new();
    table.insert("id", value(id));
    table.insert("preset", value(preset));
    if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
        table.insert("name", value(name));
    }
    if let Some(url) = base_url.filter(|url| !url.trim().is_empty()) {
        table.insert("base_url", value(url));
    }
    if let Some(key) = api_key.filter(|key| !key.trim().is_empty()) {
        table.insert("api_key", value(key));
    }
    if let Some(family) = family.filter(|family| !family.trim().is_empty()) {
        table.insert("family", value(family));
    }
    insert_provider_options(&mut table, options);
    array.push(table);
    Ok(document.to_string())
}

/// Applies one provider patch while preserving unrelated fields.
pub fn edit_provider_patch(
    text: &str,
    provider_id: &str,
    enabled: Option<bool>,
    listed: Option<bool>,
    family: Option<Option<&str>>,
    base_url: Option<&str>,
    fallback: Option<&[String]>,
) -> Result<String, ConfigError> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let table = find_provider_table(&mut document, provider_id)?;
    if let Some(on) = enabled {
        if on {
            table.remove("enabled");
        } else {
            table.insert("enabled", value(false));
        }
    }
    if let Some(on) = listed {
        if on {
            table.remove("listed");
        } else {
            table.insert("listed", value(false));
        }
    }
    if let Some(family) = family {
        if let Some(tag) = family.filter(|tag| !tag.trim().is_empty()) {
            table.insert("family", value(tag));
        } else {
            table.remove("family");
        }
    }
    if let Some(url) = base_url {
        validate_provider_base_url(url)?;
        table.insert("base_url", value(url));
    }
    if let Some(entries) = fallback {
        for entry in entries {
            parse_fallback(provider_id, entry)?;
        }
        let mut array = Array::new();
        for entry in entries {
            array.push(entry.as_str());
        }
        table.insert("fallback", value(array));
    }
    Ok(document.to_string())
}

/// Removes one provider table and every config reference to it.
pub fn edit_provider_delete(text: &str, provider_id: &str) -> Result<String, ConfigError> {
    load(text)?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let providers = document
        .get_mut("providers")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| ConfigError::Invalid(format!("unknown provider `{provider_id}`")))?;
    let index = providers
        .iter()
        .position(|table| table.get("id").and_then(Item::as_str) == Some(provider_id))
        .ok_or_else(|| ConfigError::Invalid(format!("unknown provider `{provider_id}`")))?;
    providers.remove(index);
    strip_provider_references(&mut document, provider_id)?;
    let edited = document.to_string();
    load(&edited)?;
    Ok(edited)
}

fn strip_provider_references(
    document: &mut DocumentMut,
    provider_id: &str,
) -> Result<(), ConfigError> {
    loop {
        let mut changed = false;
        let groups = document
            .get("routes")
            .and_then(Item::as_array_of_tables)
            .map(group_ids_in_routes)
            .unwrap_or_default();
        if let Some(routes) = document
            .get_mut("routes")
            .and_then(Item::as_array_of_tables_mut)
        {
            let mut remove = Vec::new();
            for (index, route) in routes.iter_mut().enumerate() {
                if let Some(upstreams) = route.get_mut("upstreams").and_then(Item::as_array_mut) {
                    let before = upstreams.len();
                    upstreams.retain(|entry| {
                        !upstream_targets_provider(entry, provider_id)
                            && !upstream_targets_missing_group(entry, &groups)
                    });
                    if upstreams.len() != before {
                        changed = true;
                    }
                }
                let empty = route
                    .get("upstreams")
                    .and_then(Item::as_array)
                    .is_none_or(|array| array.is_empty());
                if empty {
                    remove.push(index);
                }
            }
            for index in remove.into_iter().rev() {
                routes.remove(index);
                changed = true;
            }
        }
        if let Some(providers) = document
            .get_mut("providers")
            .and_then(Item::as_array_of_tables_mut)
        {
            for table in providers.iter_mut() {
                if let Some(fallback) = table.get_mut("fallback").and_then(Item::as_array_mut) {
                    let before = fallback.len();
                    fallback.retain(|entry| {
                        let Some(value) = entry.as_str() else {
                            return true;
                        };
                        if fallback_targets_provider(value, provider_id) {
                            return false;
                        }
                        if let Some(id) = value.strip_prefix("group/") {
                            return groups.contains(id);
                        }
                        true
                    });
                    if fallback.len() != before {
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    normalize_invalid_routing(document)?;
    Ok(())
}

fn normalize_invalid_routing(document: &mut DocumentMut) -> Result<(), ConfigError> {
    let Some(routes) = document
        .get_mut("routes")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return Ok(());
    };
    for route in routes.iter_mut() {
        let routing = route
            .get("routing")
            .and_then(Item::as_str)
            .unwrap_or("smart");
        if routing != "cheap" {
            continue;
        }
        if route_is_cheap_group(route) {
            continue;
        }
        route.insert("routing", value("smart"));
    }
    Ok(())
}

fn route_is_cheap_group(route: &Table) -> bool {
    let Some(upstreams) = route.get("upstreams").and_then(Item::as_array) else {
        return false;
    };
    if upstreams.len() > 1 {
        return true;
    }
    upstreams.iter().any(|entry| {
        if let Value::InlineTable(table) = entry {
            table.contains_key("group") || table.contains_key("subscription")
        } else {
            false
        }
    })
}

fn group_ids_in_routes(routes: &toml_edit::ArrayOfTables) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for route in routes.iter() {
        if let Some(model) = route.get("model").and_then(Item::as_str)
            && let Some(id) = model.strip_prefix("group/")
        {
            out.insert(id.to_owned());
        }
    }
    out
}

fn upstream_targets_provider(entry: &Value, provider_id: &str) -> bool {
    if let Some(text) = entry.as_str() {
        if text == provider_id {
            return true;
        }
        if let Some((provider, _)) = text.split_once('/') {
            return provider == provider_id;
        }
        return false;
    }
    if let Value::InlineTable(table) = entry
        && table.get("provider").and_then(|value| value.as_str()) == Some(provider_id)
    {
        return true;
    }
    false
}

fn upstream_targets_missing_group(
    entry: &Value,
    groups: &std::collections::BTreeSet<String>,
) -> bool {
    if let Value::InlineTable(table) = entry
        && let Some(group) = table.get("group").and_then(|value| value.as_str())
    {
        return !groups.contains(group);
    }
    false
}

fn fallback_targets_provider(entry: &str, provider_id: &str) -> bool {
    if entry.starts_with("group/") {
        return false;
    }
    if let Some((provider, _)) = entry.split_once('/') {
        return provider == provider_id;
    }
    entry == provider_id
}

fn validate_option_key(key: &str, allowed: &[&str]) -> Result<(), ConfigError> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid("option key is empty".to_owned()));
    }
    if FORBIDDEN_OPTION_KEYS.contains(&trimmed) {
        return Err(ConfigError::Invalid(format!(
            "option `{trimmed}` cannot replace a provider field"
        )));
    }
    if !allowed.contains(&trimmed) {
        return Err(ConfigError::Invalid(format!(
            "option `{trimmed}` is not declared by the preset catalog"
        )));
    }
    Ok(())
}

fn insert_provider_options(table: &mut Table, options: &[(String, String)]) {
    if options.is_empty() {
        return;
    }
    let mut inline = InlineTable::new();
    for (key, val) in options {
        inline.insert(key.as_str(), Value::String(Formatted::new(val.clone())));
    }
    table.insert("options", value(Value::InlineTable(inline)));
}

fn find_provider_table<'a>(
    document: &'a mut DocumentMut,
    provider_id: &str,
) -> Result<&'a mut Table, ConfigError> {
    document
        .get_mut("providers")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|providers| {
            providers
                .iter_mut()
                .find(|table| table.get("id").and_then(Item::as_str) == Some(provider_id))
        })
        .ok_or_else(|| ConfigError::Invalid(format!("unknown provider `{provider_id}`")))
}

/// Validates fallback entries without editing.
pub fn validate_fallback_list(provider_id: &str, fallback: &[String]) -> Result<(), ConfigError> {
    for entry in fallback {
        parse_fallback(provider_id, entry)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::edit_provider_delete;
    use crate::load;

    const BASE: &str = "\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[providers]]\nid = \"added\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[routes]]\nmodel = \"on-added\"\nupstreams = [\"added\"]\n\
[[routes]]\nmodel = \"safe\"\nupstreams = [\"chat\"]\n";

    #[test]
    fn r7a_provider_delete_strips_routes_and_fallbacks_then_loads() {
        let text = format!(
            "{BASE}[[providers]]\nid = \"other\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\nfallback = [\"added/m\", \"chat/x\"]\n"
        );
        let edited = edit_provider_delete(&text, "added").expect("delete");
        assert!(!edited.contains("id = \"added\""));
        assert!(!edited.contains("on-added"));
        let config = load(&edited).expect("edited config loads");
        let other = config
            .providers
            .iter()
            .find(|provider| provider.id == "other")
            .expect("other provider");
        assert_eq!(other.fallback.len(), 1);
        assert_eq!(other.fallback[0].route_model(), "chat/x");
    }

    #[test]
    fn r7a_provider_delete_drops_group_fallback_when_group_removed() {
        let text = format!(
            "{BASE}[[providers]]\nid = \"solo\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
             [[providers]]\nid = \"other\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
             fallback = [\"group/team\"]\n\
             [[routes]]\nmodel = \"group/team\"\nupstreams = [\"solo\"]\n"
        );
        let edited = edit_provider_delete(&text, "solo").expect("delete");
        load(&edited).expect("edited config loads");
        assert!(!edited.contains("group/team"));
        let config = load(&edited).expect("reload");
        let other = config
            .providers
            .iter()
            .find(|provider| provider.id == "other")
            .expect("other");
        assert!(other.fallback.is_empty());
    }

    #[test]
    fn r7a_provider_delete_demotes_cheap_group_when_one_member_remains() {
        let text = "\
[[providers]]\nid = \"a\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[providers]]\nid = \"b\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[routes]]\nmodel = \"group/pair\"\nrouting = \"cheap\"\n\
upstreams = [{ provider = \"a\" }, { provider = \"b\" }]\n";
        let edited = edit_provider_delete(text, "b").expect("delete");
        load(&edited).expect("edited config loads");
        assert!(
            edited.contains("routing = \"smart\"") || edited.contains("routing = 'smart'"),
            "expected cheap routing demoted, got: {edited}"
        );
        let config = load(&edited).expect("reload");
        let group = config
            .routes
            .iter()
            .find(|route| route.model == "group/pair")
            .expect("group route");
        assert_eq!(group.routing, "smart");
    }
}
