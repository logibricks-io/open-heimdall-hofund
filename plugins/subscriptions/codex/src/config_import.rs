//! Read-only import of Codex `config.toml` model providers.
#![allow(dead_code, missing_docs)] // Row 2.17 kernel hook; integration tests call `read_providers`.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{PluginError, ProtocolId, SecretString};
use hofund_plugin_sdk::json::{Map, Value};

use crate::store::parse_toml;

/// One provider entry read from Codex config.
#[derive(Clone, Debug)]
pub struct ImportedCodexProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub protocol: ProtocolId,
    pub bearer: Option<SecretString>,
    pub models: Vec<String>,
    pub skip: Option<String>,
}

/// Reads providers from a Codex home `config.toml` without writing anything.
pub fn read_providers(home: &Path) -> Result<Vec<ImportedCodexProvider>, PluginError> {
    let path = home.join("config.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| PluginError::io("Codex config.toml is not readable"))?;
    parse_providers(&text, &path)
}

fn parse_providers(
    text: &str,
    config_path: &Path,
) -> Result<Vec<ImportedCodexProvider>, PluginError> {
    let (top, tables) = parse_toml(text);
    let mut ids = Vec::new();
    for key in tables.keys() {
        if let Some(id) = table_id(key, "model_providers.") {
            ids.push(id);
        }
    }
    ids.sort();
    ids.dedup();
    let mut out = Vec::new();
    for id in ids {
        let table_key = provider_table_key(&id);
        let Some(table) = tables.get(&table_key) else {
            continue;
        };
        let name = string_field(table, "name").unwrap_or_else(|| id.clone());
        let mut item = ImportedCodexProvider {
            id: id.clone(),
            name,
            base_url: String::new(),
            protocol: ProtocolId::OPENAI_RESPONSES,
            bearer: None,
            models: Vec::new(),
            skip: None,
        };
        if id == "hofund" {
            item.skip = Some("it points at hofund itself".to_owned());
            out.push(item);
            continue;
        }
        let mut models = Vec::new();
        if string_field(&top, "model_provider").as_deref() == Some(id.as_str()) {
            if let Some(model) = string_field(&top, "model") {
                models.push(model);
            }
            models.extend(import_models(
                config_path,
                string_field(&top, "model_catalog_json").as_deref(),
            ));
        }
        for (profile_key, profile) in &tables {
            if !profile_key.starts_with("profiles.") {
                continue;
            }
            let provider = string_field(profile, "model_provider")
                .or_else(|| string_field(&top, "model_provider"))
                .unwrap_or_default();
            if provider != id {
                continue;
            }
            if let Some(model) = string_field(profile, "model") {
                models.push(model);
            }
            let catalog = string_field(profile, "model_catalog_json")
                .or_else(|| string_field(&top, "model_catalog_json"));
            models.extend(import_models(config_path, catalog.as_deref()));
        }
        models.sort();
        models.dedup();
        item.models = models;
        let wire = string_field(table, "wire_api").unwrap_or_default();
        if wire == "chat" {
            item.protocol = ProtocolId::OPENAI_CHAT;
            item.base_url = string_field(table, "base_url").unwrap_or_default();
        } else {
            item.base_url = string_field(table, "base_url").unwrap_or_default();
        }
        if let Some(token) = string_field(table, "experimental_bearer_token")
            && !token.is_empty()
        {
            item.bearer = Some(SecretString::from(token));
        }
        if item.base_url.is_empty() {
            item.skip = Some("no base_url".to_owned());
        }
        out.push(item);
    }
    Ok(out)
}

fn table_id(table: &str, prefix: &str) -> Option<String> {
    let id = table.strip_prefix(prefix)?;
    if id.is_empty() {
        return None;
    }
    if id.starts_with('"') {
        let trimmed = id.trim_matches('"');
        return (!trimmed.is_empty()).then(|| trimmed.to_owned());
    }
    (!id.contains('.')).then(|| id.to_owned())
}

fn provider_table_key(id: &str) -> String {
    if id
        .chars()
        .any(|ch| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
    {
        format!("model_providers.\"{id}\"")
    } else {
        format!("model_providers.{id}")
    }
}

fn string_field(table: &Map<String, Value>, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Gateway-written model catalog file name under a Codex home (skip importing its models).
pub(crate) const GATEWAY_MODEL_CATALOG: &str = "hofund-models.json";

fn gateway_catalog_in_codex_home(home: &Path) -> PathBuf {
    home.join(GATEWAY_MODEL_CATALOG)
}

fn import_models(config_path: &Path, catalog_path: Option<&str>) -> Vec<String> {
    let Some(catalog_path) = catalog_path.filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"));
    let resolved = if let Some(rest) = catalog_path.strip_prefix("~/") {
        home.join(rest)
    } else if Path::new(catalog_path).is_absolute() {
        PathBuf::from(catalog_path)
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(catalog_path)
    };
    if resolved == gateway_catalog_in_codex_home(&home) {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(resolved) else {
        return Vec::new();
    };
    let Ok(root) = hofund_plugin_sdk::json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(models) = root.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for model in models {
        let Some(object) = model.as_object() else {
            continue;
        };
        let slug = object
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let visibility = object
            .get("visibility")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !slug.is_empty() && visibility != "hide" {
            out.push(slug.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r47b_config_import_reads_model_providers() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            r#"
model_provider = "relay"
model = "gpt-5"

[model_providers.relay]
name = "Relay"
base_url = "https://relay.test/v1"
wire_api = "responses"
experimental_bearer_token = "sk-import"

[model_providers.hofund]
name = "hofund"
"#,
        )
        .expect("config");
        let rows = read_providers(&home).expect("read");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "hofund");
        assert!(rows[0].skip.is_some());
        assert_eq!(rows[1].id, "relay");
        assert_eq!(rows[1].base_url, "https://relay.test/v1");
        assert!(rows[1].models.contains(&"gpt-5".to_owned()));
    }
}
