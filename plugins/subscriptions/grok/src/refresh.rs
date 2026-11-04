//! Renewing the CLI sign-in by running `grok models` in the account home.

use std::path::PathBuf;

use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::oauth::{SecretObject, same_secret};
use hofund_plugin_sdk::timeout;

use crate::cli;
use crate::creds::{self, Stored};

const MODELS_WAIT: Duration = Duration::from_secs(120);

/// Runs `grok models` when the token is within the refresh margin, then
/// re-reads auth.json and updates the stored secret when the token changed.
pub(crate) async fn run(
    host: &HostContext,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let stored = Stored::load(host, account).await?;
    let now = host.clock().now();
    if stored.fresh_at(now) {
        return Ok(());
    }
    renew(host, program()?, &stored).await?;
    commit(host, account, &stored.home).await
}

/// Ensures the account token is fresh enough for an API call.
pub(crate) async fn ensure_fresh(
    host: &HostContext,
    account: &AccountId,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    force: bool,
) -> Result<Stored, PluginError> {
    let stored = Stored::load(host, account).await?;
    let now = host.clock().now();
    if !force && stored.fresh_at(now) {
        return Ok(stored);
    }
    renew(host, program()?, &stored).await?;
    commit(host, account, &stored.home).await?;
    Stored::load(host, account).await
}

async fn renew(host: &HostContext, program: PathBuf, stored: &Stored) -> Result<(), PluginError> {
    let home = PathBuf::from(&stored.home);
    let request = cli::request(&program, &cli::MODELS_ARGS, &home)?;
    let child = host.spawn_process().spawn(&request).await?;
    let wait = host.spawn_process().wait(child);
    let code = match timeout(MODELS_WAIT, wait).await {
        Some(Ok(code)) => code,
        Some(Err(error)) => return Err(error),
        None => {
            return Err(PluginError::timeout(
                "Grok Build did not finish renewing the sign-in with `grok models` in time",
            ));
        }
    };
    if code != 0 {
        return Err(PluginError::auth(
            "Grok Build could not renew the sign-in with `grok models`",
        ));
    }
    Ok(())
}

async fn commit(host: &HostContext, account: &AccountId, home: &str) -> Result<(), PluginError> {
    let path = std::path::PathBuf::from(home);
    let credential = creds::read_sync(&path).map_err(|error| match error {
        creds::ReadError::Torn(reason) => {
            PluginError::auth(format!("Grok Build's sign-in couldn't be read ({reason})"))
        }
        creds::ReadError::Missing(reason) => PluginError::auth(reason),
    })?;
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no Grok Build sign-in is stored for this account",
        ));
    };
    let blob = SecretObject::parse(&secret)?;
    let access = blob.secret("access_token");
    let expires = blob.number("expires_at").unwrap_or(0);
    if access.is_some_and(|token| same_secret(&token, &credential.key))
        && expires == credential.expires_ms
    {
        return Ok(());
    }
    let updated = creds::secret_blob(&path, &credential);
    let account_row = host
        .accounts()
        .account(account.clone(), label(&credential))?;
    host.accounts()
        .upsert(account_row, updated.to_secret())
        .await?;
    Ok(())
}

fn label(credential: &creds::Credential) -> String {
    if !credential.email.is_empty() {
        credential.email.clone()
    } else if !credential.who.is_empty() {
        credential.who.clone()
    } else {
        "Grok Build".to_owned()
    }
}
