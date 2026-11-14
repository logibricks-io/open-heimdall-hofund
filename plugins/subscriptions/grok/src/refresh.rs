//! Renewing the CLI sign-in by running `grok models` in the account home.

use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_plugin_api::{AccountId, AccountLease, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::same_secret;
use hofund_plugin_sdk::timeout;

use crate::cli;
use crate::creds::{self, Stored};

const LEASE_WAIT: Duration = Duration::from_secs(30);
const MODELS_WAIT: Duration = Duration::from_secs(120);

/// Runs `grok models` when the token is within the refresh margin.
pub(crate) async fn run(
    host: &HostContext,
    cache: &creds::Cache,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let stored = Stored::load(host, account).await?;
    let live = Stored {
        home: stored.home.clone(),
        credential: read_live(Path::new(&stored.home), cache)?,
    };
    if live.fresh_at(host.clock().now()) && same_credential(&stored.credential, &live.credential) {
        return Ok(());
    }
    renew(host, cache, program, account, None, true)
        .await
        .map(|_| ())
}

/// Ensures the account token is fresh enough for an API call.
pub(crate) async fn ensure_fresh(
    host: &HostContext,
    account: &AccountId,
    cache: &creds::Cache,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    rejected: Option<&SecretString>,
) -> Result<Stored, PluginError> {
    let stored = Stored::load(host, account).await?;
    if rejected.is_none() {
        let live = Stored {
            home: stored.home.clone(),
            credential: read_live(Path::new(&stored.home), cache)?,
        };
        if live.fresh_at(host.clock().now())
            && same_credential(&stored.credential, &live.credential)
        {
            return Ok(live);
        }
    }
    renew(host, cache, program, account, rejected, false).await
}

async fn renew(
    host: &HostContext,
    cache: &creds::Cache,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    account: &AccountId,
    rejected: Option<&SecretString>,
    hook: bool,
) -> Result<Stored, PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let current = creds::parse_secret(lease.secret())?;
    let now = host.clock().now();
    let home = PathBuf::from(&current.home);
    let live = Stored {
        home: current.home.clone(),
        credential: read_live(&home, cache)?,
    };
    if rejected.is_none() && live.fresh_at(now) {
        return commit(lease, account, &current, live.credential).await;
    }
    let changed = rejected.is_some_and(|key| !same_secret(&live.credential.key, key));
    if changed && live.ready_at(now) {
        return commit(lease, account, &current, live.credential).await;
    }

    let program = match program() {
        Ok(program) => program,
        Err(_) if hook || (rejected.is_none() && live.ready_at(now)) => {
            return commit(lease, account, &current, live.credential).await;
        }
        Err(_) => {
            return Err(PluginError::status_error(
                401,
                format!(
                    "Grok turned the sign-in away and there is no Grok Build here to renew it; {}",
                    cli::no_cli()
                ),
            ));
        }
    };
    let was = live.credential.key.clone();
    let outcome = run_cli(host, &program, &home).await?;
    let credential = read_live(&home, cache)?;
    if rejected.is_some() && same_secret(&credential.key, &was) {
        return Err(unrenewed(&home, &outcome));
    }
    if !(Stored {
        home: current.home.clone(),
        credential: credential.clone(),
    })
    .ready_at(now)
    {
        return Err(unrenewed(&home, &outcome));
    }
    commit(lease, account, &current, credential).await
}

struct CliRun {
    code: i32,
    last: String,
    timed_out: bool,
}

async fn run_cli(host: &HostContext, program: &Path, home: &Path) -> Result<CliRun, PluginError> {
    let request = cli::request(program, &cli::MODELS_ARGS, home)?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let mut stdout = child.stdout()?;
    let mut last = String::new();
    let finished = timeout(MODELS_WAIT, async {
        cli::drain(&mut stdout, &mut last).await;
        child.exit_code().await
    })
    .await;
    let stderr = child.stderr_tail();
    if !stderr.is_empty() {
        last = cli::last_output(&String::from_utf8_lossy(&stderr));
    }
    match finished {
        Some(Ok(code)) => Ok(CliRun {
            code,
            last,
            timed_out: false,
        }),
        Some(Err(error)) => Err(error),
        None => Ok(CliRun {
            code: -1,
            last,
            timed_out: true,
        }),
    }
}

fn read_live(home: &Path, cache: &creds::Cache) -> Result<creds::Credential, PluginError> {
    creds::read_cached(home, cache).map_err(|error| match error {
        creds::ReadError::Torn(reason) => PluginError::io(format!(
            "Grok Build's sign-in couldn't be read ({reason}); it may be mid-write, try again in a moment"
        )),
        creds::ReadError::Missing(_) => not_signed_in(home),
    })
}

async fn commit(
    lease: AccountLease,
    account: &AccountId,
    current: &Stored,
    credential: creds::Credential,
) -> Result<Stored, PluginError> {
    if creds::account_id(&credential) != *account {
        return Err(PluginError::auth(
            "the CLI home is now signed in to a different account",
        ));
    }
    let home = PathBuf::from(&current.home);
    if !same_credential(&current.credential, &credential) {
        lease
            .commit(
                Some(label(&credential)),
                creds::secret_blob(&home, &credential).to_secret(),
            )
            .await?;
    }
    Ok(Stored {
        home: current.home.clone(),
        credential,
    })
}

fn same_credential(left: &creds::Credential, right: &creds::Credential) -> bool {
    same_secret(&left.key, &right.key)
        && left.expires_ms == right.expires_ms
        && left.email == right.email
        && left.who == right.who
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

fn cli_said(run: &CliRun) -> String {
    let how = if run.timed_out {
        "timed out".to_owned()
    } else if run.code == 0 {
        "ran and left the same token".to_owned()
    } else {
        format!("exited {}", run.code)
    };
    let last = if run.last.is_empty() {
        String::new()
    } else {
        format!(": {}", run.last)
    };
    format!(" (grok models {how}{last})")
}

fn unrenewed(home: &Path, run: &CliRun) -> PluginError {
    unrenewed_at(home, &crate::default_cli_home(), run)
}

fn unrenewed_at(home: &Path, cli_home: &Path, run: &CliRun) -> PluginError {
    let how = if shared(home, cli_home) {
        "open Grok Build and send it a message, which renews the sign-in hofund reads"
    } else {
        "this account's sign-in is kept apart from Grok Build's own, so opening Grok Build doesn't renew it: add the account in hofund again"
    };
    PluginError::status_error(
        401,
        format!(
            "Grok Build couldn't renew the sign-in{}; {how}",
            cli_said(run)
        ),
    )
}

pub(crate) fn dead(home: &Path) -> PluginError {
    let message = if shared(home, &crate::default_cli_home()) {
        "Grok turned away Grok Build's sign-in even after renewing it; run `grok login` to sign Grok Build in afresh"
    } else {
        "Grok turned away this account's sign-in even after renewing it; add the account in hofund again"
    };
    PluginError::status_error(401, message)
}

fn not_signed_in(home: &Path) -> PluginError {
    let message = if shared(home, &crate::default_cli_home()) {
        "Grok is not signed in; run `grok login`".to_owned()
    } else {
        "Grok is not signed in for this account any more; add the account in hofund again"
            .to_owned()
    };
    PluginError::auth(message)
}

fn shared(home: &Path, cli_home: &Path) -> bool {
    absolute(home) == absolute(cli_home)
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{CliRun, cli_said, unrenewed_at};

    #[test]
    fn r50_unrenewed_wording_distinguishes_shared_and_account_homes() {
        let run = CliRun {
            code: 0,
            last: String::new(),
            timed_out: false,
        };
        assert_eq!(cli_said(&run), " (grok models ran and left the same token)");
        let own = unrenewed_at(Path::new("/account"), Path::new("/cli"), &run);
        assert_eq!(own.status(), Some(401));
        assert!(own.to_string().contains("add the account in hofund again"));
        assert!(!own.to_string().contains("grok login"));
        let shared = unrenewed_at(Path::new("/cli"), Path::new("/cli"), &run);
        assert!(
            shared
                .to_string()
                .contains("open Grok Build and send it a message")
        );
    }
}
