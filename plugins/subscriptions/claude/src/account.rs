//! Running `claude` for one stored account.
//!
//! Each account has `<state>/accounts/<id>` (0700). Before a run the stored
//! credentials file is written there (0600); afterwards it is read back and
//! removed, so the tokens rest only in the account store. A file Claude Code
//! rotated is committed under the account lease, which is held for the run.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SpawnRequest};
use hofund_plugin_sdk::oauth::SecretObject;

use crate::creds::{self, Profile};

/// How long a refresh waits for another holder of the account lease.
const LEASE_WAIT: Duration = Duration::from_secs(30);

/// `<state>/accounts/<id>`. An account id is kebab case.
pub(crate) fn account_dir(host: &HostContext, account: &AccountId) -> Result<PathBuf, PluginError> {
    Ok(host
        .dirs()
        .state_dir()?
        .join("accounts")
        .join(account.as_str()))
}

/// Asks Claude Code for the account's status, which refreshes its tokens
/// when they are due, and keeps what it wrote.
pub(crate) async fn refresh(
    host: &HostContext,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    if blob.flag(creds::LAPSED) {
        return Err(PluginError::auth(creds::LAPSED_REASON));
    }
    let Some(stored) = blob.text(creds::CREDENTIALS) else {
        return Err(PluginError::auth(
            "the stored Claude Code sign-in has no credentials",
        ));
    };
    let stored = stored.to_owned();
    let profile = Profile::parse(blob.text(creds::PROFILE).unwrap_or_default());
    let program = program()?;
    let dir = account_dir(host, account)?;
    creds::private_dir(&dir)?;
    let file = dir.join(creds::CREDENTIALS_FILE);
    let ran = run(host, &program, &dir, &file, &stored, &profile).await;
    let read_back = creds::read_credentials(&dir);
    let _ = std::fs::remove_file(&file);
    let status = ran?;
    if crate::creds::logged_in_says_no(status.as_ref()) {
        let lapsed = blob.with_flag(creds::LAPSED, true);
        lease.commit(None, lapsed.to_secret()).await?;
        return Err(PluginError::auth(creds::LAPSED_REASON));
    }
    // A file Claude Code rotated is kept even when it then failed to report.
    if let Some(rotated) = read_back.filter(|rotated| rotated.text != stored) {
        let updated = blob.with_text(creds::CREDENTIALS, &rotated.text);
        lease.commit(None, updated.to_secret()).await?;
    }
    if status.is_none() {
        return Err(PluginError::io(
            "Claude Code did not report the account status; the stored sign-in is kept",
        ));
    }
    Ok(())
}

const WARM_ARGS: [&str; 11] = [
    "-p",
    "--output-format",
    "json",
    "--model",
    "haiku",
    "--tools",
    "",
    "--strict-mcp-config",
    "--setting-sources",
    "",
    "--no-session-persistence",
];

/// Sends one "hi" through Claude Code for `account`.
pub(crate) async fn warmup(
    host: &HostContext,
    program: impl FnOnce() -> Result<PathBuf, PluginError>,
    account: &AccountId,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    let blob = SecretObject::parse(lease.secret())?;
    if blob.flag(creds::LAPSED) {
        return Err(PluginError::auth(creds::LAPSED_REASON));
    }
    let Some(stored) = blob.text(creds::CREDENTIALS) else {
        return Err(PluginError::auth(
            "the stored Claude Code sign-in has no credentials",
        ));
    };
    let profile = Profile::parse(blob.text(creds::PROFILE).unwrap_or_default());
    let program = program()?;
    let dir = account_dir(host, account)?;
    creds::private_dir(&dir)?;
    let file = dir.join(creds::CREDENTIALS_FILE);
    creds::write_private(&file, stored)?;
    let settings = dir.join(creds::PROFILE_FILE);
    if !settings.exists() {
        creds::write_private(&settings, &profile.settings_text())?;
    }
    let work = dir.join("warmup-run");
    let _ = std::fs::remove_dir_all(&work);
    creds::private_dir(&work)?;
    let request = SpawnRequest::new(
        &program,
        WARM_ARGS.iter().map(OsString::from).collect(),
        vec![(
            OsString::from(crate::cli::CONFIG_DIR_VAR),
            dir.as_os_str().to_owned(),
        )],
        &work,
    )?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    child.write_stdin(b"hi\n".to_vec().into()).await?;
    let mut stdout = child.stdout()?;
    let mut last = String::new();
    let finished = hofund_plugin_sdk::timeout(
        Duration::from_secs(60),
        crate::cli::drain(&mut stdout, &mut last),
    )
    .await;
    drop(child);
    if finished.is_none() {
        return Err(PluginError::timeout(
            "Claude Code did not finish the warm-up in time",
        ));
    }
    if let Some(result) = warmup_result(&last)
        && result.get("is_error").and_then(|v| v.as_bool()) == Some(true)
    {
        let message = result
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("Claude Code refused the warm-up");
        return Err(PluginError::transport(message));
    }
    if warmup_result(&last).is_none() && !last.is_empty() {
        return Err(PluginError::transport(format!(
            "Claude Code warm-up ended without a result; {}",
            crate::cli::said(&last)
        )));
    }
    drop(lease);
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_file(&file);
    Ok(())
}

fn warmup_result(text: &str) -> Option<hofund_plugin_sdk::json::Value> {
    use hofund_plugin_sdk::json::Value;
    text.lines()
        .filter_map(|line| hofund_plugin_sdk::json::from_str::<Value>(line).ok())
        .find(|value| value.get("type").and_then(|v| v.as_str()) == Some("result"))
}

async fn run(
    host: &HostContext,
    program: &Path,
    dir: &Path,
    file: &Path,
    stored: &str,
    profile: &Profile,
) -> Result<Option<hofund_plugin_sdk::json::Value>, PluginError> {
    creds::write_private(file, stored)?;
    let settings = dir.join(creds::PROFILE_FILE);
    if !settings.exists() {
        creds::write_private(&settings, &profile.settings_text())?;
    }
    Ok(crate::status(host, program, dir).await)
}

#[cfg(test)]
mod warmup_tests {
    use super::warmup_result;

    #[test]
    fn a3a_10_claude_warmup_reads_result_line() {
        let text = "not json\n{\"type\":\"result\",\"is_error\":false,\"result\":\"Hi\"}\n";
        let result = warmup_result(text).expect("result");
        assert_eq!(result["is_error"], false);
    }
}
