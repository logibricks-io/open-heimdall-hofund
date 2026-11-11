//! Focused edits for settings exposed by the control plane.

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::ConfigError;

/// Fields changed by one settings patch. `None` leaves a field untouched.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsEdit {
    /// Model listen address.
    pub listen: Option<String>,
    /// Usage display currency.
    pub currency: Option<String>,
    /// Quota bar direction.
    pub quota_left: Option<bool>,
    /// Agent account discovery switch.
    pub discover_accounts: Option<bool>,
    /// Stored LAN allow-list.
    pub lan_allow: Option<Vec<String>>,
}

/// Applies a settings patch without disturbing unrelated TOML text.
///
/// # Errors
///
/// Returns [`ConfigError`] when the document or an edited table has the wrong shape.
pub fn edit_settings(text: &str, edit: &SettingsEdit) -> Result<String, ConfigError> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    if let Some(listen) = &edit.listen {
        document["listen"] = Item::Value(Value::from(listen.as_str()));
    }
    if edit.quota_left.is_some() || edit.currency.is_some() {
        let usage = table(document.entry("usage").or_insert(Item::None), "usage")?;
        if let Some(currency) = &edit.currency {
            usage.insert("currency", Item::Value(Value::from(currency.as_str())));
        }
        if let Some(quota_left) = edit.quota_left {
            usage.insert("quota_left", Item::Value(Value::from(quota_left)));
        }
    }
    if let Some(discover) = edit.discover_accounts {
        document["discover_agent_accounts"] = Item::Value(Value::from(discover));
        if let Some(subscriptions) = document
            .get_mut("subscriptions")
            .and_then(Item::as_array_of_tables_mut)
        {
            for subscription in subscriptions.iter_mut() {
                if let Some(options) = subscription.get_mut("options") {
                    if let Some(options) = options.as_table_like_mut() {
                        options.remove("discover");
                    }
                }
            }
        }
    }
    if let Some(lan_allow) = &edit.lan_allow {
        let mut values = Array::new();
        for entry in lan_allow {
            values.push(entry.as_str());
        }
        document["lan_allow"] = Item::Value(values.into());
    }
    Ok(document.to_string())
}

fn table<'a>(item: &'a mut Item, name: &str) -> Result<&'a mut Table, ConfigError> {
    item.or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| ConfigError::Invalid(format!("{name} is not a table")))
}

#[cfg(test)]
mod tests {
    use super::{SettingsEdit, edit_settings};

    #[test]
    fn settings_edit_preserves_unrelated_text_and_replaces_discovery_flags() {
        let original = r#"# keep this comment
listen = "127.0.0.1:3425"
[[subscriptions]]
id = "one"
accounts = "*"
[subscriptions.options]
discover = true
other = "kept"
[[subscriptions]]
id = "two"
accounts = "*"
[subscriptions.options]
discover = false
"#;
        let edited = edit_settings(
            original,
            &SettingsEdit {
                listen: Some("127.0.0.1:4000".to_owned()),
                currency: Some("cny".to_owned()),
                quota_left: Some(true),
                discover_accounts: Some(false),
                lan_allow: Some(vec!["192.168.1.0/24".to_owned(), "localhost".to_owned()]),
            },
        )
        .expect("edit");
        assert!(edited.contains("# keep this comment"));
        assert!(edited.contains("other = \"kept\""));
        assert!(!edited.contains("discover ="));
        let config = crate::load(&edited).expect("load");
        assert_eq!(config.listen.to_string(), "127.0.0.1:4000");
        assert_eq!(config.usage.currency.as_str(), "cny");
        assert!(config.usage.quota_left);
        assert!(!config.discover_agent_accounts);
        assert_eq!(
            config.lan_allow,
            ["192.168.1.0/24".to_owned(), "localhost".to_owned()]
        );
    }

    #[test]
    fn discovery_can_be_enabled_without_subscriptions() {
        let edited = edit_settings(
            "listen = \"127.0.0.1:3425\"\n",
            &SettingsEdit {
                discover_accounts: Some(true),
                ..SettingsEdit::default()
            },
        )
        .expect("edit");
        assert!(crate::load(&edited).expect("load").discover_agent_accounts);
    }
}
