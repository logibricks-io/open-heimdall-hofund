//! Read-only discovery of Gemini CLI's own Google sign-in.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{
    AccountId, DiscoveredAgentAccount, HostContext, PluginError, SecretString,
};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

use crate::{import, project};

const REFERENCE: &str = "agent-sign-in";
const FILE_CAP: u64 = 1024 * 1024;

struct OwnLogin {
    refresh: SecretString,
    id_token: Option<SecretString>,
    email: String,
    project: Option<String>,
}

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let Some(login) = read_login(home) else {
        return Ok(Vec::new());
    };
    if login
        .id_token
        .as_ref()
        .is_some_and(|token| !import::discovery_client_matches(token))
    {
        return Ok(Vec::new());
    }
    let id = account_id(&login.email);
    Ok(vec![DiscoveredAgentAccount::new(
        id,
        login.email,
        REFERENCE,
    )?])
}

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
    let Some(login) = read_login(home) else {
        return Err(PluginError::auth(
            "Gemini CLI has no readable sign-in in that directory",
        ));
    };
    if login
        .id_token
        .as_ref()
        .is_some_and(|token| !super::import::discovery_client_matches(token))
    {
        return Err(PluginError::auth(
            "the Gemini CLI sign-in belongs to another OAuth client",
        ));
    }
    let id = account_id(&login.email);
    if host.accounts().secret(&id).await?.is_some() {
        return Ok(id);
    }
    let mut blob = SecretObject::new()
        .with_number("v", 1)
        .with_secret("refresh_token", &login.refresh)
        .with_text("email", &login.email);
    if let Some(project_id) = login.project {
        blob = blob.with_text(project::PROJECT, &project_id);
    }
    let account = host.accounts().account(id.clone(), login.email)?;
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(id)
}

fn account_id(email: &str) -> AccountId {
    account_id_from(&["google", &email.to_ascii_lowercase()])
}

fn read_login(home: &Path) -> Option<OwnLogin> {
    let direct = home.join("oauth_creds.json");
    let path = if direct.is_file() {
        direct
    } else {
        home.join(".gemini").join("oauth_creds.json")
    };
    let root: Value = read_json(&path)?;
    let object = root.as_object()?;
    let refresh = object
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let id_token = object
        .get("id_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| SecretString::from(value.to_owned()));
    let agent_dir = path.parent()?;
    let email = read_json(&agent_dir.join("google_accounts.json"))
        .and_then(|accounts| {
            accounts
                .get("active")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "Google".to_owned());
    let project = read_project(&agent_dir.join(".env"));
    Some(OwnLogin {
        refresh: SecretString::from(refresh.to_owned()),
        id_token,
        email,
        project,
    })
}

fn read_json(path: &PathBuf) -> Option<Value> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > FILE_CAP {
        return None;
    }
    hofund_plugin_sdk::json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn read_project(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > FILE_CAP {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "GOOGLE_CLOUD_PROJECT" {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value)
            .trim();
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{PluginId, PluginKind};

    use super::*;

    #[test]
    fn r49d_discovers_and_imports_without_writing_the_agent_home() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join(".gemini");
        fs::create_dir_all(&home).expect("home");
        let credentials = home.join("oauth_creds.json");
        fs::write(
            &credentials,
            r#"{"access_token":"access-example","refresh_token":"refresh-example","expiry_date":1}"#,
        )
        .expect("credentials");
        fs::write(
            home.join("google_accounts.json"),
            r#"{"active":"U@Example.test","old":[]}"#,
        )
        .expect("accounts");
        fs::write(
            home.join(".env"),
            "# project\nGOOGLE_CLOUD_PROJECT=\"proj-imported\"\n",
        )
        .expect("env");
        let before = fs::read(&credentials).expect("before");
        let rows = list(&home).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "U@Example.test");
        assert!(!format!("{rows:?}").contains("refresh-example"));

        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("gemini"),
            dir.path(),
        );
        let id = block_on(import(&home, REFERENCE, host.context())).expect("import");
        assert_eq!(id, account_id("u@example.test"));
        let secret = block_on(host.context().accounts().secret(&id))
            .expect("read")
            .expect("stored");
        let blob = SecretObject::parse(&secret).expect("blob");
        assert_eq!(blob.text(project::PROJECT), Some("proj-imported"));
        assert_eq!(blob.text("email"), Some("U@Example.test"));
        assert!(blob.secret("access_token").is_none());
        assert_eq!(fs::read(&credentials).expect("after"), before);
        assert_eq!(
            block_on(import(&home, "other", host.context()))
                .expect_err("unknown reference")
                .kind(),
            hofund_plugin_api::PluginErrorKind::Config
        );
    }

    #[test]
    fn r49d_parent_home_and_missing_or_invalid_files_are_safe() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("cli-parent");
        fs::create_dir_all(home.join(".gemini")).expect("home");
        fs::write(
            home.join(".gemini").join("oauth_creds.json"),
            r#"{"refresh_token":"refresh-example"}"#,
        )
        .expect("credentials");
        let rows = list(&home).expect("parent");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "Google");

        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).expect("empty");
        assert!(list(&empty).expect("empty list").is_empty());
        fs::write(empty.join("oauth_creds.json"), "not json").expect("bad");
        assert!(list(&empty).expect("bad list").is_empty());
    }
}
