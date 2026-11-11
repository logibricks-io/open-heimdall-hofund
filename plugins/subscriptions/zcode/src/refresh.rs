//! Token refresh and agent credential re-import.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::{SecretObject, same_secret};

use crate::Endpoints;
use crate::creds;
use crate::session::LEASE_WAIT;

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
        let home = agent_home_from_credentials(path)?;
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
        let lease = host.accounts().lease(account, LEASE_WAIT).await?;
        let current = SecretObject::parse(lease.secret())?;
        let keep_lapsed = current.flag(creds::LAPSED)
            && current.secret("team_token").is_some_and(|stored| {
                fresh
                    .team_token
                    .as_ref()
                    .is_some_and(|token| same_secret(&stored, token))
            });
        let mut replacement = creds::secret_blob(&fresh, &mid);
        if keep_lapsed {
            replacement = replacement.with_flag(creds::LAPSED, true);
        }
        lease
            .commit(Some(fresh.label.clone()), replacement.to_secret())
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

fn agent_home_from_credentials(path: &str) -> Result<PathBuf, PluginError> {
    let home = Path::new(path)
        .parent()
        .and_then(|p| p.parent())
        .filter(|home| home.is_absolute())
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            PluginError::auth("the stored Z Code agent credential path is not an absolute v2 path")
        })?;
    Ok(home)
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::PluginErrorKind;

    #[test]
    fn r57_agent_home_requires_an_absolute_v2_credential_path() {
        for path in [
            "credentials.json",
            "v2/credentials.json",
            "/credentials.json",
        ] {
            let error = super::agent_home_from_credentials(path).expect_err(path);
            assert_eq!(error.kind(), PluginErrorKind::Auth);
        }
        let home = super::agent_home_from_credentials("/tmp/agent/.zcode/v2/credentials.json")
            .expect("absolute path");
        assert_eq!(home, std::path::Path::new("/tmp/agent/.zcode"));
    }
}
