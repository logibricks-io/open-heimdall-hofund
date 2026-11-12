//! Token refresh and agent credential re-import.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::Endpoints;
use crate::creds;

pub(crate) async fn run(
    host: &HostContext,
    _endpoints: &Endpoints,
    account: &AccountId,
    device_file: &Path,
) -> Result<(), PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no Z Code sign-in is stored for this account",
        ));
    };
    let blob = SecretObject::parse(&secret)?;
    if blob.flag("follow_agent")
        && let Some(path) = blob.text("agent_credentials").filter(|p| !p.is_empty())
    {
        let home = agent_home_from_credentials(path);
        let fresh = creds::parse_agent_home(&home)?;
        if creds::account_id_for(&fresh) != *account {
            return Err(PluginError::auth(
                "the Z Code app is signed in to a different account than this import",
            ));
        }
        let mid = blob
            .text("device")
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                crate::device::device_mid(device_file).unwrap_or_else(|_| String::new())
            });
        let account_row = host
            .accounts()
            .account(account.clone(), fresh.label.clone())?;
        host.accounts()
            .upsert(account_row, creds::secret_blob(&fresh, &mid).to_secret())
            .await?;
        return Ok(());
    }
    let auth = creds::parse_secret(&secret)?;
    if auth.api_key.is_some() || auth.team() {
        return Ok(());
    }
    if let Some(jwt) = &auth.jwt
        && creds::jwt_exp_ms(jwt).is_some_and(|exp| {
            let now_ms = host
                .clock()
                .now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            exp > now_ms + 120_000
        })
    {
        return Ok(());
    }
    Err(PluginError::auth(
        "the Z Code sign-in expired; sign in again or refresh it in the Z Code app",
    ))
}

fn agent_home_from_credentials(path: &str) -> PathBuf {
    Path::new(path)
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".zcode"))
}
