//! Native `/responses/compact` account holds and the API-key sign-in allowance.

use hofund_plugin_api::{CallerScope, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{account_id_from, unverified_claims};

const AUTH_CLAIM: &str = "https://api.openai.com/auth";
const FILE_CAP: u64 = 1024 * 1024;

/// Whether native compaction must be refused for `model` under `scope`.
#[must_use]
pub(crate) fn compact_signin_held(scope: &CallerScope, model: &str) -> Option<String> {
    if !scope.holds_accounts() {
        return None;
    }
    if codex_api_key_signed_in() {
        return None;
    }
    let Some(reference) = sign_in_account_reference() else {
        return Some(held_message(scope, model));
    };
    if allows_account(&scope.accounts, "codex", &reference) {
        return None;
    }
    Some(held_message(scope, model))
}

/// Codex is signed in with its own OpenAI API key (`auth_mode: apikey`).
#[must_use]
pub(crate) fn codex_api_key_signed_in() -> bool {
    let Some(text) = read_auth_text() else {
        return false;
    };
    let Ok(root) = json::from_str::<Value>(&text) else {
        return false;
    };
    root.get("auth_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode == "apikey")
}

fn sign_in_account_reference() -> Option<String> {
    let text = read_auth_text()?;
    let root = json::from_str::<Value>(&text).ok()?;
    if root
        .get("auth_mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode == "apikey")
    {
        return None;
    }
    let id_token = root
        .get("tokens")?
        .get("id_token")?
        .as_str()
        .filter(|value| !value.is_empty())?;
    let identity = sign_in_identity(id_token).ok()?;
    Some(identity)
}

fn sign_in_identity(id_token: &str) -> Result<String, ()> {
    let claims = unverified_claims(&SecretString::from(id_token.to_owned())).map_err(|_| ())?;
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if email.is_empty() {
        return Err(());
    }
    let auth = claims.get(AUTH_CLAIM);
    let chatgpt_account = auth
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let sub = claims
        .get("sub")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(account_id_from(&[sub, chatgpt_account]).to_string())
}

fn read_auth_text() -> Option<String> {
    let home = crate::official::codex_home_dir()?;
    let path = home.join("auth.json");
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return None;
    }
    std::fs::read_to_string(&path).ok()
}

fn allows_account(accounts: &[String], provider: &str, reference: &str) -> bool {
    if accounts.is_empty() {
        return true;
    }
    let provider = provider.trim().to_lowercase();
    let key = format!("{provider}/{}", reference.trim().to_lowercase());
    let mut named = false;
    for account in accounts {
        let account = account.trim().to_lowercase();
        if account == key {
            return true;
        }
        if account.split_once('/').is_some_and(|(p, _)| p == provider) {
            named = true;
        }
    }
    !named
}

fn held_message(scope: &CallerScope, model: &str) -> String {
    format!(
        "Native compaction for {model} goes through the account Codex is signed in to, which the gateway key {:?} may not use; it may use {}. Native compaction has no other route: add the signed-in account to the key in hofund's gateway-key settings, or use one of hofund's models, whose compaction goes through a compaction_trigger on /responses and the key's accounts.",
        scope.key_name,
        key_account_names(scope),
    )
}

fn key_account_names(scope: &CallerScope) -> String {
    if scope.accounts.is_empty() {
        return "none".to_owned();
    }
    scope.accounts.join(", ")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

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
            .prefix(&format!("hofund-r47a3-{label}-"))
            .tempdir()
            .expect("scratch")
    }

    fn write_chatgpt_auth(home: &Path, sub: &str, account: &str) {
        fs::create_dir_all(home).expect("home");
        let id = id_token("u@example.test", sub, account, "plus");
        let text = format!(
            r#"{{"auth_mode":"chatgpt","tokens":{{"access_token":"access-example","refresh_token":"refresh-example","id_token":"{id}"}}}}"#
        );
        fs::write(home.join("auth.json"), text).expect("auth");
    }

    fn write_apikey_auth(home: &Path) {
        fs::create_dir_all(home).expect("home");
        let id = id_token("k@example.test", "sub-k", "acct-k", "plus");
        let text = format!(
            r#"{{"auth_mode":"apikey","tokens":{{"access_token":"sk-example","refresh_token":"refresh-example","id_token":"{id}"}}}}"#
        );
        fs::write(home.join("auth.json"), text).expect("auth");
    }

    fn account_id(sub: &str, account: &str) -> String {
        account_id_from(&[sub, account]).to_string()
    }

    fn with_home(home: PathBuf, run: impl FnOnce()) {
        crate::official::set_test_codex_home(Some(home));
        run();
        crate::official::set_test_codex_home(None);
    }

    #[test]
    fn r47a_a3_compact_holds_refuse_chatgpt_sign_in_outside_key_accounts() {
        let dir = scratch("held");
        let home = dir.path().to_path_buf();
        write_chatgpt_auth(&home, "sub-held", "acct-held");
        let allowed = account_id("sub-held", "acct-held");
        let scope = CallerScope::new("held-key", vec![format!("codex/{allowed}")]);
        with_home(home.clone(), || {
            assert!(compact_signin_held(&scope, "gpt-5").is_none());
        });
        let only = account_id("sub-only", "acct-only");
        write_chatgpt_auth(&home, "sub-held", "acct-held");
        let scope = CallerScope::new("held-key", vec![format!("codex/{only}")]);
        with_home(home, || {
            let held = compact_signin_held(&scope, "gpt-5").expect("held");
            assert!(held.contains("Native compaction for gpt-5"));
            assert!(held.contains("held-key"));
            assert!(held.contains("compaction_trigger on /responses"));
            assert!(held.contains(&format!("codex/{only}")));
        });
    }

    #[test]
    fn r47a_a3_compact_holds_allow_apikey_sign_in_with_account_holds() {
        let dir = scratch("apikey");
        let home = dir.path().to_path_buf();
        write_apikey_auth(&home);
        let scope = CallerScope::new("narrow", vec!["codex/some-account".to_owned()]);
        with_home(home, || {
            assert!(codex_api_key_signed_in());
            assert!(compact_signin_held(&scope, "gpt-5").is_none());
        });
    }

    #[test]
    fn r47a_a3_compact_holds_allow_when_key_has_no_account_holds() {
        let dir = scratch("open");
        let home = dir.path().to_path_buf();
        write_chatgpt_auth(&home, "sub-open", "acct-open");
        let scope = CallerScope::default();
        with_home(home, || {
            assert!(compact_signin_held(&scope, "gpt-5").is_none());
        });
    }
}
