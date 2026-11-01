//! Reading Claude Code's own sign-in for account discovery.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::creds::{self, Profile};

/// Opaque import handle for the one sign-in Claude Code keeps in its home.
pub(crate) const REFERENCE: &str = "agent-sign-in";

/// Lists the account named in `home`, when credentials are present.
pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    if creds::read_credentials(home).is_none() {
        return Ok(Vec::new());
    }
    let profile = Profile::read(home);
    let id = profile.account_id()?;
    let label = profile
        .email()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string());
    let row = DiscoveredAgentAccount::new(id, label, REFERENCE)?;
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
    let Some(credentials) = creds::read_credentials(home) else {
        return Err(PluginError::auth(
            "Claude Code has no readable sign-in in that directory",
        ));
    };
    let profile = Profile::read(home);
    let id = profile.account_id()?;
    if host.accounts().secret(&id).await?.is_some() {
        return Ok(id);
    }
    let label = profile
        .email()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string());
    let blob = creds::blob(&credentials, &profile);
    let account = host.accounts().account(id.clone(), label)?;
    host.accounts().upsert(account, blob.to_secret()).await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{PluginId, PluginKind};
    use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

    use super::*;

    fn scratch(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("hofund-a1-claude-{label}-"))
            .tempdir()
            .expect("scratch")
    }

    fn write_sign_in(home: &Path) {
        fs::create_dir_all(home).expect("home");
        fs::write(
            home.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"access-example","refreshToken":"refresh-example"}}"#,
        )
        .expect("creds");
        fs::write(
            home.join(".claude.json"),
            r#"{"oauthAccount":{"emailAddress":"user@example.test","accountUuid":"11111111-2222-3333-4444-555555555555"}}"#,
        )
        .expect("profile");
    }

    #[test]
    fn a1_16_claude_list_is_empty_without_credentials() {
        let dir = scratch("empty");
        let home = dir.path().join("claude-home");
        fs::create_dir_all(&home).expect("home");
        let rows = list(&home).expect("list");
        assert!(rows.is_empty());
    }

    #[test]
    fn a1_17_claude_rejects_unknown_import_reference() {
        let dir = scratch("bad-ref");
        let home = dir.path().join("claude-home");
        write_sign_in(&home);
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let error = block_on(import(&home, "other", host.context())).expect_err("ref");
        assert!(
            error
                .to_string()
                .contains("unknown agent sign-in reference")
        );
    }

    #[test]
    fn a1_18_claude_import_leaves_stored_secret_unchanged() {
        let dir = scratch("stored-secret");
        let home = dir.path().join("claude-home");
        write_sign_in(&home);
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        let marker = SecretObject::new()
            .with_text("credentials", "marker-unchanged")
            .to_secret();
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "stored")
            .expect("account");
        block_on(host.context().accounts().upsert(account, marker)).expect("upsert");
        block_on(import(&home, REFERENCE, host.context())).expect("import");
        let after = block_on(host.context().accounts().secret(&id))
            .expect("read")
            .expect("stored");
        let blob = SecretObject::parse(&after).expect("parse");
        assert_eq!(blob.text("credentials"), Some("marker-unchanged"));
    }

    #[test]
    fn a1_3_lists_claude_sign_in_from_temp_home() {
        let dir = scratch("list");
        let home = dir.path().join("claude-home");
        write_sign_in(&home);
        let rows = list(&home).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "user@example.test");
    }

    #[test]
    fn a1_5_skips_import_when_account_already_stored() {
        let dir = scratch("dedupe");
        let home = dir.path().join("claude-home");
        write_sign_in(&home);
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = account_id_from(&["claude", "11111111-2222-3333-4444-555555555555"]);
        let account = host
            .context()
            .accounts()
            .account(id.clone(), "stored")
            .expect("account");
        let secret = SecretObject::new()
            .with_text("credentials", "{}")
            .to_secret();
        block_on(host.context().accounts().upsert(account, secret)).expect("upsert");
        let imported = block_on(import(&home, REFERENCE, host.context())).expect("import");
        assert_eq!(imported, id);
        let listed = block_on(host.context().accounts().list()).expect("list");
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn a1_6_import_adds_new_account() {
        let dir = scratch("import");
        let home = dir.path().join("claude-home");
        write_sign_in(&home);
        let host = TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        );
        let id = block_on(import(&home, REFERENCE, host.context())).expect("import");
        assert!(!id.as_str().is_empty());
    }

    #[test]
    fn a1_7_discover_does_not_write_agent_files() {
        let dir = scratch("readonly");
        let home = dir.path().join("claude-home");
        write_sign_in(&home);
        let creds = home.join(".credentials.json");
        let modified = fs::metadata(&creds)
            .expect("meta")
            .modified()
            .expect("time");
        let _rows = list(&home).expect("list");
        assert_eq!(
            fs::metadata(&creds)
                .expect("meta")
                .modified()
                .expect("time"),
            modified
        );
    }

    #[test]
    fn a1_8_list_output_holds_no_token_like_secrets() {
        let dir = scratch("redact");
        let home = dir.path().join("claude-home");
        write_sign_in(&home);
        let rows = list(&home).expect("list");
        let text = format!("{rows:?}");
        assert!(!text.contains("access-example"));
        assert!(!text.contains("refresh-example"));
    }
}
