//! Importing ChatGPT accounts from external export files.

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::{SecretObject, parse_token_set, token_error};

use crate::sign_in;
use crate::tokens;

const ACCESS_ONLY: &str = "only an access token, no refresh_token: it would stop working within days and can't be renewed; sign in to this account instead";

struct LoginRow {
    email: String,
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: String,
    account_id: Option<String>,
    error: Option<String>,
}

/// Stores every account in a supported import file.
pub(crate) async fn import_accounts(
    host: &HostContext,
    text: &str,
    token_url: &str,
) -> Result<Vec<AccountId>, PluginError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PluginError::config("nothing to import"));
    }
    let Ok(value) = hofund_plugin_sdk::json::from_str::<Value>(trimmed) else {
        return Err(PluginError::unsupported(
            "not an account export this subscription reads",
        ));
    };
    let rows = collect_rows(&value)?;
    if rows.is_empty() {
        return Err(PluginError::unsupported(
            "not an account export this subscription reads",
        ));
    }
    let mut ids = Vec::new();
    let mut seen_refresh = std::collections::HashSet::new();
    for row in rows {
        if let Some(message) = row.error {
            return Err(PluginError::config(message));
        }
        if !seen_refresh.insert(row.refresh_token.clone()) {
            return Err(PluginError::config("in the files twice"));
        }
        ids.push(store_row(host, token_url, row).await?);
    }
    Ok(ids)
}

fn collect_rows(value: &Value) -> Result<Vec<LoginRow>, PluginError> {
    let mut out = Vec::new();
    if let Some(items) = value.as_array() {
        for item in items {
            walk_lenient(item, &mut out);
        }
        return Ok(out);
    }
    walk_strict(value, &mut out)?;
    Ok(out)
}

fn walk_lenient(value: &Value, out: &mut Vec<LoginRow>) {
    let _ = walk_strict(value, out);
}

fn walk_strict(value: &Value, out: &mut Vec<LoginRow>) -> Result<(), PluginError> {
    match value {
        Value::Array(items) => {
            for item in items {
                walk_lenient(item, out);
            }
        }
        Value::Object(map) => {
            if map.get("credentials").and_then(Value::as_object).is_some()
                && !json_str(map, &["platform"]).is_empty()
            {
                out.push(parse_sub2api(map)?);
                return Ok(());
            }
            if map.get("config").and_then(Value::as_object).is_some()
                && map.get("credentials").is_some()
                && !json_str(map, &["email"]).is_empty()
            {
                out.push(LoginRow {
                    error: Some("a Claude account (claude-swap's), not Codex's".to_owned()),
                    ..empty_row()
                });
                return Ok(());
            }
            if let Some(rows) = codexbar_rows(map) {
                out.extend(rows);
                return Ok(());
            }
            if let Some(accounts) = map.get("accounts").and_then(Value::as_array)
                && json_str(map, &["refresh_token", "refreshToken"]).is_empty()
            {
                for entry in accounts {
                    walk_lenient(entry, out);
                }
                return Ok(());
            }
            if let Some(row) = parse_object(map) {
                out.push(row);
            }
        }
        Value::String(line) => {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return Ok(());
            }
            out.push(LoginRow {
                refresh_token: line.to_owned(),
                ..empty_row()
            });
        }
        _ => {}
    }
    Ok(())
}

fn empty_row() -> LoginRow {
    LoginRow {
        email: String::new(),
        id_token: None,
        access_token: None,
        refresh_token: String::new(),
        account_id: None,
        error: None,
    }
}

fn parse_object(map: &Map<String, Value>) -> Option<LoginRow> {
    let mut row = empty_row();
    row.email = json_str(map, &["email"]);
    let mut src = map;
    if let Some(tokens) = map.get("tokens").and_then(Value::as_object) {
        src = tokens;
    }
    row.id_token = non_empty(json_str_opt(src, &["id_token", "idToken"]));
    row.access_token = non_empty(json_str_opt(src, &["access_token", "accessToken"]));
    row.refresh_token = json_str(src, &["refresh_token", "refreshToken"]);
    row.account_id = non_empty(json_str_opt(src, &["account_id", "accountId"]));
    let typ = json_str(map, &["type"]).to_ascii_lowercase();
    if !typ.is_empty() && typ != "codex" {
        row.error = Some(format!("a {typ} sign-in, not Codex's"));
        return Some(row);
    }
    if map.get("claudeAiOauth").is_some()
        || map
            .get("credentials")
            .and_then(Value::as_object)
            .is_some_and(|creds| creds.contains_key("claudeAiOauth"))
    {
        row.error = Some("a Claude account (claude-swap's), not Codex's".to_owned());
        return Some(row);
    }
    if row.refresh_token.is_empty()
        && !json_str(map, &["OPENAI_API_KEY", "api_key", "apiKey"]).is_empty()
    {
        row.error = Some("an API key, not a sign-in; add it as a key instead".to_owned());
        return Some(row);
    }
    if row.refresh_token.is_empty() && row.access_token.is_some() {
        row.error = Some(ACCESS_ONLY.to_owned());
    }
    Some(row)
}

fn parse_sub2api(entry: &Map<String, Value>) -> Result<LoginRow, PluginError> {
    let credentials = entry
        .get("credentials")
        .and_then(Value::as_object)
        .ok_or_else(|| PluginError::config("an account has no credentials"))?;
    let platform = json_str(entry, &["platform"]).to_ascii_lowercase();
    let ours = matches!(platform.as_str(), "openai" | "codex");
    let extra = entry.get("extra").and_then(Value::as_object);
    let mut row = empty_row();
    row.email = cmp_str(
        json_str(credentials, &["email", "email_address"]),
        extra.map(|e| json_str(e, &["email_address", "email"])),
        json_str(entry, &["name"]),
    );
    row.id_token = non_empty(json_str_opt(credentials, &["id_token"]));
    row.access_token = non_empty(json_str_opt(credentials, &["access_token"]));
    row.refresh_token = json_str(credentials, &["refresh_token"]);
    row.account_id = non_empty(json_str_opt(
        credentials,
        &["chatgpt_account_id", "account_id"],
    ));
    let typ = json_str(entry, &["type"]).to_ascii_lowercase();
    if !ours {
        row.error = Some(format!("a {platform} account, not Codex's"));
    } else if typ == "apikey" {
        row.error = Some("an API key, not a sign-in; add it as a key instead".to_owned());
    } else if typ != "oauth" && !typ.is_empty() {
        row.error = Some("not a Codex sign-in".to_owned());
    } else if row.refresh_token.is_empty() && row.access_token.is_some() {
        row.error = Some(ACCESS_ONLY.to_owned());
    }
    Ok(row)
}

fn codexbar_rows(root: &Map<String, Value>) -> Option<Vec<LoginRow>> {
    let providers = root.get("providers")?.as_array()?;
    let mut accounts = Vec::new();
    for provider in providers {
        let Some(provider) = provider.as_object() else {
            continue;
        };
        if json_str(provider, &["kind"]) == "openai_oauth"
            && let Some(list) = provider.get("accounts").and_then(Value::as_array)
        {
            accounts.extend(list.iter().cloned());
        }
    }
    if let Some(openai) = root.get("openAI").and_then(Value::as_object)
        && let Some(list) = openai
            .get("remoteConnectionAccounts")
            .and_then(Value::as_array)
    {
        accounts.extend(list.iter().cloned());
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for account in accounts {
        let Some(account) = account.as_object() else {
            continue;
        };
        let mut row = empty_row();
        row.email = json_str(account, &["email"]);
        row.id_token = non_empty(json_str_opt(account, &["idToken"]));
        row.access_token = non_empty(json_str_opt(account, &["accessToken"]));
        row.refresh_token = json_str(account, &["refreshToken"]);
        row.account_id = non_empty(json_str_opt(account, &["openAIAccountId"]));
        if json_str(account, &["kind"]) == "api_key" {
            row.error = Some("an API key, not a sign-in; add it as a key instead".to_owned());
        } else if row.refresh_token.is_empty() && row.access_token.is_some() {
            row.error = Some(ACCESS_ONLY.to_owned());
        }
        if !row.refresh_token.is_empty() && !seen.insert(row.refresh_token.clone()) {
            continue;
        }
        out.push(row);
    }
    (!out.is_empty()).then_some(out)
}

async fn store_row(
    host: &HostContext,
    token_url: &str,
    row: LoginRow,
) -> Result<AccountId, PluginError> {
    if row.refresh_token.is_empty() {
        return Err(PluginError::config("no refresh_token in it"));
    }
    let body = SecretObject::new()
        .with_text("client_id", crate::CLIENT_ID)
        .with_text("grant_type", "refresh_token")
        .with_secret(
            "refresh_token",
            &SecretString::from(row.refresh_token.clone()),
        )
        .with_text("scope", crate::REFRESH_SCOPE)
        .to_body();
    let reply = Call::post(token_url).json(body).fetch(host.http()).await?;
    if !(200..300).contains(&reply.status()) {
        return Err(token_error(reply.status(), reply.body()));
    }
    let now = host.clock().now();
    let refresh_token = row.refresh_token.clone();
    let set = parse_token_set(reply.body(), now)
        .map_err(|error| PluginError::decode(error.message()))?
        .or_refresh(Some(&SecretString::from(refresh_token.clone())));
    let id_token = set
        .id_token()
        .cloned()
        .or_else(|| row.id_token.map(SecretString::from))
        .ok_or_else(|| PluginError::auth("ChatGPT sent back no ID token"))?;
    let identity = sign_in::identity(&id_token)?;
    if host.accounts().secret(&identity.id).await?.is_some() {
        return Ok(identity.id);
    }
    let account_id = row
        .account_id
        .filter(|value| !value.is_empty())
        .unwrap_or(identity.chatgpt_account.clone());
    let label = disambiguate_label(host, &identity.label, &account_id).await?;
    let secret = SecretObject::new()
        .with_number("v", 1)
        .with_secret("id_token", &id_token)
        .with_secret("access_token", set.access())
        .with_secret(
            "refresh_token",
            set.refresh().unwrap_or(&SecretString::from(refresh_token)),
        )
        .with_text("account_id", &account_id)
        .with_number("last_refresh", tokens::unix_seconds(now))
        .to_secret();
    let account = host.accounts().account(identity.id.clone(), label)?;
    host.accounts().upsert(account, secret).await?;
    Ok(identity.id)
}

async fn disambiguate_label(
    host: &HostContext,
    base: &str,
    workspace: &str,
) -> Result<String, PluginError> {
    let members = host.accounts().list().await?;
    let taken = |name: &str| {
        members
            .iter()
            .any(|member| member.label().eq_ignore_ascii_case(name))
    };
    if !taken(base) {
        return Ok(base.to_owned());
    }
    let short = if workspace.len() > 8 {
        &workspace[..8]
    } else {
        workspace
    };
    let mut candidate = if short.is_empty() {
        base.to_owned()
    } else {
        format!("{base} · {short}")
    };
    let mut n = 2;
    while taken(&candidate) {
        candidate = format!("{base} · {short} ({n})");
        n += 1;
    }
    Ok(candidate)
}

fn json_str(map: &Map<String, Value>, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| map.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn json_str_opt(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    let value = json_str(map, keys);
    (!value.is_empty()).then_some(value)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.is_empty())
}

fn cmp_str(primary: String, secondary: Option<String>, fallback: String) -> String {
    if !primary.is_empty() {
        return primary;
    }
    if let Some(secondary) = secondary.filter(|value| !value.is_empty()) {
        return secondary;
    }
    if fallback.contains('@') {
        return fallback;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r47b_import_rejects_claude_swap_shape() {
        let text = r#"{"swapVersion":1,"accounts":[{"email":"a@test","credentials":{"claudeAiOauth":{}}}]}"#;
        let value: Value = hofund_plugin_sdk::json::from_str(text).expect("json");
        let rows = collect_rows(&value).expect("rows");
        assert!(
            rows[0]
                .error
                .as_deref()
                .is_some_and(|msg| msg.contains("Claude"))
        );
    }

    #[test]
    fn r47b_import_reads_codexbar_config() {
        let text = include_str!("fixtures/codexbar-config.json");
        let value: Value = hofund_plugin_sdk::json::from_str(text).expect("json");
        let rows = collect_rows(&value).expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].email, "user@example.test");
        assert_eq!(rows[0].refresh_token, "refresh-codexbar");
    }

    #[test]
    fn r47b_import_dedupes_codexbar_refresh_tokens() {
        let text = r#"{"providers":[{"kind":"openai_oauth","accounts":[
            {"email":"a@test","refreshToken":"same","accessToken":"a1","idToken":"i1"},
            {"email":"b@test","refreshToken":"same","accessToken":"a2","idToken":"i2"}
        ]}]}"#;
        let value: Value = hofund_plugin_sdk::json::from_str(text).expect("json");
        let rows = collect_rows(&value).expect("rows");
        assert_eq!(rows.len(), 1);
    }
}
