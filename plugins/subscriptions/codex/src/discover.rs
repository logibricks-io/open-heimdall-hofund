//! Reading Codex CLI's auth.json for account discovery.

use std::path::Path;

use hofund_plugin_api::{
    AccountId, DiscoveredAgentAccount, HostContext, PluginError, SecretString,
};
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::sign_in;
use crate::tokens;

/// Opaque import handle for the one sign-in Codex keeps in its home.
pub(crate) const REFERENCE: &str = "agent-sign-in";

const FILE_CAP: u64 = 1024 * 1024;

/// Lists the account named in `home`, when auth.json is present.
pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let Some(parsed) = read_auth(home) else {
        return Ok(Vec::new());
    };
    let identity = sign_in::identity(&parsed.id_token)?;
    let row = DiscoveredAgentAccount::new(identity.id, identity.label, REFERENCE)?;
    Ok(vec![row])
}

/// Stores the sign-in read from `home`.
pub(crate) async fn import(
    home: &Path,
    reference: &str,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    if reference != REFERENCE {
        return Err(PluginError::config(format!(
            "unknown agent sign-in reference `{reference}`"
        )));
    }
    let Some(parsed) = read_auth(home) else {
        return Err(PluginError::auth(
            "Codex has no readable sign-in in that directory",
        ));
    };
    let identity = sign_in::identity(&parsed.id_token)?;
    if host.accounts().secret(&identity.id).await?.is_some() {
        return Ok(identity.id);
    }
    let secret = secret_from_parsed(&parsed, host)?;
    let account = host
        .accounts()
        .account(identity.id.clone(), identity.label)?;
    host.accounts().upsert(account, secret).await?;
    Ok(identity.id)
}

struct ParsedAuth {
    id_token: SecretString,
    access_token: SecretString,
    refresh_token: Option<SecretString>,
}

fn read_auth(home: &Path) -> Option<ParsedAuth> {
    let path = home.join("auth.json");
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    parse_auth_text(&text)
}

fn parse_auth_text(text: &str) -> Option<ParsedAuth> {
    let root = hofund_plugin_sdk::json::from_str::<Value>(text).ok()?;
    let tokens = root.get("tokens")?.as_object()?;
    let access = token_field(tokens, "access_token")?;
    let id_token = token_field(tokens, "id_token")?;
    let refresh = token_field(tokens, "refresh_token");
    Some(ParsedAuth {
        id_token,
        access_token: access,
        refresh_token: refresh,
    })
}

fn token_field(tokens: &Map<String, Value>, key: &str) -> Option<SecretString> {
    tokens
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::from(value.to_owned()))
}

fn secret_from_parsed(
    parsed: &ParsedAuth,
    host: &HostContext,
) -> Result<SecretString, PluginError> {
    let identity = sign_in::identity(&parsed.id_token)?;
    let refresh = parsed
        .refresh_token
        .as_ref()
        .ok_or_else(|| PluginError::auth("Codex auth.json has no refresh token"))?;
    let blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("id_token", &parsed.id_token)
        .with_secret("access_token", &parsed.access_token)
        .with_secret("refresh_token", refresh)
        .with_text("account_id", &identity.chatgpt_account)
        .with_number("last_refresh", tokens::unix_seconds(host.clock().now()));
    Ok(blob.to_secret())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
            let count = chunk.len() + 1;
            for i in 0..count {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            }
        }
        out
    }

    fn jwt(claims: &str) -> String {
        format!("e30.{}.sig", b64url(claims.as_bytes()))
    }

    fn id_token(email: &str, sub: &str, account: &str, plan: &str) -> String {
        jwt(&format!(
            r#"{{"email":"{email}","sub":"{sub}","https://api.openai.com/auth":{{"chatgpt_account_id":"{account}","chatgpt_plan_type":"{plan}"}}}}"#
        ))
    }

    fn scratch(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("hofund-a1-codex-{label}-"))
            .tempdir()
            .expect("scratch")
    }

    fn write_auth(home: &Path) {
        fs::create_dir_all(home).expect("home");
        let id = id_token("u@example.test", "sub-1", "acct-1", "plus");
        let text = format!(
            r#"{{"tokens":{{"access_token":"access-example","refresh_token":"refresh-example","id_token":"{id}"}}}}"#
        );
        fs::write(home.join("auth.json"), text).expect("auth");
    }

    #[test]
    fn a1_19_codex_ignores_oversized_auth_json() {
        let dir = scratch("large");
        let home = dir.path().join("codex-home");
        write_auth(&home);
        assert_eq!(list(&home).expect("list").len(), 1, "valid file is listed");
        let path = home.join("auth.json");
        let mut data = fs::read(&path).expect("read");
        let cap = usize::try_from(FILE_CAP).expect("cap");
        data.resize(cap + 1, b' ');
        fs::write(&path, data).expect("pad");
        let rows = list(&home).expect("list");
        assert!(rows.is_empty(), "valid JSON over the size cap is ignored");
    }

    #[test]
    fn a1_20_codex_rejects_unknown_import_reference() {
        let dir = scratch("bad-ref");
        let home = dir.path().join("codex-home");
        write_auth(&home);
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("codex"),
            dir.path(),
        );
        let error = hofund_kit_subscription::block_on(import(&home, "other", host.context()))
            .expect_err("ref");
        assert!(
            error
                .to_string()
                .contains("unknown agent sign-in reference")
        );
    }

    #[test]
    fn a1_21_codex_import_leaves_stored_secret_unchanged() {
        let dir = scratch("stored-secret");
        let home = dir.path().join("codex-home");
        write_auth(&home);
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("codex"),
            dir.path(),
        );
        let rows = list(&home).expect("list");
        let id = rows[0].id().clone();
        let marker = SecretObject::new()
            .with_text("account_id", "marker-unchanged")
            .to_secret();
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "stored")
            .expect("account");
        hofund_kit_subscription::block_on(host.context().accounts().upsert(account, marker))
            .expect("upsert");
        hofund_kit_subscription::block_on(import(&home, REFERENCE, host.context()))
            .expect("import");
        let after = hofund_kit_subscription::block_on(host.context().accounts().secret(&id))
            .expect("read")
            .expect("stored");
        let blob = SecretObject::parse(&after).expect("parse");
        assert_eq!(blob.text("account_id"), Some("marker-unchanged"));
    }

    #[test]
    fn a1_22_codex_import_errors_when_refresh_token_missing() {
        let dir = scratch("no-refresh");
        let home = dir.path().join("codex-home");
        fs::create_dir_all(&home).expect("home");
        let id = id_token("u@example.test", "sub-1", "acct-1", "plus");
        let text = format!(r#"{{"tokens":{{"access_token":"access-example","id_token":"{id}"}}}}"#);
        fs::write(home.join("auth.json"), text).expect("auth");
        let host = hofund_kit_subscription::TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("codex"),
            dir.path(),
        );
        let error = hofund_kit_subscription::block_on(import(&home, REFERENCE, host.context()))
            .expect_err("refresh");
        assert!(error.to_string().contains("no refresh token"));
    }

    #[test]
    fn a1_4_lists_codex_sign_in_from_temp_home() {
        let dir = scratch("list");
        let home = dir.path().join("codex-home");
        write_auth(&home);
        let rows = list(&home).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "u@example.test");
        let text = format!("{rows:?}");
        assert!(!text.contains("access-example"));
    }
}
