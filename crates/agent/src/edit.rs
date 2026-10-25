//! JSON, TOML, and env-file edits that keep unrelated keys.

use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item};

use crate::AgentError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    Json,
    Toml,
    Env,
}

pub(crate) struct Assignment {
    pub key: &'static str,
    pub value: String,
    /// When set, `value` is JSON text and is written as that value, not as a string.
    pub raw_json: bool,
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
    };
    Ok(text.into_bytes())
}

pub(crate) fn get(kind: FileKind, text: &str, key: &str) -> Result<Option<String>, AgentError> {
    match kind {
        FileKind::Json => json_get(&json_root(text)?, key),
        FileKind::Toml => Ok(toml_get(&parse_toml(text)?, key)),
        FileKind::Env => Ok(env_get(text, key)),
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
    };
    Ok(text.into_bytes())
}

/// Whether `bytes` has no keys left. Comments alone count as empty.
pub(crate) fn is_empty_document(kind: FileKind, bytes: &[u8]) -> Result<bool, AgentError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AgentError::message("file is not UTF-8".to_owned()))?;
    match kind {
        FileKind::Json => Ok(json_root(text)?
            .as_object()
            .is_some_and(|object| object.is_empty())),
        FileKind::Toml => Ok(parse_toml(text)?.as_table().is_empty()),
        FileKind::Env => Ok(!split_lines(text)
            .iter()
            .any(|line| parse_assignment(line).is_some())),
    }
}

fn render_json(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    let mut root = json_root(original)?;
    if assignments
        .iter()
        .any(|assignment| assignment.key == "model.name")
    {
        promote_string_model(&mut root);
    }
    for assignment in assignments {
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
        unset_json(&mut root, &split_key(key))?;
    }
    pretty_json(&root)
}

fn promote_string_model(root: &mut Value) {
    let Some(object) = root.as_object_mut() else {
        return;
    };
    if object.get("model").is_some_and(Value::is_string) {
        object.remove("model");
    }
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
    use super::{Assignment, FileKind, get, is_empty_document, remove_keys, render};

    #[test]
    fn json_keeps_unrelated_keys_and_accepts_jsonc() {
        let original = "{\n  // note\n  \"ui\": { \"theme\": \"dark\" },\n}\n";
        let assignments = [Assignment {
            key: "env.ANTHROPIC_BASE_URL",
            value: "http://127.0.0.1:3425".to_owned(),
            raw_json: false,
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
            },
            Assignment {
                key: "GOOGLE_GEMINI_BASE_URL",
                value: "http://127.0.0.1:3425".to_owned(),
                raw_json: false,
            },
            Assignment {
                key: "ODD",
                value: "a b#c".to_owned(),
                raw_json: false,
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
}
