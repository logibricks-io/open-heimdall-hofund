//! Account discovery from kiro-cli's database or the IDE token file.

use std::path::Path;

use hofund_plugin_api::{AccountId, DiscoveredAgentAccount, HostContext, PluginError};

use crate::cli_db;
use crate::creds::{self, Credential};

pub(crate) const REFERENCE: &str = "ide-token";
pub(crate) const CLI_REFERENCE: &str = "cli-db";

pub(crate) fn list(home: &Path) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    let cli_path = cli_db::default_path().ok();
    list_at(home, cli_path.as_deref())
}

pub(crate) fn list_at(
    home: &Path,
    cli_path: Option<&Path>,
) -> Result<Vec<DiscoveredAgentAccount>, PluginError> {
    if let Some(path) = cli_path
        && let Ok(Some(credential)) = cli_db::read_at(path)
    {
        let id = creds::account_id_for_home(home, &credential);
        let row = DiscoveredAgentAccount::new(id, credential.label.clone(), CLI_REFERENCE)?;
        return Ok(vec![row]);
    }
    let path = creds::token_path_in_home(home);
    let Ok(credential) = creds::read_token_file(&path) else {
        return Ok(Vec::new());
    };
    let id = creds::account_id_for_home(home, &credential);
    let row = DiscoveredAgentAccount::new(id, credential.label.clone(), REFERENCE)?;
    Ok(vec![row])
}

pub(crate) async fn import(
    home: &Path,
    reference: &str,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let cli_path = cli_db::default_path().ok();
    import_at(home, reference, host, cli_path.as_deref()).await
}

pub(crate) async fn import_at(
    home: &Path,
    reference: &str,
    host: &HostContext,
    cli_path: Option<&Path>,
) -> Result<AccountId, PluginError> {
    match reference {
        CLI_REFERENCE => {
            let path = cli_path.ok_or_else(|| {
                PluginError::auth("cannot resolve the kiro-cli sign-in database path")
            })?;
            let credential = cli_db::read_at(path)?.ok_or_else(|| {
                PluginError::auth("kiro-cli has no readable sign-in in its database")
            })?;
            store_credential(home, &credential, host).await
        }
        REFERENCE => {
            let path = creds::token_path_in_home(home);
            let mut credential = creds::read_token_file(&path)?;
            credential.ide_token_path = path.to_string_lossy().into_owned();
            store_credential(home, &credential, host).await
        }
        _ => Err(PluginError::config(format!(
            "unknown agent sign-in reference `{reference}`"
        ))),
    }
}

async fn store_credential(
    home: &Path,
    credential: &Credential,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let id = creds::account_id_for_home(home, credential);
    let account = host
        .accounts()
        .account(id.clone(), credential.label.clone())?;
    host.accounts()
        .upsert(account, creds::secret_blob(credential).to_secret())
        .await?;
    Ok(id)
}

pub(crate) async fn store(
    home: &Path,
    credential: &Credential,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let mut credential = credential.clone();
    if credential.ide_token_path.is_empty() && credential.db_key.is_empty() {
        credential.ide_token_path = creds::token_path_in_home(home)
            .to_string_lossy()
            .into_owned();
    }
    store_credential(home, &credential, host).await
}

pub(crate) async fn store_browser(
    credential: &Credential,
    host: &HostContext,
) -> Result<AccountId, PluginError> {
    let id = creds::browser_account_id(credential);
    let account = host
        .accounts()
        .account(id.clone(), credential.label.clone())?;
    host.accounts()
        .upsert(account, creds::secret_blob(credential).to_secret())
        .await?;
    Ok(id)
}
