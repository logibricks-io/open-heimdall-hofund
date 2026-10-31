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
    /// When set, `value` holds only hofund's entries, merged into the array
    /// or map at `key`: hofund's existing entries are replaced, others kept.
    pub shared: Option<Shared>,
}

/// How hofund's entries are told apart in a value shared with the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shared {
    /// An array: `value` is a JSON array, and an entry is hofund's when it is
    /// an object passing every test.
    Array(&'static [OwnerTest]),
    /// A map: `value` is a JSON object, and an entry is hofund's when its
    /// name starts with the prefix.
    Map(&'static str),
}

impl Shared {
    /// The value holding none of hofund's entries, as JSON text.
    pub(crate) const fn empty(self) -> &'static str {
        match self {
            Self::Array(_) => "[]",
            Self::Map(_) => "{}",
        }
    }
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
        FileKind::Toml => Ok(toml_string(&parse_toml(text)?, key)),
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

/// The array or map at `key` without hofund's entries, as JSON text.
///
/// `None` when the key is absent. Env files hold neither; TOML files hold
/// maps only.
pub(crate) fn strip_owned(
    kind: FileKind,
    text: &str,
    key: &str,
    shared: Shared,
) -> Result<Option<String>, AgentError> {
    split_owned(kind, text, key, shared, false)
}

/// hofund's entries of the array or map at `key`, as JSON text.
///
/// `None` when the key is absent. File kinds as for [`strip_owned`].
pub(crate) fn owned_part(
    kind: FileKind,
    text: &str,
    key: &str,
    shared: Shared,
) -> Result<Option<String>, AgentError> {
    split_owned(kind, text, key, shared, true)
}

/// The user's entries, or hofund's when `ours`, of a shared value.
fn split_owned(
    kind: FileKind,
    text: &str,
    key: &str,
    shared: Shared,
    ours: bool,
) -> Result<Option<String>, AgentError> {
    let path = split_key(key);
    let existing = match (kind, shared) {
        (FileKind::Json, _) => json_child(&json_root(text)?, &path).cloned(),
        (FileKind::Yaml, _) => yaml_child(&yaml_root(text)?, &path).map(yaml_to_json),
        (FileKind::Toml, Shared::Map(_)) => {
            toml_child(parse_toml(text)?.as_item(), &path).map(toml_to_json)
        }
        (FileKind::Toml, Shared::Array(_)) | (FileKind::Env, _) => {
            return Err(AgentError::message(format!(
                "`{key}` cannot hold shared entries in this file kind"
            )));
        }
    };
    let kept = match shared {
        Shared::Array(owner) => {
            kept_entries(existing.as_ref(), path.is_empty(), key, owner, ours)?.map(Value::Array)
        }
        Shared::Map(prefix) => kept_map(existing.as_ref(), key, prefix, ours)?.map(Value::Object),
    };
    Ok(kept.map(|kept| kept.to_string()))
}

/// Whether JSON text is an empty array or object.
pub(crate) fn is_empty_json(text: &str) -> bool {
    matches!(text.trim(), "[]" | "{}")
}

/// The user's entries of an existing array value, or hofund's when `ours`,
/// in order.
///
/// `None` when there is no array yet. An empty object root counts as no
/// array, so a missing root-array file can be created.
fn kept_entries(
    existing: Option<&Value>,
    root: bool,
    key: &str,
    owner: &[OwnerTest],
    ours: bool,
) -> Result<Option<Vec<Value>>, AgentError> {
    match existing {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(object)) if root && object.is_empty() => Ok(None),
        Some(Value::Array(items)) => Ok(Some(
            items
                .iter()
                .filter(|item| owned(item, owner) == ours)
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
    let kept = kept_entries(existing, root, assignment.key, owner, false)?;
    if kept.is_none() && ours.is_empty() {
        return Ok(None);
    }
    let mut items = kept.unwrap_or_default();
    items.extend(ours);
    Ok(Some(Value::Array(items)))
}

/// The user's entries of an existing map value, or hofund's when `ours`, in
/// order.
///
/// `None` when there is no map yet.
fn kept_map(
    existing: Option<&Value>,
    key: &str,
    prefix: &str,
    ours: bool,
) -> Result<Option<Map<String, Value>>, AgentError> {
    match existing {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(object)) => Ok(Some(
            object
                .iter()
                .filter(|(name, _)| name.starts_with(prefix) == ours)
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        )),
        Some(_) => Err(AgentError::message(format!("`{key}` is not a map"))),
    }
}

/// hofund's map entries from an assignment's JSON text.
fn our_entries(assignment: &Assignment) -> Result<Map<String, Value>, AgentError> {
    let ours: Value = serde_json::from_str(&assignment.value).map_err(|error| {
        AgentError::message(format!("`{}` is not JSON: {error}", assignment.key))
    })?;
    match ours {
        Value::Object(ours) => Ok(ours),
        _ => Err(AgentError::message(format!(
            "`{}` entries are not an object",
            assignment.key
        ))),
    }
}

/// The user's map entries followed by hofund's, or `None` to leave the key
/// out.
fn merged_map(
    existing: Option<&Value>,
    assignment: &Assignment,
    prefix: &str,
) -> Result<Option<Value>, AgentError> {
    let ours = our_entries(assignment)?;
    let kept = kept_map(existing, assignment.key, prefix, false)?;
    if kept.is_none() && ours.is_empty() {
        return Ok(None);
    }
    let mut entries = kept.unwrap_or_default();
    entries.extend(ours);
    Ok(Some(Value::Object(entries)))
}

/// Merges hofund's entries into an existing JSON or YAML value.
fn merged_shared(
    existing: Option<&Value>,
    root: bool,
    assignment: &Assignment,
    shared: Shared,
) -> Result<Option<Value>, AgentError> {
    match shared {
        Shared::Array(owner) => merged(existing, root, assignment, owner),
        Shared::Map(prefix) => merged_map(existing, assignment, prefix),
    }
}

fn json_child<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cursor = value;
    for part in path {
        cursor = cursor.as_object()?.get(*part)?;
    }
    Some(cursor)
}

fn render_json(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    let before = json_root(original)?;
    let root = json_rendered_value(before.clone(), assignments)?;
    if !original.trim().is_empty()
        && let Some(text) = crate::jsonc_edit::reconcile(original, &before, &root)
    {
        return Ok(text);
    }
    // A full render is only for an empty file or input the scanner cannot parse.
    pretty_json(&root)
}

#[cfg(test)]
fn legacy_render_json(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    pretty_json(&json_rendered_value(json_root(original)?, assignments)?)
}

fn json_rendered_value(mut root: Value, assignments: &[Assignment]) -> Result<Value, AgentError> {
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
        if let Some(shared) = assignment.shared {
            let existing = json_child(&root, &path);
            let Some(value) = merged_shared(existing, path.is_empty(), assignment, shared)? else {
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
    Ok(root)
}

fn render_toml(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    let mut document = parse_toml(original)?;
    for assignment in assignments {
        let path = split_key(assignment.key);
        match assignment.shared {
            Some(Shared::Map(prefix)) => {
                let ours = our_entries(assignment)?;
                merge_toml_map(document.as_item_mut(), &path, prefix, &ours)?;
            }
            Some(Shared::Array(_)) => {
                return Err(AgentError::message(format!(
                    "`{}` cannot hold a shared array in a TOML file",
                    assignment.key
                )));
            }
            None if assignment.raw_json => {
                let value: Value = serde_json::from_str(&assignment.value).map_err(|error| {
                    AgentError::message(format!("`{}` is not JSON: {error}", assignment.key))
                })?;
                if path.is_empty() {
                    let Item::Table(table) = json_to_toml_item(&value) else {
                        return Err(AgentError::message("root is not a table".to_owned()));
                    };
                    *document.as_table_mut() = table;
                } else {
                    set_toml_item(document.as_item_mut(), &path, json_to_toml_item(&value))?;
                }
            }
            None => set_toml(document.as_item_mut(), &path, &assignment.value)?,
        }
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
    let before = json_root(original)?;
    let mut root = before.clone();
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
    if let Some(text) = crate::jsonc_edit::reconcile(original, &before, &root) {
        return Ok(text);
    }
    // A full render is the safe fallback when the scanner cannot parse the file.
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

pub(crate) fn pretty_json(root: &Value) -> Result<String, AgentError> {
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
    let before = yaml_root(original)?;
    let root = yaml_rendered_value(before.clone(), assignments)?;
    if !original.trim().is_empty()
        && let Some(text) = crate::yaml_edit::reconcile(original, &before, &root)
    {
        return Ok(text);
    }
    // The line editor defers touched flow collections, tags, and aliases here.
    emit_yaml(&root)
}

#[cfg(test)]
fn legacy_render_yaml(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    emit_yaml(&yaml_rendered_value(yaml_root(original)?, assignments)?)
}

fn yaml_rendered_value(mut root: Yaml, assignments: &[Assignment]) -> Result<Yaml, AgentError> {
    for assignment in assignments {
        let path = split_key(assignment.key);
        if let Some(shared) = assignment.shared {
            let existing = yaml_child(&root, &path).map(yaml_to_json);
            let Some(value) =
                merged_shared(existing.as_ref(), path.is_empty(), assignment, shared)?
            else {
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
    Ok(root)
}

fn remove_yaml(original: &str, keys: &[&str]) -> Result<String, AgentError> {
    let before = yaml_root(original)?;
    let mut root = before.clone();
    for key in keys {
        unset_yaml(&mut root, &split_key(key))?;
    }
    if let Some(text) = crate::yaml_edit::reconcile(original, &before, &root) {
        return Ok(text);
    }
    emit_yaml(&root)
}

/// Parses one YAML document. An empty file is an empty mapping.
pub(crate) fn yaml_root(text: &str) -> Result<Yaml, AgentError> {
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

pub(crate) fn emit_yaml(root: &Yaml) -> Result<String, AgentError> {
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

pub(crate) fn yaml_key(key: &str) -> Yaml {
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

pub(crate) fn set_yaml(value: &mut Yaml, path: &[&str], written: Yaml) -> Result<(), AgentError> {
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

pub(crate) fn unset_yaml(value: &mut Yaml, path: &[&str]) -> Result<(), AgentError> {
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

/// The string at `key`, when that value is a string.
fn toml_string(document: &DocumentMut, key: &str) -> Option<String> {
    lookup(document.as_item(), &split_key(key)).map(ToOwned::to_owned)
}

/// The value at `key`: a string as itself, anything else as JSON text.
fn toml_get(document: &DocumentMut, key: &str) -> Option<String> {
    let item = toml_child(document.as_item(), &split_key(key))?;
    if let Some(text) = item.as_str() {
        return Some(text.to_owned());
    }
    let value = toml_to_json(item);
    (!value.is_null()).then(|| value.to_string())
}

fn toml_child<'a>(item: &'a Item, path: &[&str]) -> Option<&'a Item> {
    let mut cursor = item;
    for part in path {
        cursor = cursor.as_table_like()?.get(part)?;
    }
    Some(cursor)
}

fn toml_to_json(item: &Item) -> Value {
    match item {
        Item::None => Value::Null,
        Item::Value(value) => toml_value_to_json(value),
        Item::Table(table) => Value::Object(
            table
                .iter()
                .map(|(key, item)| (key.to_owned(), toml_to_json(item)))
                .collect(),
        ),
        Item::ArrayOfTables(tables) => Value::Array(
            tables
                .iter()
                .map(|table| {
                    Value::Object(
                        table
                            .iter()
                            .map(|(key, item)| (key.to_owned(), toml_to_json(item)))
                            .collect(),
                    )
                })
                .collect(),
        ),
    }
}

fn toml_value_to_json(value: &toml_edit::Value) -> Value {
    match value {
        toml_edit::Value::String(text) => Value::String(text.value().clone()),
        toml_edit::Value::Integer(number) => Value::from(*number.value()),
        toml_edit::Value::Float(number) => {
            serde_json::Number::from_f64(*number.value()).map_or(Value::Null, Value::Number)
        }
        toml_edit::Value::Boolean(flag) => Value::Bool(*flag.value()),
        toml_edit::Value::Datetime(time) => Value::String(time.value().to_string()),
        toml_edit::Value::Array(items) => {
            Value::Array(items.iter().map(toml_value_to_json).collect())
        }
        toml_edit::Value::InlineTable(table) => Value::Object(
            table
                .iter()
                .map(|(key, value)| (key.to_owned(), toml_value_to_json(value)))
                .collect(),
        ),
    }
}

/// A JSON value as a TOML item: objects become tables, a table holding only
/// tables stays implicit, and null becomes no item.
fn json_to_toml_item(value: &Value) -> Item {
    match value {
        Value::Object(object) => {
            let mut table = toml_edit::Table::new();
            // A null entry becomes `Item::None`, which TOML leaves out.
            for (key, value) in object {
                table.insert(key, json_to_toml_item(value));
            }
            // Only an empty table needs its own header.
            table.set_implicit(!table.is_empty());
            Item::Table(table)
        }
        Value::Null => Item::None,
        other => Item::Value(json_to_toml_value(other)),
    }
}

/// A JSON value as an inline TOML value. Null becomes an empty string, since
/// TOML has no null.
fn json_to_toml_value(value: &Value) -> toml_edit::Value {
    match value {
        Value::Null => toml_edit::Value::from(""),
        Value::Bool(flag) => toml_edit::Value::from(*flag),
        Value::Number(number) => number.as_i64().map_or_else(
            || toml_edit::Value::from(number.as_f64().unwrap_or_default()),
            toml_edit::Value::from,
        ),
        Value::String(text) => toml_edit::Value::from(text.as_str()),
        Value::Array(items) => {
            toml_edit::Value::Array(items.iter().map(json_to_toml_value).collect())
        }
        Value::Object(object) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, value) in object {
                if !value.is_null() {
                    table.insert(key, json_to_toml_value(value));
                }
            }
            toml_edit::Value::InlineTable(table)
        }
    }
}

/// Drops the entries of the table at `path` whose names start with `prefix`
/// and adds `ours` after the rest. A missing table is created only when
/// there is something to add.
fn merge_toml_map(
    item: &mut Item,
    path: &[&str],
    prefix: &str,
    ours: &Map<String, Value>,
) -> Result<(), AgentError> {
    if let Some((key, rest)) = path.split_first() {
        let missing = item
            .as_table_like()
            .ok_or_else(|| AgentError::message(format!("`{key}` is not under a table")))?
            .get(key)
            .is_none();
        if missing {
            // Implicit tables left empty are not written out.
            item[key] = implicit_table();
        }
        let child = item
            .as_table_like_mut()
            .and_then(|table| table.get_mut(key))
            .expect("child");
        if !child.is_table_like() {
            return Err(AgentError::message(format!("`{key}` is not a table")));
        }
        return merge_toml_map(child, rest, prefix, ours);
    }
    let Some(table) = item.as_table_like_mut() else {
        return Err(AgentError::message("the map is not a table".to_owned()));
    };
    let owned: Vec<String> = table
        .iter()
        .map(|(name, _)| name.to_owned())
        .filter(|name| name.starts_with(prefix))
        .collect();
    for name in &owned {
        table.remove(name);
    }
    for (name, value) in ours {
        // An inline parent turns the table into an inline value.
        table.insert(name, json_to_toml_item(value));
    }
    Ok(())
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
        if let Some(prefix) = unset_toml(document.as_item_mut(), &split_key(key))? {
            let trailing = document.trailing().as_str().unwrap_or_default().to_owned();
            document.set_trailing(format!("{prefix}{trailing}"));
        }
    }
    let mut text = document.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

fn unset_toml(item: &mut Item, path: &[&str]) -> Result<Option<String>, AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty TOML key"));
    };
    if item.as_table_like().is_none() {
        return Err(AgentError::message(format!("`{key}` is not a table")));
    }
    if rest.is_empty() {
        return Ok(transfer_toml_prefix(item, key, String::new(), true));
    }
    let present = item
        .as_table_like()
        .and_then(|table| table.get(key))
        .is_some();
    if !present {
        return Ok(None);
    }
    let is_table = item
        .as_table_like()
        .and_then(|table| table.get(key))
        .is_some_and(Item::is_table_like);
    if !is_table {
        return Err(AgentError::message(format!("`{key}` is not a table")));
    }
    let mut prefix = {
        let child = item
            .as_table_like_mut()
            .and_then(|table| table.get_mut(key))
            .expect("child");
        unset_toml(child, rest)?
    };
    let empty = item
        .as_table_like()
        .and_then(|table| table.get(key))
        .and_then(Item::as_table_like)
        .is_some_and(|table| table.is_empty());
    if empty {
        prefix = transfer_toml_prefix(item, key, prefix.take().unwrap_or_default(), true);
    } else if let Some(comment) = prefix.take() {
        prefix = transfer_toml_prefix(item, key, comment, false);
    }
    Ok(prefix)
}

fn transfer_toml_prefix(
    item: &mut Item,
    key: &str,
    prefix: String,
    remove: bool,
) -> Option<String> {
    let table = item.as_table_like_mut()?;
    let own = table
        .get_key_value(key)
        .and_then(|(key, _)| key.leaf_decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default()
        .to_owned();
    let mut found = false;
    let next = table.iter().find_map(|(candidate, _)| {
        let next = found;
        found |= candidate == key;
        (next && candidate != key).then(|| candidate.to_owned())
    });
    if remove {
        table.remove(key);
    }
    let prefix = format!("{own}{prefix}");
    if prefix.is_empty() {
        return None;
    }
    if let Some(next) = next {
        if let Some((key, item)) = table.get_key_value_mut(&next)
            && prepend_toml_prefix(key, item, &prefix)
        {
            return None;
        }
        Some(prefix)
    } else {
        Some(prefix)
    }
}

fn prepend_toml_prefix(mut key: toml_edit::KeyMut<'_>, item: &mut Item, prefix: &str) -> bool {
    if item.as_table().is_some_and(|table| table.is_implicit()) {
        let Some(table) = item.as_table_like_mut() else {
            return false;
        };
        let Some((child_key, child_item)) = table.iter_mut().next() else {
            return false;
        };
        return prepend_toml_prefix(child_key, child_item, prefix);
    }
    let existing = key
        .leaf_decor()
        .prefix()
        .and_then(|prefix| prefix.as_str())
        .unwrap_or_default()
        .to_owned();
    key.leaf_decor_mut()
        .set_prefix(format!("{prefix}{existing}"));
    true
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
        let table = item
            .as_table_like_mut()
            .ok_or_else(|| AgentError::message(format!("`{key}` is not a table")))?;
        if let Some(slot) = table.get_mut(key) {
            *slot = toml_edit::value(text);
        } else {
            table.insert(key, toml_edit::value(text));
        }
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

/// Writes `written` at `path`, creating implicit parent tables.
fn set_toml_item(item: &mut Item, path: &[&str], written: Item) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty TOML key"));
    };
    let inline = item.is_inline_table();
    let Some(table) = item.as_table_like_mut() else {
        return Err(AgentError::message(format!("`{key}` is not under a table")));
    };
    if rest.is_empty() {
        let written = match written {
            Item::Table(_) if inline => {
                return Err(AgentError::message(format!(
                    "`{key}` cannot hold a table inside an inline table"
                )));
            }
            written => written,
        };
        if let Some(slot) = table.get_mut(key) {
            *slot = written;
        } else {
            table.insert(key, written);
        }
        return Ok(());
    }
    if table.get(key).is_none() {
        table.insert(key, implicit_table());
    }
    let child = table.get_mut(key).expect("child");
    if !child.is_table_like() {
        return Err(AgentError::message(format!("`{key}` is not a table")));
    }
    set_toml_item(child, rest, written)
}

fn implicit_table() -> Item {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    Item::Table(table)
}

/// The parts of a dotted key. A part in double quotes is taken whole, dots
/// included, so `"chat.defaultModel"` names one key.
fn split_key(key: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = key;
    while !rest.is_empty() {
        if let Some(quoted) = rest.strip_prefix('"')
            && let Some(end) = quoted.find('"')
        {
            parts.push(&quoted[..end]);
            rest = quoted[end + 1..].trim_start_matches('.');
            continue;
        }
        let (part, tail) = rest.split_once('.').unwrap_or((rest, ""));
        parts.push(part);
        rest = tail;
    }
    parts.retain(|part| !part.is_empty());
    parts
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
        Assignment, FileKind, Shared, get, is_empty_document, owned_part, remove_keys, render,
        string_at, strip_owned,
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
            shared: Some(Shared::Array(OWNER)),
        }
    }

    fn text(kind: FileKind, original: Option<&str>, assignments: &[Assignment]) -> String {
        String::from_utf8(render(kind, original, assignments).expect("render")).expect("utf8")
    }

    #[test]
    fn quoted_key_parts_keep_their_dots() {
        assert_eq!(
            super::split_key("chat.defaultModel"),
            ["chat", "defaultModel"]
        );
        assert_eq!(
            super::split_key("\"chat.defaultModel\""),
            ["chat.defaultModel"]
        );
        assert_eq!(super::split_key("a.\"b.c\".d"), ["a", "b.c", "d"]);
        assert_eq!(super::split_key("."), Vec::<&str>::new());
        let written = text(
            FileKind::Json,
            Some("{\"chat\":{\"x\":1}}"),
            &[Assignment {
                key: "\"chat.defaultModel\"",
                value: "m".to_owned(),
                raw_json: false,
                shared: None,
            }],
        );
        let value: serde_json::Value = serde_json::from_str(&written).expect("json");
        assert_eq!(
            value,
            serde_json::json!({"chat":{"x":1},"chat.defaultModel":"m"})
        );
        assert_eq!(
            super::string_at(FileKind::Json, &written, "\"chat.defaultModel\"").expect("read"),
            Some("m".to_owned())
        );
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
    fn owned_part_keeps_only_hofund_entries() {
        let list = r#"{"list":[{"id":"u"},{"id":"hof/a","kind":"gw"}],"m":{"x":1,"hof/b":2}}"#;
        assert_eq!(
            owned_part(FileKind::Json, list, "list", Shared::Array(OWNER)).expect("array"),
            Some(r#"[{"id":"hof/a","kind":"gw"}]"#.to_owned())
        );
        assert_eq!(
            owned_part(FileKind::Json, list, "m", Shared::Map("hof/")).expect("map"),
            Some(r#"{"hof/b":2}"#.to_owned())
        );
        assert_eq!(
            owned_part(FileKind::Json, list, "absent", Shared::Map("hof/")).expect("absent"),
            None
        );
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
            strip_owned(FileKind::Json, &kept, ".", Shared::Array(OWNER)).expect("strip"),
            Some(r#"[{"id":"u"}]"#.to_owned())
        );
        assert!(is_empty_document(FileKind::Json, b"[]").expect("empty"));
        assert!(!is_empty_document(FileKind::Json, b"[1]").expect("not empty"));
        assert!(is_empty_document(FileKind::Yaml, b"[]\n").expect("empty yaml"));
        assert_eq!(
            text(FileKind::Yaml, Some("- id: u\n"), &[append(".", ours)]),
            "- id: u\n- id: hof/a\n  kind: gw\n"
        );

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
            strip_owned(FileKind::Yaml, &yaml, "list", Shared::Array(OWNER)).expect("strip"),
            Some(r#"[{"id":"u"}]"#.to_owned())
        );
        assert_eq!(
            strip_owned(FileKind::Yaml, "k: 1\n", "list", Shared::Array(OWNER)).expect("absent"),
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
            let error =
                strip_owned(kind, original, key, Shared::Array(OWNER)).expect_err("strip shape");
            assert!(error.to_string().contains("is not an array"), "{error}");
        }
        let error = render(FileKind::Json, None, &[append("list", "{}")]).expect_err("object");
        assert!(error.to_string().contains("not an array"), "{error}");
        let error = render(FileKind::Json, None, &[append("list", "[")]).expect_err("json");
        assert!(error.to_string().contains("is not JSON"), "{error}");
        for kind in [FileKind::Toml, FileKind::Env] {
            let error = strip_owned(kind, "", "list", Shared::Array(OWNER)).expect_err("kind");
            assert!(
                error.to_string().contains("cannot hold shared entries"),
                "{error}"
            );
        }
        assert_eq!(
            strip_owned(
                FileKind::Json,
                r#"{"l":[{"id":"hof/a","kind":"gw"}]}"#,
                "l",
                Shared::Array(&[])
            )
            .expect("no owner"),
            Some(r#"[{"id":"hof/a","kind":"gw"}]"#.to_owned())
        );
    }

    fn map(key: &'static str, value: &str) -> Assignment {
        Assignment {
            key,
            value: value.to_owned(),
            raw_json: true,
            shared: Some(Shared::Map("hof/")),
        }
    }

    const OURS: &str = r#"{"hof/a.1":{"model":"a.1","size":128000,"tools":["t"]},"hof/b":{"model":"b","on":true}}"#;

    #[test]
    fn map_merges_after_user_entries_in_json_and_yaml() {
        let original = r#"{"m":{"mine":{"x":1},"hof/old":{"model":"old"}},"k":2}"#;
        let once = text(FileKind::Json, Some(original), &[map("m", OURS)]);
        let value: serde_json::Value = serde_json::from_str(&once).expect("json");
        let names: Vec<&String> = value["m"].as_object().expect("map").keys().collect();
        assert_eq!(names, ["mine", "hof/a.1", "hof/b"]);
        assert_eq!(value["m"]["hof/a.1"]["size"], 128_000);
        assert_eq!(value["k"], 2);
        assert_eq!(text(FileKind::Json, Some(&once), &[map("m", OURS)]), once);
        assert_eq!(
            strip_owned(FileKind::Json, &once, "m", Shared::Map("hof/")).expect("strip"),
            Some(r#"{"mine":{"x":1}}"#.to_owned())
        );
        let cleared = text(FileKind::Json, Some(&once), &[map("m", "{}")]);
        assert_eq!(
            get(FileKind::Json, &cleared, "m").expect("get"),
            Some(r#"{"mine":{"x":1}}"#.to_owned())
        );
        assert_eq!(text(FileKind::Json, None, &[map("m", "{}")]), "{}\n");

        let yaml = text(FileKind::Yaml, Some("m:\n  mine: 1\n"), &[map("m", OURS)]);
        assert_eq!(text(FileKind::Yaml, Some(&yaml), &[map("m", OURS)]), yaml);
        assert_eq!(
            strip_owned(FileKind::Yaml, &yaml, "m", Shared::Map("hof/")).expect("strip"),
            Some(r#"{"mine":1}"#.to_owned())
        );
        assert!(yaml.contains("hof/a.1:"), "{yaml}");
        assert_eq!(
            strip_owned(FileKind::Yaml, "k: 1\n", "m", Shared::Map("hof/")).expect("absent"),
            None
        );
        let root = text(FileKind::Json, Some(r#"{"mine":1}"#), &[map(".", OURS)]);
        assert_eq!(
            strip_owned(FileKind::Json, &root, ".", Shared::Map("hof/")).expect("root"),
            Some(r#"{"mine":1}"#.to_owned())
        );
    }

    #[test]
    fn map_writes_typed_toml_tables_and_keeps_the_rest() {
        let original = "# top\ndefault = \"mine\"\n\n[m.mine]\nmodel = \"x\" # keep\n\n[m.\"hof/old\"]\nmodel = \"old\"\n";
        let once = text(FileKind::Toml, Some(original), &[map("m", OURS)]);
        assert_eq!(
            once,
            "# top\ndefault = \"mine\"\n\n[m.mine]\nmodel = \"x\" # keep\n\n[m.\"hof/a.1\"]\nmodel = \"a.1\"\nsize = 128000\ntools = [\"t\"]\n\n[m.\"hof/b\"]\nmodel = \"b\"\non = true\n"
        );
        assert_eq!(text(FileKind::Toml, Some(&once), &[map("m", OURS)]), once);
        assert_eq!(
            get(FileKind::Toml, &once, "m").expect("get"),
            Some(
                r#"{"mine":{"model":"x"},"hof/a.1":{"model":"a.1","size":128000,"tools":["t"]},"hof/b":{"model":"b","on":true}}"#
                    .to_owned()
            )
        );
        assert_eq!(string_at(FileKind::Toml, &once, "m").expect("string"), None);
        assert_eq!(
            strip_owned(FileKind::Toml, &once, "m", Shared::Map("hof/")).expect("strip"),
            Some(r#"{"mine":{"model":"x"}}"#.to_owned())
        );
        let cleared = text(FileKind::Toml, Some(&once), &[map("m", "{}")]);
        assert_eq!(
            cleared,
            "# top\ndefault = \"mine\"\n\n[m.mine]\nmodel = \"x\" # keep\n"
        );
        assert_eq!(text(FileKind::Toml, None, &[map("m", "{}")]), "\n");
        let fresh = text(FileKind::Toml, None, &[map("p.m", OURS)]);
        assert!(fresh.starts_with("[p.m.\"hof/a.1\"]\n"), "{fresh}");
        let inline = text(
            FileKind::Toml,
            Some("m = { mine = 1 }\n"),
            &[map("m", OURS)],
        );
        assert_eq!(
            inline,
            "m = { mine = 1 , \"hof/a.1\" = { model = \"a.1\", size = 128000, tools = [\"t\"] }, \"hof/b\" = { model = \"b\", on = true } }\n"
        );
        let error = render(FileKind::Toml, Some("m = 1\n"), &[map("m", OURS)]).expect_err("shape");
        assert!(error.to_string().contains("`m` is not a table"), "{error}");
        let error = render(FileKind::Toml, None, &[map("m", "[]")]).expect_err("entries");
        assert!(error.to_string().contains("not an object"), "{error}");
        let error = render(FileKind::Toml, None, &[append("m", "[]")]).expect_err("array in toml");
        assert!(error.to_string().contains("shared array"), "{error}");
        let error =
            render(FileKind::Json, Some(r#"{"m":[]}"#), &[map("m", OURS)]).expect_err("json shape");
        assert!(error.to_string().contains("`m` is not a map"), "{error}");
    }

    #[test]
    fn toml_raw_json_keeps_types_and_reads_back_as_json() {
        let written = text(
            FileKind::Toml,
            Some("a = 1\n"),
            &[
                assign("t.n", "5", true),
                assign("t.f", "1.5", true),
                assign("t.l", r#"[1,{"k":"v"}]"#, true),
                assign("t.o", r#"{"x":{"y":"z"},"gone":null}"#, true),
                assign("t.s", "plain", false),
            ],
        );
        assert_eq!(
            written,
            "a = 1\n\n[t]\nn = 5\nf = 1.5\nl = [1, { k = \"v\" }]\ns = \"plain\"\n\n[t.o.x]\ny = \"z\"\n"
        );
        assert_eq!(
            get(FileKind::Toml, &written, "t.o").expect("get"),
            Some(r#"{"x":{"y":"z"}}"#.to_owned())
        );
        assert_eq!(
            get(FileKind::Toml, &written, "t.n").expect("get"),
            Some("5".to_owned())
        );
        assert_eq!(
            get(FileKind::Toml, &written, "t.s").expect("get"),
            Some("plain".to_owned())
        );
        let root = text(
            FileKind::Toml,
            Some("a = 1\n"),
            &[assign(".", r#"{"b":true}"#, true)],
        );
        assert_eq!(root, "b = true\n");
        let error = render(FileKind::Toml, None, &[assign(".", "3", true)]).expect_err("root");
        assert!(error.to_string().contains("root is not a table"), "{error}");
        let error = render(
            FileKind::Toml,
            Some("t = { a = 1 }\n"),
            &[assign("t.o", r#"{"x":1}"#, true)],
        )
        .expect_err("inline");
        assert!(
            error.to_string().contains("inside an inline table"),
            "{error}"
        );
        let empty = text(FileKind::Toml, None, &[assign("e", "{}", true)]);
        assert_eq!(empty, "[e]\n");
        let nulls = text(
            FileKind::Toml,
            None,
            &[assign("l", r#"[{"k":null,"v":1}]"#, true)],
        );
        assert_eq!(nulls, "l = [{ v = 1 }]\n");
        let special = "n = nan\n[[arr]]\nx = 1\n[[arr]]\ny = true\n";
        assert_eq!(get(FileKind::Toml, special, "n").expect("nan"), None);
        assert_eq!(
            get(FileKind::Toml, special, "arr").expect("array of tables"),
            Some(r#"[{"x":1},{"y":true}]"#.to_owned())
        );
        let inline = text(
            FileKind::Toml,
            Some("t = { a = 1 }\n"),
            &[assign("t.l", "[2]", true)],
        );
        assert_eq!(inline, "t = { a = 1 , l = [2] }\n");
        let error = render(
            FileKind::Toml,
            Some("t = 1\n"),
            &[assign("t.l", "[2]", true)],
        )
        .expect_err("leaf");
        assert!(error.to_string().contains("`t` is not a table"), "{error}");
        let error = render(FileKind::Toml, None, &[assign("t", "{", true)]).expect_err("json");
        assert!(error.to_string().contains("is not JSON"), "{error}");
    }

    fn assign(key: &'static str, value: &str, raw_json: bool) -> Assignment {
        Assignment {
            key,
            value: value.to_owned(),
            raw_json,
            shared: None,
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
                "# a comment",
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
            shared: None,
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
            shared: None,
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
    fn toml_comment_above_removed_key_is_kept() {
        let original = "# model choice\ndefault_model = \"mine\"\ndefault_thinking = true\n";
        let cleared = remove_keys(FileKind::Toml, original, &["default_model"]).expect("clear");
        let cleared = String::from_utf8(cleared).expect("UTF-8");
        assert_eq!(cleared, "# model choice\ndefault_thinking = true\n");

        let last = remove_keys(
            FileKind::Toml,
            "# only model\nmodel = \"mine\"\n",
            &["model"],
        )
        .expect("clear last");
        assert_eq!(String::from_utf8(last).expect("UTF-8"), "# only model\n");

        let table = remove_keys(
            FileKind::Toml,
            "# model choice\ndefault_model = \"mine\"\n\n# mine\n[providers.mine]\ntype = \"kimi\"\n",
            &["default_model"],
        )
        .expect("clear before a table");
        assert_eq!(
            String::from_utf8(table).expect("UTF-8"),
            "# model choice\n\n# mine\n[providers.mine]\ntype = \"kimi\"\n"
        );
    }

    #[test]
    fn toml_restore_after_clear_keeps_the_comment() {
        let original = "# model choice\ndefault_model = \"mine\"\nkeep = true\n";
        let cleared = remove_keys(FileKind::Toml, original, &["default_model"]).expect("clear");
        let cleared = String::from_utf8(cleared).expect("UTF-8");
        let restored = text(
            FileKind::Toml,
            Some(&cleared),
            &[assign("default_model", "mine", false)],
        );
        assert!(restored.contains("# model choice"), "{restored}");
        assert_eq!(
            get(FileKind::Toml, &restored, "default_model").expect("model"),
            Some("mine".to_owned())
        );
        assert!(restored.contains("keep = true"));
    }

    #[test]
    fn env_preserves_comments_and_quotes_a_hash() {
        let original = "# note\nexport KEEP=1\nGEMINI_API_KEY=old\n";
        let assignments = [
            Assignment {
                key: "GEMINI_API_KEY",
                value: "local-token".to_owned(),
                raw_json: false,
                shared: None,
            },
            Assignment {
                key: "GOOGLE_GEMINI_BASE_URL",
                value: "http://127.0.0.1:3425".to_owned(),
                raw_json: false,
                shared: None,
            },
            Assignment {
                key: "ODD",
                value: "a b#c".to_owned(),
                raw_json: false,
                shared: None,
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
                shared: None,
            },
            Assignment {
                key: "model.name",
                value: "user-model".to_owned(),
                raw_json: false,
                shared: None,
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
            shared: None,
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
            shared: None,
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
            shared: None,
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
            shared: None,
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
            shared: None,
        }];
        let rendered = render(FileKind::Json, Some(original), &restore).expect("render");
        let rendered: serde_json::Value = serde_json::from_slice(&rendered).expect("rendered JSON");
        assert_eq!(
            rendered["model"],
            serde_json::json!({"name": {"name": "x", "extra": 1}})
        );
    }

    fn agent_fixture_files(extension: &str) -> Vec<std::path::PathBuf> {
        fn collect(
            directory: &std::path::Path,
            extension: &str,
            files: &mut Vec<std::path::PathBuf>,
        ) {
            for entry in std::fs::read_dir(directory).expect("fixture directory") {
                let path = entry.expect("fixture entry").path();
                if path.is_dir() {
                    collect(&path, extension, files);
                } else if path.extension().is_some_and(|found| found == extension)
                    && path.components().any(|part| part.as_os_str() == "before")
                {
                    files.push(path);
                }
            }
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../plugins/agents");
        let mut files = Vec::new();
        collect(&root, extension, &mut files);
        files
    }

    #[test]
    fn jsonc_fixture_edits_match_legacy_semantics() {
        let mut cases = 0;
        for path in agent_fixture_files("json") {
            let original = std::fs::read_to_string(&path).expect("fixture");
            let Ok(before) = super::json_root(&original) else {
                continue;
            };
            let assignments = if before.is_array() {
                vec![append(".", r#"[{"id":"hof/a","kind":"gw"}]"#)]
            } else {
                vec![assign("hofund_semantic_check", "kept", false)]
            };
            let expected = super::legacy_render_json(&original, &assignments)
                .unwrap_or_else(|error| panic!("legacy {}: {error}", path.display()));
            let actual = text(FileKind::Json, Some(&original), &assignments);
            assert_eq!(
                super::json_root(&actual).expect("new JSON"),
                super::json_root(&expected).expect("legacy JSON"),
                "{}",
                path.display()
            );
            if let Some(key) = before
                .as_object()
                .and_then(|object| object.keys().next().cloned())
            {
                let actual = remove_keys(FileKind::Json, &original, &[&key])
                    .unwrap_or_else(|error| panic!("remove {}: {error}", path.display()));
                let mut after = before.clone();
                super::unset_json(&mut after, &super::split_key(&key)).expect("legacy removal");
                assert_eq!(
                    super::json_root(std::str::from_utf8(&actual).expect("UTF-8"))
                        .expect("new JSON removal"),
                    after,
                    "{}",
                    path.display()
                );
            }
            cases += 1;
        }
        assert!(cases > 20, "expected the shipped JSON fixtures");
    }

    #[test]
    fn yaml_fixture_edits_match_legacy_semantics() {
        let mut cases = 0;
        for path in agent_fixture_files("yaml") {
            let original = std::fs::read_to_string(&path).expect("fixture");
            let Ok(before) = super::yaml_root(&original) else {
                continue;
            };
            let assignments = if before.is_array() {
                vec![append(".", r#"[{"id":"hof/a","kind":"gw"}]"#)]
            } else {
                vec![assign("hofund_semantic_check", "kept", false)]
            };
            let expected = super::legacy_render_yaml(&original, &assignments)
                .unwrap_or_else(|error| panic!("legacy {}: {error}", path.display()));
            let actual = text(FileKind::Yaml, Some(&original), &assignments);
            assert_eq!(
                super::yaml_root(&actual).expect("new YAML"),
                super::yaml_root(&expected).expect("legacy YAML"),
                "{}",
                path.display()
            );
            if let Some(key) = before
                .as_hash()
                .and_then(|hash| hash.keys().find_map(|key| key.as_str().map(str::to_owned)))
            {
                let actual = remove_keys(FileKind::Yaml, &original, &[&key])
                    .unwrap_or_else(|error| panic!("remove {}: {error}", path.display()));
                let mut after = before.clone();
                super::unset_yaml(&mut after, &[&key]).expect("legacy removal");
                assert_eq!(
                    super::yaml_root(std::str::from_utf8(&actual).expect("UTF-8"))
                        .expect("new YAML removal"),
                    after,
                    "{}",
                    path.display()
                );
            }
            cases += 1;
        }
        assert!(cases > 5, "expected the shipped YAML fixtures");
    }
}
