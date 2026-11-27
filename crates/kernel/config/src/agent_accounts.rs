//! Accounts by agent: one agent uses one account or key of a provider.

use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, Value};

use crate::ConfigError;

/// An account id or key fingerprint, and the name an operator may type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountName {
    /// Stable id or key fingerprint.
    pub id: String,
    /// Display name. Empty when the account has none.
    pub name: String,
}

/// Keeps one pin per agent. An empty reference drops that agent. An empty
/// agent id, or two entries that differ only by case, is a configuration error.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] for an empty agent id or a repeated agent.
pub fn normalize_agent_accounts(
    owner: &str,
    raw: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut out = BTreeMap::new();
    for (agent, reference) in raw {
        let agent = agent.trim().to_lowercase();
        if agent.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "`{owner}` accounts by agent need an agent id"
            )));
        }
        let reference = reference.trim().to_owned();
        if reference.is_empty() {
            continue;
        }
        if out.insert(agent.clone(), reference).is_some() {
            return Err(ConfigError::Invalid(format!(
                "`{owner}` accounts by agent list `{agent}` twice"
            )));
        }
    }
    Ok(out)
}

/// Sets or clears one agent's account on a provider or subscription.
///
/// An empty `reference` removes the pin. A non-empty reference must be an id
/// or the unique display name of an entry in `known`. The stored value is the
/// id. Unrelated TOML is preserved.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when the agent id is empty, the owner is
/// absent, or the reference is not one of `known`. Returns [`ConfigError::Syntax`]
/// when `text` is not TOML.
pub fn edit_agent_account(
    text: &str,
    owner_id: &str,
    agent: &str,
    reference: &str,
    known: &[AccountName],
) -> Result<String, ConfigError> {
    let agent = agent.trim().to_lowercase();
    if agent.is_empty() {
        return Err(ConfigError::Invalid(
            "accounts by agent need an agent id".to_owned(),
        ));
    }
    let stored = if reference.trim().is_empty() {
        None
    } else {
        Some(resolve_account(owner_id, reference, known)?)
    };
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let table = owner_table(&mut document, owner_id)?;
    let mut accounts = read_accounts(table.get("agent_accounts"));
    match stored {
        Some(id) => {
            accounts.insert(agent, id);
        }
        None => {
            accounts.remove(&agent);
        }
    }
    write_accounts(table, &accounts);
    Ok(document.to_string())
}

/// Points every pin that stored `from` at `to` on `owner_id`.
///
/// A missing table is left unchanged. Matching is case-insensitive.
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when the owner is absent, or
/// [`ConfigError::Syntax`] when `text` is not TOML.
pub fn move_agent_account(
    text: &str,
    owner_id: &str,
    from: &str,
    to: &str,
) -> Result<String, ConfigError> {
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() || to.is_empty() || from.eq_ignore_ascii_case(to) {
        return Ok(text.to_owned());
    }
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let table = owner_table(&mut document, owner_id)?;
    let mut accounts = read_accounts(table.get("agent_accounts"));
    if accounts.is_empty() {
        return Ok(text.to_owned());
    }
    let mut changed = false;
    for reference in accounts.values_mut() {
        if reference.eq_ignore_ascii_case(from) {
            *reference = to.to_owned();
            changed = true;
        }
    }
    if !changed {
        return Ok(text.to_owned());
    }
    write_accounts(table, &accounts);
    Ok(document.to_string())
}

fn resolve_account(
    owner: &str,
    reference: &str,
    known: &[AccountName],
) -> Result<String, ConfigError> {
    let reference = reference.trim();
    if let Some(found) = known
        .iter()
        .find(|item| item.id.eq_ignore_ascii_case(reference))
    {
        return Ok(found.id.clone());
    }
    let names = known
        .iter()
        .filter(|item| !item.name.trim().is_empty() && item.name.eq_ignore_ascii_case(reference))
        .collect::<Vec<_>>();
    if names.len() == 1 {
        return Ok(names[0].id.clone());
    }
    Err(ConfigError::Invalid(format!(
        "`{owner}` has no account or key `{reference}`"
    )))
}

fn position(document: &DocumentMut, kind: &str, owner_id: &str) -> Option<usize> {
    document
        .get(kind)
        .and_then(Item::as_array_of_tables)
        .and_then(|entries| {
            entries
                .iter()
                .position(|table| table.get("id").and_then(Item::as_str) == Some(owner_id))
        })
}

fn table_at<'a>(
    document: &'a mut DocumentMut,
    kind: &str,
    index: usize,
) -> &'a mut toml_edit::Table {
    document
        .get_mut(kind)
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|entries| entries.get_mut(index))
        .expect("owner index")
}

fn owner_table<'a>(
    document: &'a mut DocumentMut,
    owner_id: &str,
) -> Result<&'a mut toml_edit::Table, ConfigError> {
    if let Some(index) = position(document, "providers", owner_id) {
        return Ok(table_at(document, "providers", index));
    }
    if let Some(index) = position(document, "subscriptions", owner_id) {
        return Ok(table_at(document, "subscriptions", index));
    }
    Err(ConfigError::Invalid(format!(
        "unknown provider or subscription `{owner_id}`"
    )))
}

fn read_accounts(item: Option<&Item>) -> BTreeMap<String, String> {
    let Some(item) = item else {
        return BTreeMap::new();
    };
    let pairs: Vec<(String, String)> = if let Some(table) = item.as_table() {
        table
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|text| (key.to_owned(), text.to_owned())))
            .collect()
    } else if let Some(table) = item.as_inline_table() {
        table
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|text| (key.to_owned(), text.to_owned())))
            .collect()
    } else {
        Vec::new()
    };
    let mut accounts = BTreeMap::new();
    for (agent, reference) in pairs {
        let agent = agent.trim().to_lowercase();
        let reference = reference.trim().to_owned();
        if agent.is_empty() || reference.is_empty() {
            continue;
        }
        accounts.insert(agent, reference);
    }
    accounts
}

fn write_accounts(table: &mut toml_edit::Table, accounts: &BTreeMap<String, String>) {
    if accounts.is_empty() {
        table.remove("agent_accounts");
        return;
    }
    let mut inline = toml_edit::InlineTable::new();
    for (agent, reference) in accounts {
        inline.insert(agent, Value::from(reference.as_str()));
    }
    table.insert("agent_accounts", Item::Value(Value::InlineTable(inline)));
}
