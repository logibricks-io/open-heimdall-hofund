//! Format-preserving routing group edits on `[[routes]]` tables.

use toml_edit::{Array, DocumentMut, Formatted, InlineTable, Item, Table, Value, value};

use crate::{
    ConfigError, ROUTING_NAMES, Stays, load, parse_rule_line, provider_id_valid,
    rule_rename_group_ref, rule_without_line,
};

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
) -> Result<(String, String), ConfigError> {
    if let Some(lines) = rules {
        validate_group_rules(lines)?;
    }
    let before = load(text)?;
    let old_model = format!("group/{group_id}");
    let old_route = before
        .routes
        .iter()
        .find(|route| route.model == old_model)
        .ok_or_else(|| ConfigError::Invalid(format!("unknown group `{group_id}`")))?;
    let old_member_refs = member_refs_from_route(old_route);
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
        let new_refs: Vec<String> = entries
            .iter()
            .map(|(reference, _, _, _)| reference.clone())
            .collect();
        if rules.is_none() {
            let removed: Vec<String> = old_member_refs
                .iter()
                .filter(|reference| !new_refs.iter().any(|left| left == *reference))
                .cloned()
                .collect();
            if !removed.is_empty() && !old_route.rules.is_empty() {
                let mut rewritten = Vec::new();
                for line in &old_route.rules {
                    let mut current = line.clone();
                    for reference in &removed {
                        if let Some(next) = rule_without_line(&current, reference) {
                            current = next;
                        } else {
                            current.clear();
                            break;
                        }
                    }
                    if !current.trim().is_empty() {
                        rewritten.push(current);
                    }
                }
                let mut array = Array::new();
                for line in &rewritten {
                    array.push(line.as_str());
                }
                if array.is_empty() {
                    table.remove("rules");
                } else {
                    table.insert("rules", value(array));
                }
            }
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
    let mut effective_id = group_id.to_owned();
    if let Some(new_id) = rename_to {
        let new_id = new_id.trim();
        if !provider_id_valid(new_id) {
            return Err(ConfigError::Invalid(format!(
                "group id `{new_id}` is not valid kebab-case"
            )));
        }
        if new_id == group_id {
            return Ok((document.to_string(), effective_id));
        }
        let new_model = format!("group/{new_id}");
        let old_model = format!("group/{group_id}");
        if document
            .get("routes")
            .and_then(Item::as_array_of_tables)
            .is_some_and(|routes| {
                routes.iter().any(|table| {
                    table.get("model").and_then(Item::as_str) == Some(new_model.as_str())
                })
            })
        {
            return Err(ConfigError::Invalid(format!(
                "there is a group `{new_id}` already"
            )));
        }
        rewrite_group_refs_in_document(&mut document, group_id, new_id)?;
        if let Some(routes) = document
            .get_mut("routes")
            .and_then(Item::as_array_of_tables_mut)
        {
            for table in routes.iter_mut() {
                if table.get("model").and_then(Item::as_str) == Some(old_model.as_str()) {
                    table.insert("model", value(new_model.as_str()));
                }
            }
        }
        effective_id = new_id.to_owned();
    }
    Ok((document.to_string(), effective_id))
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

fn member_refs_from_route(route: &crate::Route) -> Vec<String> {
    route
        .upstreams
        .iter()
        .map(|upstream| match &upstream.target {
            crate::UpstreamTarget::Provider(id) => upstream
                .model
                .as_ref()
                .map(|model| format!("{id}/{model}"))
                .unwrap_or_else(|| id.clone()),
            crate::UpstreamTarget::Subscription(id) => upstream
                .model
                .as_ref()
                .map(|model| format!("{id}/{model}"))
                .unwrap_or_else(|| id.clone()),
            crate::UpstreamTarget::Group(id) => format!("group/{id}"),
        })
        .collect()
}

fn rewrite_group_refs_in_document(
    document: &mut DocumentMut,
    from_id: &str,
    to_id: &str,
) -> Result<(), ConfigError> {
    let old_ref = format!("group/{from_id}");
    let new_ref = format!("group/{to_id}");
    let Some(routes) = document
        .get_mut("routes")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return Ok(());
    };
    for table in routes.iter_mut() {
        let model = table
            .get("model")
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(classifier) = table.get_mut("classifier")
            && classifier.as_str() == Some(old_ref.as_str())
        {
            *classifier = value(new_ref.as_str());
        }
        if let Some(upstreams) = table.get_mut("upstreams").and_then(Item::as_array_mut) {
            for entry in upstreams.iter_mut() {
                rewrite_upstream_group_ref(entry, &old_ref, &new_ref);
            }
        }
        if model.starts_with("group/")
            && let Some(rules) = table.get_mut("rules").and_then(Item::as_array_mut)
        {
            for entry in rules.iter_mut() {
                if let Some(line) = entry.as_str() {
                    *entry = Value::String(Formatted::new(rule_rename_group_ref(
                        line, &old_ref, &new_ref,
                    )));
                }
            }
        }
    }
    Ok(())
}

fn rewrite_upstream_group_ref(entry: &mut Value, old_ref: &str, new_ref: &str) {
    if let Some(text) = entry.as_str()
        && text == old_ref
    {
        *entry = Value::String(Formatted::new(new_ref.to_owned()));
        return;
    }
    if let Value::InlineTable(table) = entry
        && let Some(group) = table.get("group").and_then(|value| value.as_str())
        && format!("group/{group}") == old_ref
    {
        table.insert(
            "group",
            Value::String(Formatted::new(
                new_ref.strip_prefix("group/").unwrap_or(new_ref).to_owned(),
            )),
        );
    }
}

fn member_array(reference: &str) -> Item {
    value(Array::from_iter([member_value(reference, "", false, true)]))
}

fn member_value(reference: &str, effort: &str, fast: bool, enabled: bool) -> Value {
    if effort.is_empty() && !fast && enabled && !reference.contains('/') {
        return Value::String(Formatted::new(reference.to_owned()));
    }
    let mut table = InlineTable::new();
    if let Some(group) = reference.strip_prefix("group/") {
        table.insert("group", Value::String(Formatted::new(group.to_owned())));
    } else if let Some((provider, model)) = reference.split_once('/') {
        table.insert(
            "provider",
            Value::String(Formatted::new(provider.to_owned())),
        );
        table.insert("model", Value::String(Formatted::new(model.to_owned())));
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

#[cfg(test)]
mod tests {
    use super::{edit_group_create, edit_group_patch};
    use crate::{UpstreamTarget, load};

    const BASE: &str = "\
listen = \"127.0.0.1:0\"\n\
[[providers]]\nid = \"chat\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1/v1\"\napi_key = \"k\"\n\
[[routes]]\nmodel = \"m\"\nupstreams = [\"chat\"]\n";

    #[test]
    fn r7a_group_create_and_patch_keep_group_member_refs() {
        let (inner_text, _) = edit_group_create(BASE, "Inner", "chat/m").expect("inner group");
        let (with_outer, _) =
            edit_group_create(&inner_text, "Outer", "group/inner").expect("outer group");
        let config = load(&with_outer).expect("load after create");
        let outer = config
            .routes
            .iter()
            .find(|route| route.model == "group/outer")
            .expect("outer route");
        assert_eq!(outer.upstreams.len(), 1);
        assert!(matches!(
            &outer.upstreams[0].target,
            UpstreamTarget::Group(id) if id == "inner"
        ));

        let (patched, _) = edit_group_patch(
            &with_outer,
            "outer",
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&[
                ("group/inner".to_owned(), "high".to_owned(), true, true),
                ("chat/m".to_owned(), String::new(), false, true),
            ]),
            None,
            None,
        )
        .expect("patch members");
        let config = load(&patched).expect("load after patch");
        let outer = config
            .routes
            .iter()
            .find(|route| route.model == "group/outer")
            .expect("outer route");
        assert_eq!(outer.upstreams.len(), 2);
        assert!(matches!(
            &outer.upstreams[0].target,
            UpstreamTarget::Group(id) if id == "inner"
        ));
        assert_eq!(outer.upstreams[0].effort.as_deref(), Some("high"));
        assert!(outer.upstreams[0].fast);
    }
}
