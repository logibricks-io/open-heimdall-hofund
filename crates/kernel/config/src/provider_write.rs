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

/// Removes one provider table.
pub fn edit_provider_delete(text: &str, provider_id: &str) -> Result<String, ConfigError> {
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
    Ok(document.to_string())
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
