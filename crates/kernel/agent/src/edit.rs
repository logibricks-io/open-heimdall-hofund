//! JSON, TOML, YAML, and env-file edits that keep unrelated keys.

use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item};
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use hofund_plugin_api::OwnerTest;

use crate::AgentError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    Json,
    Toml,
    Env,
    Yaml,
}

pub(crate) struct Assignment {
    pub key: &'static str,
    pub value: String,
    /// When set, `value` is JSON text and is written as that value, not as a string.
    pub raw_json: bool,
    /// When set, `value` is a JSON array of hofund's entries, merged into the
    /// array at `key`: entries passing these tests are replaced, others kept.
    pub owner: Option<&'static [OwnerTest]>,
}

pub(crate) fn render(
    kind: FileKind,
    original: Option<&str>,
    assignments: &[Assignment],
) -> Result<Vec<u8>, AgentError> {
    let text = match kind {
        FileKind::Json => render_json(original.unwrap_or(""), assignments)?,
        FileKind::Toml => render_toml(original.unwrap_or(""), assignments)?,
        FileKind::Env => render_env(original.unwrap_or(""), assignments),
        FileKind::Yaml => render_yaml(original.unwrap_or(""), assignments)?,
    };
    Ok(text.into_bytes())
}

pub(crate) fn get(kind: FileKind, text: &str, key: &str) -> Result<Option<String>, AgentError> {
    match kind {
        FileKind::Json => json_get(&json_root(text)?, key),
        FileKind::Toml => Ok(toml_get(&parse_toml(text)?, key)),
        FileKind::Env => Ok(env_get(text, key)),
        FileKind::Yaml => Ok(yaml_get(&yaml_root(text)?, key)),
    }
}

/// The string stored at `key`, when that value is a string.
pub(crate) fn string_at(
    kind: FileKind,
    text: &str,
    key: &str,
) -> Result<Option<String>, AgentError> {
    match kind {
        FileKind::Json => json_string(&json_root(text)?, key),
        FileKind::Toml => Ok(toml_get(&parse_toml(text)?, key)),
        FileKind::Env => Ok(env_get(text, key)),
        FileKind::Yaml => Ok(yaml_string(&yaml_root(text)?, key)),
    }
}

/// Deletes `keys` and drops object or table parents that become empty.
pub(crate) fn remove_keys(
    kind: FileKind,
    original: &str,
    keys: &[&str],
) -> Result<Vec<u8>, AgentError> {
    let text = match kind {
        FileKind::Json => remove_json(original, keys)?,
        FileKind::Toml => remove_toml(original, keys)?,
        FileKind::Env => remove_env(original, keys),
        FileKind::Yaml => remove_yaml(original, keys)?,
    };
    Ok(text.into_bytes())
}

/// Whether `bytes` has no keys left. Comments alone count as empty.
pub(crate) fn is_empty_document(kind: FileKind, bytes: &[u8]) -> Result<bool, AgentError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AgentError::message("file is not UTF-8".to_owned()))?;
    match kind {
        FileKind::Json => Ok(match json_root(text)? {
            Value::Object(object) => object.is_empty(),
            Value::Array(items) => items.is_empty(),
            _ => false,
        }),
        FileKind::Toml => Ok(parse_toml(text)?.as_table().is_empty()),
        FileKind::Env => Ok(!split_lines(text)
            .iter()
            .any(|line| parse_assignment(line).is_some())),
        FileKind::Yaml => Ok(match yaml_root(text)? {
            Yaml::Hash(hash) => hash.is_empty(),
            Yaml::Array(items) => items.is_empty(),
            _ => false,
        }),
    }
}

/// The array at `key` without hofund's entries, as JSON text.
///
/// `None` when the key is absent. Only JSON and YAML files hold arrays.
pub(crate) fn strip_owned(
    kind: FileKind,
    text: &str,
    key: &str,
    owner: &[OwnerTest],
) -> Result<Option<String>, AgentError> {
    let path = split_key(key);
    let existing = match kind {
        FileKind::Json => json_child(&json_root(text)?, &path).cloned(),
        FileKind::Yaml => yaml_child(&yaml_root(text)?, &path).map(yaml_to_json),
        FileKind::Toml | FileKind::Env => {
            return Err(AgentError::message(format!(
                "`{key}` cannot hold an array in this file kind"
            )));
        }
    };
    match kept_entries(existing.as_ref(), path.is_empty(), key, owner)? {
        Some(kept) => Ok(Some(Value::Array(kept).to_string())),
        None => Ok(None),
    }
}

/// The user's entries of an existing array value, in order.
///
/// `None` when there is no array yet. An empty object root counts as no
/// array, so a missing root-array file can be created.
fn kept_entries(
    existing: Option<&Value>,
    root: bool,
    key: &str,
    owner: &[OwnerTest],
) -> Result<Option<Vec<Value>>, AgentError> {
    match existing {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(object)) if root && object.is_empty() => Ok(None),
        Some(Value::Array(items)) => Ok(Some(
            items
                .iter()
                .filter(|item| !owned(item, owner))
                .cloned()
                .collect(),
        )),
        Some(_) => Err(AgentError::message(format!("`{key}` is not an array"))),
    }
}

/// Whether hofund owns an array entry: an object passing every test.
fn owned(item: &Value, owner: &[OwnerTest]) -> bool {
    let Some(object) = item.as_object() else {
        return false;
    };
    !owner.is_empty()
        && owner.iter().all(|test| {
            object
                .get(test.key())
                .and_then(Value::as_str)
                .is_some_and(|value| test.matches(value))
        })
}

/// The user's entries followed by hofund's, or `None` to leave the key out.
fn merged(
    existing: Option<&Value>,
    root: bool,
    assignment: &Assignment,
    owner: &[OwnerTest],
) -> Result<Option<Value>, AgentError> {
    let ours: Value = serde_json::from_str(&assignment.value).map_err(|error| {
        AgentError::message(format!("`{}` is not JSON: {error}", assignment.key))
    })?;
    let Value::Array(ours) = ours else {
        return Err(AgentError::message(format!(
            "`{}` entries are not an array",
            assignment.key
        )));
    };
    let kept = kept_entries(existing, root, assignment.key, owner)?;
    if kept.is_none() && ours.is_empty() {
        return Ok(None);
    }
    let mut items = kept.unwrap_or_default();
    items.extend(ours);
    Ok(Some(Value::Array(items)))
}

fn json_child<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cursor = value;
    for part in path {
        cursor = cursor.as_object()?.get(*part)?;
    }
    Some(cursor)
}

fn render_json(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    let mut root = json_root(original)?;
    let string_model = assignments.iter().find_map(string_model_restore);
    let writes_model_name = assignments.iter().any(|assignment| {
        assignment.key == "model.name" && string_model_restore(assignment).is_none()
    });
    let promotes_string_model = writes_model_name
        || assignments
            .iter()
            .any(|assignment| assignment.key == "security.auth.selectedType");
    if promotes_string_model {
        promote_string_model(&mut root, !writes_model_name);
    }
    if let Some(name) = string_model {
        restore_string_model(&mut root, name)?;
    }
    for assignment in assignments {
        if string_model_restore(assignment).is_some() {
            continue;
        }
        let path = split_key(assignment.key);
        if let Some(owner) = assignment.owner {
            let existing = json_child(&root, &path);
            let Some(value) = merged(existing, path.is_empty(), assignment, owner)? else {
                continue;
            };
            if path.is_empty() {
                root = value;
            } else {
                set_json(&mut root, &path, &value.to_string(), true)?;
            }
            continue;
        }
        if path.is_empty() && assignment.raw_json {
            root = serde_json::from_str(&assignment.value)
                .map_err(|error| AgentError::message(format!("root is not JSON: {error}")))?;
            continue;
        }
        set_json(
            &mut root,
            &split_key(assignment.key),
            &assignment.value,
            assignment.raw_json,
        )?;
    }
    pretty_json(&root)
}

fn render_toml(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    let mut document = parse_toml(original)?;
    for assignment in assignments {
        set_toml(
            document.as_item_mut(),
            &split_key(assignment.key),
            &assignment.value,
        )?;
    }
    let mut text = document.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn render_env(original: &str, assignments: &[Assignment]) -> String {
    let newline = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut lines = split_lines(original);
    let mut seen = Vec::new();
    for line in &mut lines {
        let Some((prefix, key, _)) = parse_assignment(line) else {
            continue;
        };
        let Some(assignment) = assignments.iter().find(|assignment| assignment.key == key) else {
            continue;
        };
        *line = format!("{prefix}{key}={}", quote_env(&assignment.value));
        seen.push(key);
    }
    for assignment in assignments {
        if !seen.iter().any(|key| *key == assignment.key) {
            lines.push(format!(
                "{}={}",
                assignment.key,
                quote_env(&assignment.value)
            ));
        }
    }
    let mut text = lines.join(newline);
    if !text.is_empty() && !text.ends_with(newline) {
        text.push_str(newline);
    }
    text
}

pub(crate) fn json_root(text: &str) -> Result<Value, AgentError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok(value);
    }
    serde_json::from_str(&loosen_json(trimmed))
        .map_err(|error| AgentError::message(format!("JSON: {error}")))
}

fn remove_json(original: &str, keys: &[&str]) -> Result<String, AgentError> {
    let mut root = json_root(original)?;
    for key in keys {
        // `model.name` may still be the older string `model`.
        if *key == "model.name"
            && let Some(object) = root.as_object_mut()
            && object.get("model").is_some_and(Value::is_string)
        {
            object.remove("model");
            continue;
        }
        unset_json(&mut root, &split_key(key))?;
    }
    pretty_json(&root)
}

fn promote_string_model(root: &mut Value, preserve_name: bool) {
    let Some(object) = root.as_object_mut() else {
        return;
    };
    if object.get("model").is_some_and(Value::is_string) {
        let name = object.remove("model").expect("string model");
        if preserve_name {
            object.insert("model".to_owned(), serde_json::json!({ "name": name }));
        }
    }
}

fn string_model_restore(assignment: &Assignment) -> Option<String> {
    if assignment.key != "model.name" || !assignment.raw_json {
        return None;
    }
    let Value::Object(model) = serde_json::from_str(&assignment.value).ok()? else {
        return None;
    };
    if model.len() != 1 {
        return None;
    }
    match model.get("name") {
        Some(Value::String(name)) => Some(name.clone()),
        _ => None,
    }
}

fn restore_string_model(root: &mut Value, name: String) -> Result<(), AgentError> {
    let restorable = root
        .get("model")
        .and_then(Value::as_object)
        .is_some_and(|model| model.len() == 1 && model.get("name").is_some_and(Value::is_string));
    if !restorable {
        return Err(AgentError::message(
            "`model` must contain only a string `name` to restore the original string; move other keys out of `model` before disconnect",
        ));
    }
    root.as_object_mut()
        .expect("JSON root")
        .insert("model".to_owned(), Value::String(name));
    Ok(())
}

fn pretty_json(root: &Value) -> Result<String, AgentError> {
    let mut bytes = serde_json::to_vec_pretty(root)
        .map_err(|error| AgentError::message(format!("JSON: {error}")))?;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    String::from_utf8(bytes).map_err(|error| AgentError::message(format!("JSON: {error}")))
}

fn unset_json(value: &mut Value, path: &[&str]) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty JSON key"));
    };
    if !value.is_object() {
        return Err(AgentError::message("JSON value is not an object"));
    }
    let object = value.as_object_mut().expect("checked object");
    if rest.is_empty() {
        object.remove(*key);
        return Ok(());
    }
    let Some(child) = object.get(*key) else {
        return Ok(());
    };
    if !child.is_object() {
        return Err(AgentError::message(format!("`{key}` is not an object")));
    }
    unset_json(object.get_mut(*key).expect("child"), rest)?;
    let empty = object
        .get(*key)
        .and_then(Value::as_object)
        .is_some_and(|child| child.is_empty());
    if empty {
        object.remove(*key);
    }
    Ok(())
}

pub(crate) fn json_string(value: &Value, key: &str) -> Result<Option<String>, AgentError> {
    let mut cursor = value;
    for part in split_key(key) {
        let Some(object) = cursor.as_object() else {
            return Err(AgentError::message(format!("`{part}` is not an object")));
        };
        let Some(child) = object.get(part) else {
            return Ok(None);
        };
        cursor = child;
    }
    match cursor {
        Value::String(text) => Ok(Some(text.clone())),
        _ => Ok(None),
    }
}

fn json_get(value: &Value, key: &str) -> Result<Option<String>, AgentError> {
    if key == "model.name" {
        return json_model_name(value);
    }
    let mut cursor = value;
    for part in split_key(key) {
        let Some(object) = cursor.as_object() else {
            return Err(AgentError::message(format!("`{part}` is not an object")));
        };
        let Some(child) = object.get(part) else {
            return Ok(None);
        };
        cursor = child;
    }
    match cursor {
        Value::String(text) => Ok(Some(text.clone())),
        Value::Null => Ok(None),
        _ => Ok(Some(cursor.to_string())),
    }
}

fn json_model_name(value: &Value) -> Result<Option<String>, AgentError> {
    let Some(root) = value.as_object() else {
        return Err(AgentError::message(
            "cannot read `model.name` unless the JSON root is an object; restore an object before disconnect",
        ));
    };
    let Some(model) = root.get("model") else {
        return Ok(None);
    };
    let model = match model {
        Value::String(model) => {
            return Ok(Some(serde_json::json!({ "name": model }).to_string()));
        }
        Value::Object(model) => model,
        _ => {
            return Err(AgentError::message(
                "`model` must be a string or an object containing `name`; restore `model` to one of those shapes before disconnect",
            ));
        }
    };
    match model.get("name") {
        Some(Value::String(name)) => Ok(Some(name.clone())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(AgentError::message(
            "`model.name` must be a string or null; restore `model.name` to a string before disconnect",
        )),
    }
}

fn set_json(
    value: &mut Value,
    path: &[&str],
    text: &str,
    raw_json: bool,
) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty JSON key"));
    };
    if !value.is_object() {
        return Err(AgentError::message("JSON value is not an object"));
    }
    let object = value.as_object_mut().expect("checked object");
    if rest.is_empty() {
        let written = if raw_json {
            serde_json::from_str(text)
                .map_err(|error| AgentError::message(format!("`{key}` is not JSON: {error}")))?
        } else {
            Value::String(text.to_owned())
        };
        object.insert((*key).to_owned(), written);
        return Ok(());
    }
    if let Some(child) = object.get(*key) {
        if !child.is_object() {
            return Err(AgentError::message(format!("`{key}` is not an object")));
        }
    } else {
        object.insert((*key).to_owned(), Value::Object(Map::new()));
    }
    set_json(object.get_mut(*key).expect("child"), rest, text, raw_json)
}

fn render_yaml(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    let mut root = yaml_root(original)?;
    for assignment in assignments {
        let path = split_key(assignment.key);
        if let Some(owner) = assignment.owner {
            let existing = yaml_child(&root, &path).map(yaml_to_json);
            let Some(value) = merged(existing.as_ref(), path.is_empty(), assignment, owner)? else {
                continue;
            };
            if path.is_empty() {
                root = json_to_yaml(&value);
            } else {
                set_yaml(&mut root, &path, json_to_yaml(&value))?;
            }
            continue;
        }
        let value = if assignment.raw_json {
            let value: Value = serde_json::from_str(&assignment.value).map_err(|error| {
                AgentError::message(format!("`{}` is not JSON: {error}", assignment.key))
            })?;
            json_to_yaml(&value)
        } else {
            Yaml::String(assignment.value.clone())
        };
        if path.is_empty() && assignment.raw_json {
            root = value;
            continue;
        }
        set_yaml(&mut root, &path, value)?;
    }
    emit_yaml(&root)
}

fn remove_yaml(original: &str, keys: &[&str]) -> Result<String, AgentError> {
    let mut root = yaml_root(original)?;
    for key in keys {
        unset_yaml(&mut root, &split_key(key))?;
    }
    emit_yaml(&root)
}

/// Parses one YAML document. An empty file is an empty mapping.
fn yaml_root(text: &str) -> Result<Yaml, AgentError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut documents = YamlLoader::load_from_str(text)
        .map_err(|error| AgentError::message(format!("YAML: {error}")))?;
    match documents.len() {
        0 => Ok(Yaml::Hash(yaml_rust2::yaml::Hash::new())),
        1 => {
            let root = documents.remove(0);
            Ok(if root.is_null() {
                Yaml::Hash(yaml_rust2::yaml::Hash::new())
            } else {
                root
            })
        }
        _ => Err(AgentError::message(
            "YAML: the file has more than one document; keep one document before connecting",
        )),
    }
}

fn emit_yaml(root: &Yaml) -> Result<String, AgentError> {
    let mut text = String::new();
    YamlEmitter::new(&mut text)
        .dump(root)
        .map_err(|error| AgentError::message(format!("YAML: {error}")))?;
    // The emitter always opens with a document marker.
    let mut text = text.strip_prefix("---\n").unwrap_or(&text).to_owned();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn yaml_key(key: &str) -> Yaml {
    Yaml::String(key.to_owned())
}

fn yaml_child<'a>(value: &'a Yaml, path: &[&str]) -> Option<&'a Yaml> {
    let mut cursor = value;
    for part in path {
        cursor = match cursor {
            Yaml::Hash(hash) => hash.get(&yaml_key(part))?,
            _ => return None,
        };
    }
    Some(cursor)
}

fn yaml_string(value: &Yaml, key: &str) -> Option<String> {
    match yaml_child(value, &split_key(key))? {
        Yaml::String(text) => Some(text.clone()),
        _ => None,
    }
}

/// The value at `key`: a string as itself, anything else as JSON text.
fn yaml_get(value: &Yaml, key: &str) -> Option<String> {
    match yaml_child(value, &split_key(key))? {
        Yaml::String(text) => Some(text.clone()),
        Yaml::Null | Yaml::BadValue => None,
        other => Some(yaml_to_json(other).to_string()),
    }
}

fn set_yaml(value: &mut Yaml, path: &[&str], written: Yaml) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty YAML key"));
    };
    let Yaml::Hash(hash) = value else {
        return Err(AgentError::message("YAML value is not a mapping"));
    };
    if rest.is_empty() {
        // Replacing in place keeps the key where the user put it.
        match hash.get_mut(&yaml_key(key)) {
            Some(slot) => *slot = written,
            None => {
                hash.insert(yaml_key(key), written);
            }
        }
        return Ok(());
    }
    // `entry` would move an existing key to the end, so look it up first.
    let name = yaml_key(key);
    if !hash.contains_key(&name) {
        hash.insert(name.clone(), Yaml::Hash(yaml_rust2::yaml::Hash::new()));
    }
    let Some(child) = hash.get_mut(&name) else {
        return Err(AgentError::message(format!("`{key}` is missing")));
    };
    if child.is_null() {
        *child = Yaml::Hash(yaml_rust2::yaml::Hash::new());
    }
    if !child.is_hash() {
        return Err(AgentError::message(format!("`{key}` is not a mapping")));
    }
    set_yaml(child, rest, written)
}

fn unset_yaml(value: &mut Yaml, path: &[&str]) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty YAML key"));
    };
    let Yaml::Hash(hash) = value else {
        return Err(AgentError::message("YAML value is not a mapping"));
    };
    let key = yaml_key(key);
    if rest.is_empty() {
        hash.remove(&key);
        return Ok(());
    }
    let Some(child) = hash.get_mut(&key) else {
        return Ok(());
    };
    if !child.is_hash() {
        return Err(AgentError::message(format!(
            "`{}` is not a mapping",
            key.as_str().unwrap_or_default()
        )));
    }
    unset_yaml(child, rest)?;
    if matches!(child, Yaml::Hash(inner) if inner.is_empty()) {
        hash.remove(&key);
    }
    Ok(())
}

fn json_to_yaml(value: &Value) -> Yaml {
    match value {
        Value::Null => Yaml::Null,
        Value::Bool(flag) => Yaml::Boolean(*flag),
        Value::Number(number) => number
            .as_i64()
            .map_or_else(|| Yaml::Real(number.to_string()), Yaml::Integer),
        Value::String(text) => Yaml::String(text.clone()),
        Value::Array(items) => Yaml::Array(items.iter().map(json_to_yaml).collect()),
        Value::Object(object) => Yaml::Hash(
            object
                .iter()
                .map(|(key, value)| (yaml_key(key), json_to_yaml(value)))
                .collect(),
        ),
    }
}

fn yaml_to_json(value: &Yaml) -> Value {
    match value {
        Yaml::Real(text) => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| Value::String(text.clone()), Value::Number),
        Yaml::Integer(number) => Value::from(*number),
        Yaml::String(text) => Value::String(text.clone()),
        Yaml::Boolean(flag) => Value::Bool(*flag),
        Yaml::Array(items) => Value::Array(items.iter().map(yaml_to_json).collect()),
        Yaml::Hash(hash) => Value::Object(
            hash.iter()
                .map(|(key, value)| {
                    let key = match key {
                        Yaml::String(text) => text.clone(),
                        other => yaml_to_json(other).to_string(),
                    };
                    (key, yaml_to_json(value))
                })
                .collect(),
        ),
        Yaml::Alias(_) | Yaml::Null | Yaml::BadValue => Value::Null,
    }
}

fn parse_toml(text: &str) -> Result<DocumentMut, AgentError> {
    if text.trim().is_empty() {
        return Ok(DocumentMut::new());
    }
    text.parse::<DocumentMut>()
        .map_err(|error| AgentError::message(format!("TOML: {error}")))
}

fn toml_get(document: &DocumentMut, key: &str) -> Option<String> {
    lookup(document.as_item(), &split_key(key)).map(ToOwned::to_owned)
}

fn lookup<'a>(item: &'a Item, path: &[&str]) -> Option<&'a str> {
    let (key, rest) = path.split_first()?;
    let child = item.as_table_like()?.get(key)?;
    if rest.is_empty() {
        child.as_str()
    } else {
        lookup(child, rest)
    }
}

fn remove_toml(original: &str, keys: &[&str]) -> Result<String, AgentError> {
    let mut document = parse_toml(original)?;
    for key in keys {
        unset_toml(document.as_item_mut(), &split_key(key))?;
    }
    let mut text = document.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn unset_toml(item: &mut Item, path: &[&str]) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty TOML key"));
    };
    if item.as_table_like().is_none() {
        return Err(AgentError::message(format!("`{key}` is not a table")));
    }
    if rest.is_empty() {
        if let Some(table) = item.as_table_like_mut() {
            table.remove(key);
        }
        return Ok(());
    }
    let present = item
        .as_table_like()
        .and_then(|table| table.get(key))
        .is_some();
    if !present {
        return Ok(());
    }
    let is_table = item
        .as_table_like()
        .and_then(|table| table.get(key))
        .is_some_and(Item::is_table_like);
    if !is_table {
        return Err(AgentError::message(format!("`{key}` is not a table")));
    }
    {
        let child = item
            .as_table_like_mut()
            .and_then(|table| table.get_mut(key))
            .expect("child");
        unset_toml(child, rest)?;
    }
    let empty = item
        .as_table_like()
        .and_then(|table| table.get(key))
        .and_then(Item::as_table_like)
        .is_some_and(|table| table.is_empty());
    if empty && let Some(table) = item.as_table_like_mut() {
        table.remove(key);
    }
    Ok(())
}

fn remove_env(original: &str, keys: &[&str]) -> String {
    let newline = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let lines: Vec<String> = split_lines(original)
        .into_iter()
        .filter(|line| match parse_assignment(line) {
            Some((_, key, _)) => !keys.iter().any(|candidate| *candidate == key),
            None => true,
        })
        .collect();
    let mut text = lines.join(newline);
    if !text.is_empty() && !text.ends_with(newline) {
        text.push_str(newline);
    }
    text
}

fn set_toml(item: &mut Item, path: &[&str], text: &str) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty TOML key"));
    };
    if rest.is_empty() {
        item[key] = toml_edit::value(text);
        return Ok(());
    }
    let missing = item
        .as_table_like()
        .and_then(|table| table.get(key))
        .is_none();
    if missing {
        item[key] = implicit_table();
    }
    let child = item
        .as_table_like_mut()
        .and_then(|table| table.get_mut(key))
        .ok_or_else(|| AgentError::message(format!("`{key}` is not a table")))?;
    if !child.is_table_like() {
        return Err(AgentError::message(format!("`{key}` is not a table")));
    }
    set_toml(child, rest, text)
}

fn implicit_table() -> Item {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    Item::Table(table)
}

fn split_key(key: &str) -> Vec<&str> {
    key.split('.').filter(|part| !part.is_empty()).collect()
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
        .collect();
    if text.ends_with('\n') && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn parse_assignment(line: &str) -> Option<(String, String, String)> {
    let leading_len = line.len() - line.trim_start().len();
    let leading = &line[..leading_len];
    let body = line[leading_len..].trim_end();
    if body.is_empty() || body.starts_with('#') {
        return None;
    }
    let (export, rest) = if let Some(rest) = body.strip_prefix("export ") {
        ("export ", rest.trim_start())
    } else if let Some(rest) = body.strip_prefix("export\t") {
        ("export ", rest.trim_start())
    } else {
        ("", body)
    };
    let (key, raw) = rest.split_once('=')?;
    let key = key.trim();
    if !is_env_key(key) {
        return None;
    }
    Some((
        format!("{leading}{export}"),
        key.to_owned(),
        unquote_env(raw.trim()),
    ))
}

fn env_get(text: &str, key: &str) -> Option<String> {
    for line in split_lines(text) {
        if let Some((_, found, value)) = parse_assignment(&line)
            && found == key
        {
            return Some(value);
        }
    }
    None
}

fn is_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|char| char == '_' || char.is_ascii_alphanumeric())
}

fn unquote_env(raw: &str) -> String {
    if let Some(inner) = raw.strip_prefix('"') {
        return unescape_double(inner);
    }
    if let Some(inner) = raw.strip_prefix('\'')
        && let Some((body, _)) = inner.split_once('\'')
    {
        return body.to_owned();
    }
    match raw.split_once(" #") {
        Some((value, _)) => value.trim_end().to_owned(),
        None => raw.to_owned(),
    }
}

fn unescape_double(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    let mut escape = false;
    for char in chars.by_ref() {
        if escape {
            match char {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => out.push(other),
            }
            escape = false;
            continue;
        }
        if char == '\\' {
            escape = true;
            continue;
        }
        if char == '"' {
            break;
        }
        out.push(char);
    }
    out
}

fn quote_env(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value
            .chars()
            .any(|char| char.is_whitespace() || matches!(char, '#' | '"' | '\'' | '\\'));
    if !needs_quotes {
        return value.to_owned();
    }
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// Drops `//` and `/* */` comments and trailing commas outside strings.
fn loosen_json(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(char) = chars.next() {
        if in_string {
            out.push(char);
            if escape {
                escape = false;
            } else if char == '\\' {
                escape = true;
            } else if char == '"' {
                in_string = false;
            }
            continue;
        }
        match char {
            '"' => {
                in_string = true;
                out.push(char);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    let mut previous = '\0';
                    for next in chars.by_ref() {
                        if previous == '*' && next == '/' {
                            break;
                        }
                        previous = next;
                    }
                }
                _ => out.push(char),
            },
            ',' => {
                let rest: String = chars.clone().collect();
                let trimmed = rest.trim_start();
                if !trimmed.starts_with('}') && !trimmed.starts_with(']') {
                    out.push(char);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Assignment, FileKind, get, is_empty_document, remove_keys, render, string_at, strip_owned,
    };
    use hofund_plugin_api::OwnerTest;

    static OWNER: &[OwnerTest] = &[
        OwnerTest::prefix("id", "hof/"),
        OwnerTest::equals("kind", "gw"),
    ];

    fn append(key: &'static str, value: &str) -> Assignment {
        Assignment {
            key,
            value: value.to_owned(),
            raw_json: true,
            owner: Some(OWNER),
        }
    }

    fn text(kind: FileKind, original: Option<&str>, assignments: &[Assignment]) -> String {
        String::from_utf8(render(kind, original, assignments).expect("render")).expect("utf8")
    }

    #[test]
    fn append_merges_after_user_entries_and_replaces_ours() {
        let ours = r#"[{"id":"hof/a","kind":"gw"}]"#;
        let original = r#"{"list":[{"id":"hof/old","kind":"gw"},{"id":"hof/x"},{"id":"mine","kind":"gw"},"s",{"id":7,"kind":"gw"}],"k":1}"#;
        let first = text(FileKind::Json, Some(original), &[append("list", ours)]);
        let value: serde_json::Value = serde_json::from_str(&first).expect("json");
        assert_eq!(
            value["list"],
            serde_json::json!([{"id":"hof/x"},{"id":"mine","kind":"gw"},"s",{"id":7,"kind":"gw"},{"id":"hof/a","kind":"gw"}])
        );
        assert_eq!(value["k"], 1);
        assert_eq!(
            text(FileKind::Json, Some(&first), &[append("list", ours)]),
            first
        );

        let created = text(FileKind::Json, Some("{}"), &[append("a.list", ours)]);
        let value: serde_json::Value = serde_json::from_str(&created).expect("json");
        assert_eq!(
            value["a"]["list"],
            serde_json::json!([{"id":"hof/a","kind":"gw"}])
        );
        let none = text(FileKind::Json, Some("{\"k\":1}"), &[append("list", "[]")]);
        assert!(!none.contains("list"), "{none}");
        let emptied = text(FileKind::Json, Some(&first), &[append("list", "[]")]);
        let value: serde_json::Value = serde_json::from_str(&emptied).expect("json");
        assert_eq!(value["list"].as_array().expect("list").len(), 4);
    }

    #[test]
    fn append_handles_a_root_array_and_yaml() {
        let ours = r#"[{"id":"hof/a","kind":"gw"}]"#;
        let root = text(FileKind::Json, None, &[append(".", ours)]);
        let value: serde_json::Value = serde_json::from_str(&root).expect("json");
        assert_eq!(value, serde_json::json!([{"id":"hof/a","kind":"gw"}]));
        let kept = text(
            FileKind::Json,
            Some(r#"[{"id":"u"}]"#),
            &[append(".", ours)],
        );
        let value: serde_json::Value = serde_json::from_str(&kept).expect("json");
        assert_eq!(
            value,
            serde_json::json!([{"id":"u"},{"id":"hof/a","kind":"gw"}])
        );
        assert_eq!(
            strip_owned(FileKind::Json, &kept, ".", OWNER).expect("strip"),
            Some(r#"[{"id":"u"}]"#.to_owned())
        );
        assert!(is_empty_document(FileKind::Json, b"[]").expect("empty"));
        assert!(!is_empty_document(FileKind::Json, b"[1]").expect("not empty"));
        assert!(is_empty_document(FileKind::Yaml, b"[]\n").expect("empty yaml"));

        let yaml = text(
            FileKind::Yaml,
            Some("list:\n  - id: u\nk: 1\n"),
            &[append("list", ours)],
        );
        assert_eq!(
            yaml,
            "list:\n  - id: u\n  - id: hof/a\n    kind: gw\nk: 1\n"
        );
        assert_eq!(
            text(FileKind::Yaml, Some(&yaml), &[append("list", ours)]),
            yaml
        );
        assert_eq!(
            strip_owned(FileKind::Yaml, &yaml, "list", OWNER).expect("strip"),
            Some(r#"[{"id":"u"}]"#.to_owned())
        );
        assert_eq!(
            strip_owned(FileKind::Yaml, "k: 1\n", "list", OWNER).expect("absent"),
            None
        );
    }

    #[test]
    fn append_rejects_shapes_it_cannot_merge() {
        let ours = r#"[{"id":"hof/a","kind":"gw"}]"#;
        for (kind, original, key) in [
            (FileKind::Json, r#"{"list":{"id":"x"}}"#, "list"),
            (FileKind::Json, r#"{"k":1}"#, "."),
            (FileKind::Yaml, "list: 3\n", "list"),
        ] {
            let error = render(kind, Some(original), &[append(key, ours)]).expect_err("shape");
            assert!(error.to_string().contains("is not an array"), "{error}");
            let error = strip_owned(kind, original, key, OWNER).expect_err("strip shape");
            assert!(error.to_string().contains("is not an array"), "{error}");
        }
        let error = render(FileKind::Json, None, &[append("list", "{}")]).expect_err("object");
        assert!(error.to_string().contains("not an array"), "{error}");
        let error = render(FileKind::Json, None, &[append("list", "[")]).expect_err("json");
        assert!(error.to_string().contains("is not JSON"), "{error}");
        for kind in [FileKind::Toml, FileKind::Env] {
            let error = strip_owned(kind, "", "list", OWNER).expect_err("kind");
            assert!(
                error.to_string().contains("cannot hold an array"),
                "{error}"
            );
        }
        assert_eq!(
            strip_owned(
                FileKind::Json,
                r#"{"l":[{"id":"hof/a","kind":"gw"}]}"#,
                "l",
                &[]
            )
            .expect("no owner"),
            Some(r#"[{"id":"hof/a","kind":"gw"}]"#.to_owned())
        );
    }

    fn assign(key: &'static str, value: &str, raw_json: bool) -> Assignment {
        Assignment {
            key,
            value: value.to_owned(),
            raw_json,
            owner: None,
        }
    }

    #[test]
    fn yaml_writes_into_existing_parents_in_place() {
        let original = "a:\n  x: 1\nb: 2\n";
        let assignments = [assign("a.z", "3", false), assign("a.x", "4", false)];
        let bytes = render(FileKind::Yaml, Some(original), &assignments).expect("render");
        assert_eq!(
            String::from_utf8(bytes).expect("utf8"),
            "a:\n  x: \"4\"\n  z: \"3\"\nb: 2\n"
        );
    }

    #[test]
    fn yaml_keeps_unrelated_keys_and_is_stable() {
        let original = "# a comment\nGOOSE_PROVIDER: openai\nextensions:\n  dev:\n    enabled: true\n    timeout: 300\nratio: 1.50\n";
        let assignments = [
            assign("GOOSE_PROVIDER", "hofund", false),
            assign("model.provider", "hofund", false),
        ];
        let bytes = render(FileKind::Yaml, Some(original), &assignments).expect("render");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(!text.starts_with("---"), "{text}");
        assert!(text.ends_with('\n'));
        assert_eq!(
            get(FileKind::Yaml, &text, "GOOSE_PROVIDER").expect("get"),
            Some("hofund".to_owned())
        );
        assert_eq!(
            get(FileKind::Yaml, &text, "model.provider").expect("get"),
            Some("hofund".to_owned())
        );
        assert_eq!(
            get(FileKind::Yaml, &text, "extensions.dev.enabled").expect("get"),
            Some("true".to_owned())
        );
        assert_eq!(
            get(FileKind::Yaml, &text, "extensions.dev.timeout").expect("get"),
            Some("300".to_owned())
        );
        assert!(text.contains("ratio: 1.50"), "{text}");
        assert_eq!(
            string_at(FileKind::Yaml, &text, "extensions.dev.enabled").expect("string"),
            None
        );
        let order: Vec<&str> = text.lines().filter(|line| !line.starts_with(' ')).collect();
        assert_eq!(
            order,
            [
                "GOOSE_PROVIDER: hofund",
                "extensions:",
                "ratio: 1.50",
                "model:"
            ]
        );
        let again = render(FileKind::Yaml, Some(&text), &assignments).expect("again");
        assert_eq!(again, text.into_bytes());
    }

    #[test]
    fn yaml_writes_raw_json_as_yaml_values() {
        let assignments = [assign(
            "providers.hofund",
            r#"{"base_url":"http://127.0.0.1:1/v1","models":["a","b"],"limit":2,"rate":0.5,"off":false,"none":null}"#,
            true,
        )];
        let bytes = render(FileKind::Yaml, None, &assignments).expect("render");
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(
            get(FileKind::Yaml, &text, "providers.hofund").expect("get"),
            Some(
                r#"{"base_url":"http://127.0.0.1:1/v1","models":["a","b"],"limit":2,"rate":0.5,"off":false,"none":null}"#
                    .to_owned()
            )
        );
        assert_eq!(
            get(FileKind::Yaml, &text, "providers.hofund.none").expect("get"),
            None
        );
        let error = render(FileKind::Yaml, None, &[assign("x", "{", true)]).expect_err("bad json");
        assert!(error.to_string().contains("`x` is not JSON"), "{error}");
    }

    #[test]
    fn yaml_removal_drops_empty_parents_only() {
        let original = "model:\n  provider: hofund\n  default: m\nproviders:\n  hofund:\n    url: u\nkeep: 1\n";
        let bytes = remove_keys(
            FileKind::Yaml,
            original,
            &[
                "model.provider",
                "providers.hofund.url",
                "absent.key",
                "keep.deeper",
            ],
        )
        .expect_err("keep is a scalar")
        .to_string();
        assert!(bytes.contains("`keep` is not a mapping"), "{bytes}");
        let bytes = remove_keys(
            FileKind::Yaml,
            original,
            &["model.provider", "providers.hofund.url", "absent.key"],
        )
        .expect("remove");
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(text, "model:\n  default: m\nkeep: 1\n");
        assert!(!is_empty_document(FileKind::Yaml, text.as_bytes()).expect("empty"));
        let bytes = remove_keys(FileKind::Yaml, &text, &["model.default", "keep"]).expect("remove");
        assert!(is_empty_document(FileKind::Yaml, &bytes).expect("empty"));
        assert!(is_empty_document(FileKind::Yaml, b"# only a comment\n").expect("empty"));
        assert!(!is_empty_document(FileKind::Yaml, b"- a\n").expect("list"));
    }

    #[test]
    fn yaml_rejects_shapes_it_cannot_edit() {
        let error = render(
            FileKind::Yaml,
            Some("a: 1\n---\nb: 2\n"),
            &[assign("a", "x", false)],
        )
        .expect_err("two documents");
        assert!(
            error.to_string().contains("more than one document"),
            "{error}"
        );
        let error = render(FileKind::Yaml, Some("- a\n"), &[assign("a", "x", false)])
            .expect_err("list root");
        assert!(error.to_string().contains("not a mapping"), "{error}");
        let error = render(
            FileKind::Yaml,
            Some("model: m\n"),
            &[assign("model.provider", "x", false)],
        )
        .expect_err("scalar parent");
        assert!(
            error.to_string().contains("`model` is not a mapping"),
            "{error}"
        );
        let error = render(FileKind::Yaml, Some("a: [\n"), &[]).expect_err("syntax");
        assert!(error.to_string().starts_with("YAML: "), "{error}");
        let bytes = render(
            FileKind::Yaml,
            Some("\u{feff}model:\n"),
            &[assign("model.provider", "x", false)],
        )
        .expect("null parent becomes a mapping");
        assert_eq!(
            String::from_utf8(bytes).expect("utf8"),
            "model:\n  provider: x\n"
        );
        let bytes = render(FileKind::Yaml, Some("~\n"), &[assign("a", "x", false)])
            .expect("null root becomes a mapping");
        assert_eq!(String::from_utf8(bytes).expect("utf8"), "a: x\n");
        let bytes = render(FileKind::Yaml, Some(""), &[]).expect("empty");
        assert_eq!(String::from_utf8(bytes).expect("utf8"), "{}\n");
    }

    #[test]
    fn json_keeps_unrelated_keys_and_accepts_jsonc() {
        let original = "{\n  // note\n  \"ui\": { \"theme\": \"dark\" },\n}\n";
        let assignments = [Assignment {
            key: "env.ANTHROPIC_BASE_URL",
            value: "http://127.0.0.1:3425".to_owned(),
            raw_json: false,
            owner: None,
        }];
        let bytes = render(FileKind::Json, Some(original), &assignments).expect("render");
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(
            get(FileKind::Json, &text, "ui.theme").expect("get"),
            Some("dark".to_owned())
        );
        assert_eq!(
            get(FileKind::Json, &text, "env.ANTHROPIC_BASE_URL").expect("get"),
            Some("http://127.0.0.1:3425".to_owned())
        );
        let again = render(FileKind::Json, Some(&text), &assignments).expect("again");
        assert_eq!(again, text.into_bytes());
    }

    #[test]
    fn toml_keeps_a_comment_and_an_unrelated_key() {
        let original = "# keep me\nmodel = \"gpt-5\"\n";
        let assignments = [Assignment {
            key: "model_provider",
            value: "hofund".to_owned(),
            raw_json: false,
            owner: None,
        }];
        let bytes = render(FileKind::Toml, Some(original), &assignments).expect("render");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("# keep me"));
        assert_eq!(
            get(FileKind::Toml, &text, "model").expect("get"),
            Some("gpt-5".to_owned())
        );
        assert_eq!(
            get(FileKind::Toml, &text, "model_provider").expect("get"),
            Some("hofund".to_owned())
        );
        let again = render(FileKind::Toml, Some(&text), &assignments).expect("again");
        assert_eq!(again, text.into_bytes());
    }

    #[test]
    fn env_preserves_comments_and_quotes_a_hash() {
        let original = "# note\nexport KEEP=1\nGEMINI_API_KEY=old\n";
        let assignments = [
            Assignment {
                key: "GEMINI_API_KEY",
                value: "local-token".to_owned(),
                raw_json: false,
                owner: None,
            },
            Assignment {
                key: "GOOGLE_GEMINI_BASE_URL",
                value: "http://127.0.0.1:3425".to_owned(),
                raw_json: false,
                owner: None,
            },
            Assignment {
                key: "ODD",
                value: "a b#c".to_owned(),
                raw_json: false,
                owner: None,
            },
        ];
        let bytes = render(FileKind::Env, Some(original), &assignments).expect("render");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("# note"));
        assert!(text.contains("export KEEP=1"));
        assert_eq!(
            get(FileKind::Env, &text, "GEMINI_API_KEY").expect("get"),
            Some("local-token".to_owned())
        );
        assert_eq!(
            get(FileKind::Env, &text, "ODD").expect("get"),
            Some("a b#c".to_owned())
        );
        let again = render(FileKind::Env, Some(&text), &assignments).expect("again");
        assert_eq!(again, text.into_bytes());
    }

    #[test]
    fn toml_removal_keeps_a_key_added_beside_ours() {
        let original = "\
# keep me
model = \"gpt-5\"
model_provider = \"hofund\"
sandbox_mode = \"workspace-write\"

[model_providers.hofund]
name = \"hofund\"
base_url = \"http://127.0.0.1:3425/v1\"
wire_api = \"responses\"
experimental_bearer_token = \"local-token\"
";
        let keys = [
            "model_provider",
            "model_providers.hofund.name",
            "model_providers.hofund.base_url",
            "model_providers.hofund.wire_api",
            "model_providers.hofund.experimental_bearer_token",
        ];
        let bytes = remove_keys(FileKind::Toml, original, &keys).expect("remove");
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("# keep me"));
        assert!(text.contains("model = \"gpt-5\""));
        assert!(text.contains("sandbox_mode = \"workspace-write\""));
        assert!(!text.contains("hofund"));
        assert!(!text.contains("local-token"));
        assert!(!is_empty_document(FileKind::Toml, text.as_bytes()).expect("empty"));
    }

    #[test]
    fn json_restores_a_string_model_after_a_user_edit() {
        let original = r#"{
  "model": "user-model",
  "ui": { "theme": "dark" }
}
"#;
        let connect = [
            Assignment {
                key: "security.auth.selectedType",
                value: "gemini-api-key".to_owned(),
                raw_json: false,
                owner: None,
            },
            Assignment {
                key: "model.name",
                value: "user-model".to_owned(),
                raw_json: false,
                owner: None,
            },
        ];
        let connected = render(FileKind::Json, Some(original), &connect).expect("connect");
        let connected = String::from_utf8(connected).expect("connected UTF-8");
        let mut edited: serde_json::Value =
            serde_json::from_str(&connected).expect("connected JSON");
        assert_eq!(edited["model"]["name"], "user-model");
        edited
            .as_object_mut()
            .expect("JSON root")
            .insert("user_added".to_owned(), serde_json::Value::from("keep"));
        let edited = serde_json::to_string(&edited).expect("edited JSON");

        let original_model = get(FileKind::Json, original, "model.name")
            .expect("original model")
            .expect("string model");
        let restore = [Assignment {
            key: "model.name",
            value: original_model,
            raw_json: true,
            owner: None,
        }];
        let restored = render(FileKind::Json, Some(&edited), &restore).expect("restore");
        let restored = String::from_utf8(restored).expect("restored UTF-8");
        let disconnected = remove_keys(FileKind::Json, &restored, &["security.auth.selectedType"])
            .expect("disconnect");
        let disconnected: serde_json::Value =
            serde_json::from_slice(&disconnected).expect("disconnected JSON");
        assert_eq!(disconnected["model"], "user-model");
        assert_eq!(disconnected["user_added"], "keep");
        assert_eq!(disconnected["ui"]["theme"], "dark");
        assert!(disconnected.get("security").is_none());
    }

    #[test]
    fn json_singleton_object_model_keeps_its_shape_through_restore() {
        let original = r#"{
  "model": { "name": "user-model" },
  "ui": { "theme": "dark" }
}
"#;
        let connect = [Assignment {
            key: "model.name",
            value: "hofund-model".to_owned(),
            raw_json: false,
            owner: None,
        }];
        let connected = render(FileKind::Json, Some(original), &connect).expect("connect");
        let connected = String::from_utf8(connected).expect("connected UTF-8");
        let connected_json: serde_json::Value =
            serde_json::from_str(&connected).expect("connected JSON");
        assert_eq!(connected_json["model"]["name"], "hofund-model");
        assert!(connected_json["model"].is_object());
        let second = render(FileKind::Json, Some(&connected), &connect).expect("second apply");
        assert_eq!(second, connected.as_bytes());

        let mut edited = connected_json;
        edited
            .as_object_mut()
            .expect("JSON root")
            .insert("user_added".to_owned(), serde_json::Value::from("keep"));
        let edited = serde_json::to_string(&edited).expect("edited JSON");
        let original_model = get(FileKind::Json, original, "model.name")
            .expect("original model")
            .expect("object model name");
        assert_eq!(original_model, "user-model");
        let restore = [Assignment {
            key: "model.name",
            value: original_model,
            raw_json: false,
            owner: None,
        }];
        let restored = render(FileKind::Json, Some(&edited), &restore).expect("restore");
        let restored: serde_json::Value = serde_json::from_slice(&restored).expect("restored JSON");
        assert_eq!(
            restored["model"],
            serde_json::json!({ "name": "user-model" })
        );
        assert_eq!(restored["user_added"], "keep");
        assert_eq!(restored["ui"]["theme"], "dark");
    }

    #[test]
    fn json_model_shape_error_names_the_key_and_repair() {
        let error = get(FileKind::Json, r#"{"model": 42}"#, "model.name")
            .expect_err("invalid model shape")
            .to_string();
        assert!(
            error.contains("`model` must be a string or an object"),
            "{error}"
        );
        assert!(error.contains("restore `model`"), "{error}");
        assert!(!error.contains("`name` is not an object"), "{error}");
    }

    #[test]
    fn json_model_name_rejects_a_non_string_name() {
        let error = get(FileKind::Json, r#"{"model": {"name": 42}}"#, "model.name")
            .expect_err("invalid model name")
            .to_string();
        assert!(error.contains("`model.name` must be a string"), "{error}");
        assert!(error.contains("restore `model.name`"), "{error}");
    }

    #[test]
    fn json_string_model_restore_rejects_extra_model_keys() {
        let original = r#"{"model": "user-model"}"#;
        let current = r#"{"model": {"name": "user-model", "temperature": 0.2}}"#;
        let original_model = get(FileKind::Json, original, "model.name")
            .expect("original model")
            .expect("string model");
        let restore = [Assignment {
            key: "model.name",
            value: original_model,
            raw_json: true,
            owner: None,
        }];
        let error = render(FileKind::Json, Some(current), &restore)
            .expect_err("extra model key")
            .to_string();
        assert_eq!(
            error,
            "`model` must contain only a string `name` to restore the original string; move other keys out of `model` before disconnect"
        );
    }

    #[test]
    fn json_raw_model_name_object_is_not_a_string_model_restore() {
        let original = r#"{"model": {"name": "old-model"}}"#;
        let restore = [Assignment {
            key: "model.name",
            value: r#"{"name":"x","extra":1}"#.to_owned(),
            raw_json: true,
            owner: None,
        }];
        let rendered = render(FileKind::Json, Some(original), &restore).expect("render");
        let rendered: serde_json::Value = serde_json::from_slice(&rendered).expect("rendered JSON");
        assert_eq!(
            rendered["model"],
            serde_json::json!({"name": {"name": "x", "extra": 1}})
        );
    }
}
