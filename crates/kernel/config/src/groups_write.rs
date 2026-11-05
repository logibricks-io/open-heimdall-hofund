//! Format-preserving routing group edits on `[[routes]]` tables.

use toml_edit::{Array, DocumentMut, Formatted, InlineTable, Item, Table, Value, value};

use crate::{ConfigError, ROUTING_NAMES, Stays, load, parse_rule_line, provider_id_valid};

/// Derives a group id from a display name.
#[must_use]
pub fn group_id_from_name(name: &str) -> String {
    let mut id = String::new();
    let mut prev_hyphen = false;
    for ch in name.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch);
            prev_hyphen = false;
        } else if !prev_hyphen && !id.is_empty() {
            id.push('-');
            prev_hyphen = true;
        }
    }
    while id.ends_with('-') {
        id.pop();
    }
    if id.is_empty() {
        "group".to_owned()
    } else {
        id
    }
}

/// Validates every rule line.
pub fn validate_group_rules(lines: &[String]) -> Result<(), ConfigError> {
    for line in lines {
        parse_rule_line(line)?;
    }
    Ok(())
}

fn find_group_route<'a>(
    document: &'a mut DocumentMut,
    group_id: &str,
) -> Result<&'a mut Table, ConfigError> {
    let model = format!("group/{group_id}");
    document
        .get_mut("routes")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|routes| {
            routes
                .iter_mut()
                .find(|table| table.get("model").and_then(Item::as_str) == Some(model.as_str()))
        })
        .ok_or_else(|| ConfigError::Invalid(format!("unknown group `{group_id}`")))
}

/// Creates a routing group with one member.
pub fn edit_group_create(
    text: &str,
    name: &str,
    first_member: &str,
) -> Result<(String, String), ConfigError> {
    let id = group_id_from_name(name);
    if !provider_id_valid(&id) {
        return Err(ConfigError::Invalid(format!(
            "group name `{name}` does not yield a valid id"
        )));
    }
    let config = load(text)?;
    let model = format!("group/{id}");
    if config.routes.iter().any(|route| route.model == model) {
        return Err(ConfigError::Invalid(format!("group `{id}` already exists")));
    }
    validate_member_ref(&config, first_member)?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let routes = document
        .entry("routes")
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let array = routes
        .as_array_of_tables_mut()
        .ok_or_else(|| ConfigError::Syntax("routes is not an array".to_owned()))?;
    let mut table = Table::new();
    table.insert("model", value(model.as_str()));
    table.insert("name", value(name.trim()));
    table.insert("routing", value("smart"));
    table.insert("upstreams", member_array(first_member));
    array.push(table);
    Ok((document.to_string(), id))
}

/// Applies a partial group patch.
#[allow(clippy::too_many_arguments)]
pub fn edit_group_patch(
    text: &str,
    group_id: &str,
    routing: Option<&str>,
    stays: Option<Stays>,
    family: Option<Option<&str>>,
    context: Option<Option<i64>>,
    levels: Option<&[String]>,
    classifier: Option<Option<&str>>,
    members: Option<&[(String, String, bool, bool)]>,
    rules: Option<&[String]>,
    rename_to: Option<&str>,
) -> Result<String, ConfigError> {
    if let Some(lines) = rules {
        validate_group_rules(lines)?;
    }
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let table = find_group_route(&mut document, group_id)?;
    if let Some(name) = routing {
        if !ROUTING_NAMES.contains(&name) {
            return Err(ConfigError::Invalid(format!(
                "unknown routing `{name}`; expected one of {}",
                ROUTING_NAMES.join(", ")
            )));
        }
        table.insert("routing", value(name));
    }
    if let Some(stays) = stays {
        table.insert("stays", value(stays.as_str()));
    }
    if let Some(family) = family {
        if let Some(tag) = family.filter(|tag| !tag.trim().is_empty()) {
            table.insert("family", value(tag));
        } else {
            table.remove("family");
        }
    }
    if let Some(context) = context {
        if let Some(tokens) = context {
            table.insert("context", value(tokens));
        } else {
            table.remove("context");
        }
    }
    if let Some(levels) = levels {
        let mut array = Array::new();
        for level in levels {
            array.push(level.as_str());
        }
        if array.is_empty() {
            table.remove("levels");
        } else {
            table.insert("levels", value(array));
        }
    }
    if let Some(classifier) = classifier {
        if let Some(reference) = classifier.filter(|reference| !reference.trim().is_empty()) {
            if reference == format!("group/{group_id}") {
                return Err(ConfigError::Invalid(
                    "a group cannot classify with itself".to_owned(),
                ));
            }
            table.insert("classifier", value(reference));
        } else {
            table.remove("classifier");
        }
    }
    if let Some(entries) = members {
        if entries.is_empty() {
            return Err(ConfigError::Invalid(
                "a group must keep at least one member".to_owned(),
            ));
        }
        let mut array = Array::new();
        for (reference, effort, fast, enabled) in entries {
            array.push(member_value(reference, effort, *fast, *enabled));
        }
        table.insert("upstreams", value(array));
    }
    if let Some(lines) = rules {
        let mut array = Array::new();
        for line in lines {
            array.push(line.as_str());
        }
        if array.is_empty() {
            table.remove("rules");
        } else {
            table.insert("rules", value(array));
        }
    }
    if let Some(new_id) = rename_to {
        let new_id = new_id.trim();
        if !provider_id_valid(new_id) {
            return Err(ConfigError::Invalid(format!(
                "group id `{new_id}` is not valid kebab-case"
            )));
        }
        let new_model = format!("group/{new_id}");
        if let Some(model) = table.get_mut("model") {
            *model = value(new_model.as_str());
        }
    }
    Ok(document.to_string())
}

/// Deletes a group route.
pub fn edit_group_delete(text: &str, group_id: &str) -> Result<String, ConfigError> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let model = format!("group/{group_id}");
    let routes = document
        .get_mut("routes")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| ConfigError::Invalid(format!("unknown group `{group_id}`")))?;
    let index = routes
        .iter()
        .position(|table| table.get("model").and_then(Item::as_str) == Some(model.as_str()))
        .ok_or_else(|| ConfigError::Invalid(format!("unknown group `{group_id}`")))?;
    routes.remove(index);
    Ok(document.to_string())
}

/// Clears `hidden` on a found group route.
pub fn edit_group_restore(text: &str, group_id: &str) -> Result<String, ConfigError> {
    let config = load(text)?;
    let model = format!("group/{group_id}");
    let route = config
        .routes
        .iter()
        .find(|route| route.model == model)
        .ok_or_else(|| ConfigError::Invalid(format!("unknown group `{group_id}`")))?;
    if !route.hidden {
        return Err(ConfigError::Invalid(format!(
            "group `{group_id}` is not hidden"
        )));
    }
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Syntax(error.to_string()))?;
    let table = find_group_route(&mut document, group_id)?;
    table.remove("hidden");
    Ok(document.to_string())
}

fn validate_member_ref(config: &crate::Config, reference: &str) -> Result<(), ConfigError> {
    if reference.trim().is_empty() {
        return Err(ConfigError::Invalid("member ref is empty".to_owned()));
    }
    if let Some((provider, model)) = reference.split_once('/')
        && config
            .providers
            .iter()
            .any(|entry| entry.id == provider && entry.enabled)
    {
        let _ = model;
        return Ok(());
    }
    if reference.starts_with("group/")
        && config
            .routes
            .iter()
            .any(|route| route.model == reference && !route.hidden)
    {
        return Ok(());
    }
    Err(ConfigError::Invalid(format!(
        "unknown member `{reference}`"
    )))
}

fn member_array(reference: &str) -> Item {
    value(Array::from_iter([member_value(reference, "", false, true)]))
}

fn member_value(reference: &str, effort: &str, fast: bool, enabled: bool) -> Value {
    if effort.is_empty() && !fast && enabled {
        return Value::String(Formatted::new(reference.to_owned()));
    }
    let mut table = InlineTable::new();
    if let Some((provider, model)) = reference.split_once('/') {
        table.insert(
            "provider",
            Value::String(Formatted::new(provider.to_owned())),
        );
        table.insert("model", Value::String(Formatted::new(model.to_owned())));
    } else if let Some(group) = reference.strip_prefix("group/") {
        table.insert("group", Value::String(Formatted::new(group.to_owned())));
    } else {
        table.insert(
            "provider",
            Value::String(Formatted::new(reference.to_owned())),
        );
    }
    if !effort.is_empty() {
        table.insert("effort", Value::String(Formatted::new(effort.to_owned())));
    }
    if fast {
        table.insert("fast", Value::Boolean(Formatted::new(true)));
    }
    if !enabled {
        table.insert("on", Value::Boolean(Formatted::new(false)));
    }
    Value::InlineTable(table)
}
