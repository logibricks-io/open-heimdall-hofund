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

fn render_json(original: &str, assignments: &[Assignment]) -> Result<String, AgentError> {
    let mut root = json_root(original)?;
    for assignment in assignments {
        set_json(&mut root, &split_key(assignment.key), &assignment.value)?;
    }
    let mut bytes = serde_json::to_vec_pretty(&root)
        .map_err(|error| AgentError::message(format!("JSON: {error}")))?;
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    String::from_utf8(bytes).map_err(|error| AgentError::message(format!("JSON: {error}")))
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

fn json_root(text: &str) -> Result<Value, AgentError> {
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

fn set_json(value: &mut Value, path: &[&str], text: &str) -> Result<(), AgentError> {
    let Some((key, rest)) = path.split_first() else {
        return Err(AgentError::message("empty JSON key"));
    };
    if !value.is_object() {
        return Err(AgentError::message("JSON value is not an object"));
    }
    let object = value.as_object_mut().expect("checked object");
    if rest.is_empty() {
        object.insert((*key).to_owned(), Value::String(text.to_owned()));
        return Ok(());
    }
    if let Some(child) = object.get(*key) {
        if !child.is_object() {
            return Err(AgentError::message(format!("`{key}` is not an object")));
        }
    } else {
        object.insert((*key).to_owned(), Value::Object(Map::new()));
    }
    set_json(object.get_mut(*key).expect("child"), rest, text)
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
    use super::{Assignment, FileKind, get, render};

    #[test]
    fn json_keeps_unrelated_keys_and_accepts_jsonc() {
        let original = "{\n  // note\n  \"ui\": { \"theme\": \"dark\" },\n}\n";
        let assignments = [Assignment {
            key: "env.ANTHROPIC_BASE_URL",
            value: "http://127.0.0.1:3425".to_owned(),
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
            },
            Assignment {
                key: "GOOGLE_GEMINI_BASE_URL",
                value: "http://127.0.0.1:3425".to_owned(),
            },
            Assignment {
                key: "ODD",
                value: "a b#c".to_owned(),
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
}
