use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

/// Folds a tool schema's root unions into one object schema and reports
/// whether the schema needed folding.
pub fn fold_object_root(schema: &mut Value) -> bool {
    let root = schema.clone();
    fold(&root, schema, 0)
}

fn fold(root: &Value, schema: &mut Value, depth: u32) -> bool {
    let Some(object) = schema.as_object_mut() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) == Some("object")
        && !["anyOf", "oneOf", "allOf"]
            .iter()
            .any(|key| object.contains_key(*key))
    {
        return false;
    }
    let mut properties = object
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut required = object
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(members) = object.get("allOf").and_then(Value::as_array) {
        for member in members {
            let Some(branch) = resolve(root, member) else {
                continue;
            };
            if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
                for (name, schema) in branch_properties {
                    properties
                        .entry(name.clone())
                        .or_insert_with(|| schema.clone());
                }
            }
            if let Some(branch_required) = branch.get("required").and_then(Value::as_array) {
                required.extend(branch_required.clone());
            }
        }
    }
    object.remove("allOf");
    let mut branches = Vec::new();
    for key in ["anyOf", "oneOf"] {
        if let Some(members) = object.get(key).and_then(Value::as_array) {
            for member in members {
                let Some(mut branch) = resolve(root, member) else {
                    continue;
                };
                if nested_union(&branch) && depth < 8 {
                    let mut folded = Value::Object(branch.clone());
                    fold(root, &mut folded, depth + 1);
                    if let Value::Object(folded) = folded {
                        branch = folded;
                    }
                }
                if branch.get("type").and_then(Value::as_str) != Some("object")
                    && !branch.contains_key("properties")
                {
                    continue;
                }
                branches.push(branch);
            }
        }
        object.remove(key);
    }

    let mut common = Vec::new();
    let mut from: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut order = Vec::new();
    for (index, branch) in branches.iter().enumerate() {
        if let Some(branch_properties) = branch.get("properties").and_then(Value::as_object) {
            let mut names: Vec<&String> = branch_properties.keys().collect();
            names.sort();
            for name in names {
                if properties.contains_key(name) {
                    continue;
                }
                if !from.contains_key(name) {
                    order.push(name.clone());
                }
                let values = from.entry(name.clone()).or_default();
                let schema = &branch_properties[name];
                if !values.contains(schema) {
                    values.push(schema.clone());
                }
            }
        }
        let branch_required = branch
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if index == 0 {
            common = branch_required;
        } else {
            common.retain(|name| branch_required.contains(name));
        }
    }
    for name in order {
        let values = from.remove(&name).unwrap_or_default();
        if values.len() == 1 {
            properties.insert(name, values.into_iter().next().unwrap_or_default());
        } else {
            properties.insert(name, Value::Array(values));
        }
    }
    required.extend(common);
    let mut seen = BTreeSet::new();
    required.retain(|name| {
        name.as_str()
            .is_none_or(|name| seen.insert(name.to_owned()))
    });

    object.insert("type".to_owned(), Value::String("object".to_owned()));
    object.insert("properties".to_owned(), Value::Object(properties));
    if required.is_empty() {
        object.remove("required");
    } else {
        object.insert("required".to_owned(), Value::Array(required));
    }
    true
}

fn nested_union(branch: &Map<String, Value>) -> bool {
    ["anyOf", "oneOf", "allOf"]
        .iter()
        .any(|key| branch.contains_key(*key))
}

fn resolve(root: &Value, branch: &Value) -> Option<Map<String, Value>> {
    let object = branch.as_object()?;
    let Some(reference) = object.get("$ref").and_then(Value::as_str) else {
        return Some(object.clone());
    };
    for key in ["$defs", "definitions"] {
        let prefix = format!("#/{key}/");
        if let Some(name) = reference.strip_prefix(&prefix) {
            return root.get(key)?.get(name)?.as_object().cloned();
        }
    }
    None
}
