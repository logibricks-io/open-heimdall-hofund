//! Google refresh-token exports from Cockpit, Manager, and `CLIProxyAPI`.

use std::collections::HashSet;
use std::fmt;

use hofund_plugin_api::{PluginError, SecretString};
use serde_json::{Map, Value};

use super::secret_text;

const MAX_ACCOUNTS: usize = 200;

/// One account read from a Google export. Its refresh token is redacted.
pub struct GoogleImport {
    email: String,
    refresh_token: Option<SecretString>,
    project: String,
    error: Option<String>,
}

impl GoogleImport {
    /// The email named by the export, or an empty string.
    #[must_use]
    pub fn email(&self) -> &str {
        &self.email
    }

    /// The imported refresh token, when the entry supplied one.
    #[must_use]
    pub fn refresh_token(&self) -> Option<&SecretString> {
        self.refresh_token.as_ref()
    }

    /// The Cloud project named by the export, or an empty string.
    #[must_use]
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Why this entry cannot be imported, when it is invalid.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl fmt::Debug for GoogleImport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleImport")
            .field("email", &self.email)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("project", &self.project)
            .field("error", &self.error)
            .finish()
    }
}

/// Parses one Google account export for `agent`, whose display name is `app_name`.
///
/// JSON arrays and objects, Manager's nested `token` object, JSON strings, and
/// bare refresh tokens one per line are accepted. Invalid entries are returned
/// with an error so the caller can report them without exposing token material.
///
/// # Errors
///
/// Returns a config error when the input is empty, malformed JSON, has no
/// accounts, or contains more than 200 entries.
pub fn parse_google_import(
    agent: &str,
    app_name: &str,
    data: &str,
) -> Result<Vec<GoogleImport>, PluginError> {
    let text = data.strip_prefix('\u{feff}').unwrap_or(data).trim();
    if text.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    let mut entries = Vec::new();
    if text.starts_with(['[', '{']) {
        let value: Value =
            serde_json::from_str(text).map_err(|_| PluginError::config("not valid JSON"))?;
        walk(&value, agent, app_name, &mut entries);
    } else {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.contains([' ', '\t']) || line.contains("://") {
                entries.push(invalid("not a refresh token"));
            } else {
                entries.push(GoogleImport {
                    email: String::new(),
                    refresh_token: Some(SecretString::from(line.to_owned())),
                    project: String::new(),
                    error: None,
                });
            }
        }
    }
    if entries.is_empty() {
        return Err(PluginError::config("no accounts in it"));
    }
    if entries.len() > MAX_ACCOUNTS {
        return Err(PluginError::config(format!(
            "{} accounts in it; at most {MAX_ACCOUNTS} at a time",
            entries.len()
        )));
    }

    let mut tokens = HashSet::new();
    let mut users = HashSet::new();
    for entry in &mut entries {
        if entry.error.is_some() {
            continue;
        }
        let Some(refresh) = entry.refresh_token.as_ref() else {
            entry.error = Some("no refresh_token in it".to_owned());
            continue;
        };
        let token = secret_text(refresh);
        let user = entry.email.to_lowercase();
        if tokens.contains(token) || (!user.is_empty() && users.contains(&user)) {
            entry.error = Some("in the file twice".to_owned());
            continue;
        }
        tokens.insert(token.to_owned());
        if !user.is_empty() {
            users.insert(user);
        }
    }
    Ok(entries)
}

fn walk(value: &Value, agent: &str, app_name: &str, entries: &mut Vec<GoogleImport>) {
    match value {
        Value::Array(items) => {
            for item in items {
                walk(item, agent, app_name, entries);
            }
        }
        Value::Object(object) => {
            if let Some(accounts) = object.get("accounts").and_then(Value::as_array)
                && json_string(object, &["refresh_token", "refreshToken"]).is_empty()
            {
                for account in accounts {
                    walk(account, agent, app_name, entries);
                }
                return;
            }
            entries.push(read_object(object, agent, app_name));
        }
        Value::String(token) => entries.push(GoogleImport {
            email: String::new(),
            refresh_token: Some(SecretString::from(token.trim().to_owned())),
            project: String::new(),
            error: None,
        }),
        _ => entries.push(invalid("not an account")),
    }
}

fn read_object(object: &Map<String, Value>, agent: &str, app_name: &str) -> GoogleImport {
    let mut entry = GoogleImport {
        email: json_string(object, &["email"]),
        refresh_token: nonempty_secret(json_string(object, &["refresh_token", "refreshToken"])),
        project: json_string(object, &["project_id", "projectId", "project"]),
        error: None,
    };
    if let Some(token) = object.get("token").and_then(Value::as_object) {
        if entry.refresh_token.is_none() {
            entry.refresh_token =
                nonempty_secret(json_string(token, &["refresh_token", "refreshToken"]));
        }
        if entry.email.is_empty() {
            entry.email = json_string(token, &["email"]);
        }
        if entry.project.is_empty() {
            entry.project = json_string(token, &["project_id", "projectId"]);
        }
    }
    let kind = json_string(object, &["type"]);
    if !kind.is_empty() && !kind.eq_ignore_ascii_case(agent) {
        entry.error = Some(format!("a {kind} sign-in, not {app_name}'s"));
    }
    entry
}

fn invalid(reason: &str) -> GoogleImport {
    GoogleImport {
        email: String::new(),
        refresh_token: None,
        project: String::new(),
        error: Some(reason.to_owned()),
    }
}

fn nonempty_secret(value: String) -> Option<SecretString> {
    (!value.is_empty()).then(|| SecretString::from(value))
}

fn json_string(object: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{GoogleImport, parse_google_import};
    use crate::oauth::secret_text;

    fn fields(entry: &GoogleImport) -> (&str, &str, &str, Option<&str>) {
        (
            entry.email(),
            entry.refresh_token().map(secret_text).unwrap_or_default(),
            entry.project(),
            entry.error(),
        )
    }

    #[test]
    fn r49d_reads_every_export_shape_without_rendering_tokens() {
        let cases = [
            (
                r#"[{"email":"a@x.com","refresh_token":"1//a"},{"email":"b@x.com","refresh_token":"1//b"}]"#,
                vec![("a@x.com", "1//a", "", None), ("b@x.com", "1//b", "", None)],
            ),
            (
                r#"{"email":"a@x.com","refreshToken":"1//a","projectId":"p1"}"#,
                vec![("a@x.com", "1//a", "p1", None)],
            ),
            (
                r#"{"accounts":[{"email":"a@x.com","refresh_token":"1//a"}]}"#,
                vec![("a@x.com", "1//a", "", None)],
            ),
            (
                r#"{"id":"u1","email":"a@x.com","token":{"refresh_token":"1//a","email":"nested@x.com","project_id":"proj-a"}}"#,
                vec![("a@x.com", "1//a", "proj-a", None)],
            ),
            (
                r#"{"type":"antigravity","refresh_token":"1//a","email":"a@x.com","project_id":"proj-a"}"#,
                vec![("a@x.com", "1//a", "proj-a", None)],
            ),
            (
                "\u{feff}1//a\n\n# comment\n1//b\r\n",
                vec![("", "1//a", "", None), ("", "1//b", "", None)],
            ),
            (
                r#"["1//a","1//b"]"#,
                vec![("", "1//a", "", None), ("", "1//b", "", None)],
            ),
        ];
        for (text, expected) in cases {
            let parsed = parse_google_import("antigravity", "Antigravity", text).expect("parse");
            let actual: Vec<_> = parsed.iter().map(fields).collect();
            assert_eq!(actual, expected);
        }
        let parsed = parse_google_import("gemini", "Gemini CLI", r#"{"refresh_token":"SECRET"}"#)
            .expect("parse");
        assert!(!format!("{parsed:?}").contains("SECRET"));
    }

    #[test]
    fn r49d_rejects_other_apps_duplicates_and_malformed_entries() {
        let parsed = parse_google_import(
            "antigravity",
            "Antigravity",
            r#"[
                {"type":"gemini","refresh_token":"1//g","email":"g@x.com"},
                {"email":"a@x.com","refresh_token":"1//a"},
                {"email":"A@x.com","refresh_token":"1//z"},
                {"email":"b@x.com","refresh_token":"1//a"},
                {"email":"c@x.com"},
                3
            ]"#,
        )
        .expect("entries");
        let errors: Vec<_> = parsed.iter().map(|entry| entry.error()).collect();
        assert_eq!(
            errors,
            [
                Some("a gemini sign-in, not Antigravity's"),
                None,
                Some("in the file twice"),
                Some("in the file twice"),
                Some("no refresh_token in it"),
                Some("not an account"),
            ]
        );
        assert_eq!(
            parse_google_import("antigravity", "Antigravity", "not a token").expect("entry")[0]
                .error(),
            Some("not a refresh token")
        );
        for bad in ["", "  ", "{not json", "[]"] {
            assert!(parse_google_import("antigravity", "Antigravity", bad).is_err());
        }
        let oversized = format!("[{}]", vec!["\"1//a\""; 201].join(","));
        let error = parse_google_import("antigravity", "Antigravity", &oversized)
            .expect_err("too many accounts");
        assert!(error.message().contains("at most 200"));
    }
}
