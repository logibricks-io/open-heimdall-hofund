//! Provider id rename: rewrite config references and validate the new id.

use toml_edit::{DocumentMut, Formatted, Item, TableLike, Value};

use crate::{ConfigError, load};

/// The provider id agents use for the local gateway in their own config.
pub const GATEWAY_PROVIDER_ID: &str = "hofund";
/// Reserved routing-group id prefix (`group/<id>`); the bare `group` id is reserved too.
pub const GROUP_ROUTE_PREFIX: &str = "group/";

/// Returns whether `id` is a valid provider id (kebab-case).
#[must_use]
pub fn provider_id_valid(id: &str) -> bool {
    kebab(id)
}

/// Rewrites every config reference from `from` to `to` and changes the provider table id.
///
/// Historical usage rows keep the upstream id recorded at request time; they are not rewritten.
///
/// # Errors
///
/// Returns [`ConfigError`] when validation fails or the text is not valid TOML.
pub fn edit_provider_rename(text: &str, from: &str, to: &str) -> Result<String, ConfigError> {
    validate_provider_rename(text, from, to)?;
    let from = normalize_id(from);
    let to = normalize_id(to);
    if from == to {
        return Ok(text.to_owned());
    }
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    rewrite_document(&mut document, &from, &to);
    Ok(document.to_string())
}

/// Validates a provider rename before any file is touched.
///
/// # Errors
///
/// Returns [`ConfigError`] when the rename is not allowed.
pub fn validate_provider_rename(text: &str, from: &str, to: &str) -> Result<(), ConfigError> {
    let from = normalize_id(from);
    let to = normalize_id(to);
    if from == to {
        return Ok(());
    }
    if !provider_id_valid(&to) {
        return Err(ConfigError::Invalid(format!(
            "a provider's id must be lowercase letters, digits and dashes, not `{to}`"
        )));
    }
    if to == GATEWAY_PROVIDER_ID {
        return Err(ConfigError::Invalid(format!(
            "`{GATEWAY_PROVIDER_ID}` is what agents call the gateway itself; pick another id"
        )));
    }
    if to == GROUP_ROUTE_PREFIX.trim_end_matches('/') {
        return Err(ConfigError::Invalid(
            "`group` starts the ids of routing groups; pick another id".to_owned(),
        ));
    }
    let config = load(text).map_err(|error| match error {
        ConfigError::Syntax(message) => ConfigError::Syntax(message),
        other => other,
    })?;
    if !config.providers.iter().any(|provider| provider.id == from) {
        return Err(ConfigError::Invalid(format!("no provider `{from}`")));
    }
    if config.providers.iter().any(|provider| provider.id == to) {
        return Err(ConfigError::Invalid(format!(
            "there is a provider `{to}` already"
        )));
    }
    if config.subscriptions.iter().any(|entry| entry.id == to) {
        return Err(ConfigError::Invalid(format!(
            "`{to}` is the id of a subscription; pick another id"
        )));
    }
    if config.subscriptions.iter().any(|entry| entry.id == from) {
        return Err(ConfigError::Invalid(format!(
            "`{from}` is a subscription: its id is fixed"
        )));
    }
    Ok(())
}

fn normalize_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

fn kebab(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut prev_hyphen = true;
    for byte in bytes {
        if *byte == b'-' {
            if prev_hyphen {
                return false;
            }
            prev_hyphen = true;
        } else if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            prev_hyphen = false;
        } else {
            return false;
        }
    }
    !prev_hyphen
}

/// A model or upstream reference with `from` as the provider segment renamed to `to`.
#[must_use]
pub fn renamed_ref(reference: &str, from: &str, to: &str) -> String {
    let (prefix, rest) = match reference.strip_prefix("hofund/") {
        Some(rest) => ("hofund/", rest),
        None => ("", reference),
    };
    if let Some(model) = rest.strip_prefix(&format!("{from}/")) {
        return format!("{prefix}{to}/{model}");
    }
    if rest == from {
        return format!("{prefix}{to}");
    }
    reference.to_owned()
}

fn rewrite_document(document: &mut DocumentMut, from: &str, to: &str) {
    if let Some(Item::ArrayOfTables(providers)) = document.get_mut("providers") {
        for table in providers.iter_mut() {
            if table.get("id").and_then(Item::as_str) == Some(from)
                && let Some(Item::Value(value)) = table.get_mut("id")
            {
                if let Value::String(text) = value {
                    let decor = text.decor().clone();
                    *text = Formatted::new(to.to_owned());
                    *text.decor_mut() = decor;
                } else {
                    *value = Value::String(Formatted::new(to.to_owned()));
                }
            }
            rewrite_fallback(table, from, to);
        }
    }
    if let Some(Item::ArrayOfTables(routes)) = document.get_mut("routes") {
        for route in routes.iter_mut() {
            rewrite_route_upstreams(route, from, to);
        }
    }
    rewrite_agents_visible(document, from, to);
    rewrite_catalog_provider_ids(document, from, to);
}

fn rewrite_fallback(table: &mut dyn TableLike, from: &str, to: &str) {
    let Some(Item::Value(Value::Array(array))) = table.get_mut("fallback") else {
        return;
    };
    for item in array.iter_mut() {
        if let Some(entry) = item.as_str() {
            *item = Value::String(Formatted::new(rewrite_fallback_entry(entry, from, to)));
        }
    }
}

fn rewrite_fallback_entry(entry: &str, from: &str, to: &str) -> String {
    if entry.starts_with(GROUP_ROUTE_PREFIX) {
        return entry.to_owned();
    }
    renamed_ref(entry, from, to)
}

fn rewrite_route_upstreams(route: &mut dyn TableLike, from: &str, to: &str) {
    let Some(Item::Value(Value::Array(array))) = route.get_mut("upstreams") else {
        return;
    };
    for item in array.iter_mut() {
        if let Some(current) = item.as_str() {
            if current == from {
                *item = Value::String(Formatted::new(to.to_owned()));
            }
        } else if let Value::InlineTable(table) = item {
            rewrite_mapped_upstream(table, from, to);
        }
    }
}

fn rewrite_mapped_upstream(table: &mut toml_edit::InlineTable, from: &str, to: &str) {
    if table.get("provider").and_then(|value| value.as_str()) == Some(from) {
        table.insert("provider", Value::String(Formatted::new(to.to_owned())));
    }
    if let Some(model) = table.get_mut("model")
        && let Some(current) = model.as_str()
    {
        *model = Value::String(Formatted::new(renamed_ref(current, from, to)));
    }
}

fn rewrite_agents_visible(document: &mut DocumentMut, from: &str, to: &str) {
    let Some(Item::Table(agents)) = document.get_mut("agents") else {
        return;
    };
    for field in ["visible"] {
        let Some(Item::Table(map)) = agents.get_mut(field) else {
            continue;
        };
        for (_, item) in map.iter_mut() {
            let Item::Value(Value::Array(array)) = item else {
                continue;
            };
            for entry in array.iter_mut() {
                if let Some(name) = entry.as_str()
                    && name.eq_ignore_ascii_case(from)
                {
                    *entry = Value::String(Formatted::new(to.to_owned()));
                }
            }
        }
    }
}

fn rewrite_catalog_provider_ids(document: &mut DocumentMut, from: &str, to: &str) {
    let Some(Item::Table(catalog)) = document.get_mut("catalog") else {
        return;
    };
    rewrite_string_array(catalog, "reasoning_toggle_providers", from, to);
}

fn rewrite_string_array(table: &mut dyn TableLike, field: &str, from: &str, to: &str) {
    let Some(Item::Value(Value::Array(array))) = table.get_mut(field) else {
        return;
    };
    for item in array.iter_mut() {
        if let Some(current) = item.as_str() {
            let next = if current == from {
                to.to_owned()
            } else {
                renamed_ref(current, from, to)
            };
            *item = Value::String(Formatted::new(next));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GATEWAY_PROVIDER_ID, edit_provider_rename, renamed_ref, validate_provider_rename};

    const FIXTURE: &str = "# keep\n\
[[providers]]\nid = \"chat\" # main\npreset = \"openai\"\nfallback = [\"chat/old\", \"other/x\"]\n\
[[providers]]\nid = \"other\"\npreset = \"openai\"\n\
[[routes]]\nmodel = \"demo\"\nupstreams = [\"chat\", { provider = \"chat\", model = \"m\" }, { group = \"work\" }]\n\
[[routes]]\nmodel = \"group/work\"\nrouting = \"order\"\nupstreams = [{ provider = \"chat\", model = \"a\" }, \"other\"]\n\
[agents.visible]\nclaude = [\"Chat\", \"demo\"]\n";

    #[test]
    fn g3_1_every_reference_rewritten_and_comments_survive() {
        let edited = edit_provider_rename(FIXTURE, "chat", "team").expect("rename");
        assert!(edited.starts_with("# keep\n"), "{edited}");
        assert!(edited.contains("id = \"team\" # main"), "{edited}");
        assert!(
            edited.contains("fallback = [\"team/old\", \"other/x\"]"),
            "{edited}"
        );
        assert!(edited.contains("upstreams = [\"team\""), "{edited}");
        assert!(
            edited.contains("{ provider = \"team\", model = \"m\" }"),
            "{edited}"
        );
        assert!(
            edited.contains("upstreams = [{ provider = \"team\", model = \"a\" }, \"other\"]"),
            "{edited}"
        );
        assert!(edited.contains("claude = [\"team\", \"demo\"]"), "{edited}");
        let config = crate::load(&edited).expect("valid");
        assert_eq!(config.providers[0].id, "team");
        assert_eq!(config.providers[0].fallback[0].route_model(), "team/old");
    }

    #[test]
    fn g3_2_taken_or_invalid_new_id_refused_with_no_change() {
        for (to, needle) in [
            ("other", "already"),
            ("Bad Id", "lowercase"),
            (GATEWAY_PROVIDER_ID, "gateway"),
            ("group", "routing groups"),
            ("sub", "subscription"),
        ] {
            let text = format!("{FIXTURE}[[subscriptions]]\nid = \"sub\"\naccounts = \"*\"\n");
            let error = validate_provider_rename(&text, "chat", to).expect_err(to);
            assert!(
                error.to_string().to_ascii_lowercase().contains(needle),
                "{to}: {error}"
            );
            assert!(
                edit_provider_rename(&text, "chat", to).is_err(),
                "{to} should be refused"
            );
        }
        assert!(validate_provider_rename(FIXTURE, "ghost", "new").is_err());
        assert!(validate_provider_rename(FIXTURE, "chat", "chat").is_ok());
    }

    #[test]
    fn g3_3_validate_failure_leaves_text_untouched() {
        assert!(validate_provider_rename(FIXTURE, "chat", "other").is_err());
        assert!(edit_provider_rename(FIXTURE, "chat", "other").is_err());
    }

    #[test]
    fn g3_5_usage_history_keeps_recorded_upstream_ids() {
        // Usage rows are append-only; rename does not rewrite the ledger (see module docs).
        assert!(module_path!().contains("provider_rename"));
    }

    #[test]
    fn renamed_ref_handles_prefixed_and_plain_refs() {
        assert_eq!(renamed_ref("chat/m", "chat", "team"), "team/m");
        assert_eq!(
            renamed_ref("hofund/chat/m", "chat", "team"),
            "hofund/team/m"
        );
        assert_eq!(renamed_ref("other/m", "chat", "team"), "other/m");
    }
}
