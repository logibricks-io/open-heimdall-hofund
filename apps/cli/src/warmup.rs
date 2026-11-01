//! `hofund warmup`: daily warm-up status and manual triggers.

use std::path::Path;
use std::process::ExitCode;

use hofund_control_client::ClientError;
use hofund_control_client::hofund_control_proto::{WarmupRunRequest, WarmupRunResponse};

use crate::commands::{client, failure};

/// Help for `hofund warmup`.
pub(crate) const WARMUP_HELP: &str = "\
usage: hofund warmup [--json] [--config PATH]
       hofund warmup <subscription>/<account> [--config PATH]

Lists the last daily warm-up attempt per subscription account the daemon has
seen, or runs one warm-up now for a signed-in account. Configure times under
`[subscriptions.options]` with `daily-warmup-at-times`. The gateway serves the
same list at GET /v1/hofund/warmups and manual runs at POST /v1/hofund/warmups/run.
";

/// Parsed `hofund warmup` command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WarmupCommand {
    Help,
    List { json: bool },
    Run { target: String },
}

pub(crate) fn parse(words: &[&str]) -> WarmupCommand {
    if words == ["help"] || words == ["--help"] {
        return WarmupCommand::Help;
    }
    let mut json = false;
    let mut target = None;
    for word in words {
        match *word {
            "--json" => json = true,
            "--help" | "-h" => return WarmupCommand::Help,
            other if other.starts_with('-') => {}
            other => {
                if target.is_some() {
                    return WarmupCommand::Help;
                }
                target = Some(other.to_owned());
            }
        }
    }
    match target {
        Some(target) => WarmupCommand::Run { target },
        None => WarmupCommand::List { json },
    }
}

pub(crate) fn run(config: &Path, command: WarmupCommand) -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("async runtime");
    match runtime.block_on(run_async(config, command)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn run_async(config: &Path, command: WarmupCommand) -> Result<String, String> {
    match command {
        WarmupCommand::Help => Err(WARMUP_HELP.to_owned()),
        WarmupCommand::List { json } => list(config, json).await,
        WarmupCommand::Run { target } => trigger(config, &target).await,
    }
}

async fn list(config: &Path, json: bool) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.warmups().await.map_err(failure)?;
    if json {
        return serde_json::to_string_pretty(&response)
            .map_err(|error| format!("cannot encode warm-up status: {error}"));
    }
    if response.accounts.is_empty() {
        return Ok("no warm-up attempts recorded yet".to_owned());
    }
    let mut lines = Vec::new();
    for row in &response.accounts {
        let when = row
            .at_ms
            .map(|ms| format!(" at {ms}ms"))
            .unwrap_or_default();
        let detail = if row.detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", row.detail)
        };
        lines.push(format!(
            "{} / {}: {}{}{}",
            row.subscription, row.account, row.outcome, detail, when
        ));
    }
    Ok(lines.join("\n"))
}

async fn trigger(config: &Path, target: &str) -> Result<String, String> {
    let (subscription, account) = split_target(target)?;
    let client = client(config).await?;
    let response = client
        .run_warmup(WarmupRunRequest::new(subscription, account))
        .await
        .map_err(|error| match error {
            ClientError::Server { .. } => failure(error),
            other => other.to_string(),
        })?;
    Ok(format_run(&response))
}

fn split_target(target: &str) -> Result<(String, String), String> {
    let (subscription, account) = target.split_once('/').ok_or_else(|| {
        "use subscription/account, for example acct/user@example.test".to_owned()
    })?;
    if subscription.is_empty() || account.is_empty() {
        return Err("subscription and account must be non-empty".to_owned());
    }
    Ok((subscription.to_owned(), account.to_owned()))
}

fn format_run(response: &WarmupRunResponse) -> String {
    format!(
        "warm-up {} at {}ms{detail}",
        response.outcome,
        response.at_ms,
        detail = if response.detail.is_empty() {
            String::new()
        } else {
            format!(": {}", response.detail)
        }
    )
}

#[cfg(test)]
mod tests {
    use super::{WarmupCommand, parse, split_target};

    #[test]
    fn a3a_18_warmup_cli_parses_list_and_run() {
        assert_eq!(parse(&[]), WarmupCommand::List { json: false });
        assert_eq!(parse(&["--json"]), WarmupCommand::List { json: true });
        assert_eq!(parse(&["codex", "user@x.io"]), WarmupCommand::Help);
        assert_eq!(
            parse(&["codex/user@x.io"]),
            WarmupCommand::Run {
                target: "codex/user@x.io".to_owned()
            }
        );
    }

    #[test]
    fn a3a_18_warmup_cli_splits_target() {
        let (sub, acct) = split_target("claude/me@x.io").expect("split");
        assert_eq!(sub, "claude");
        assert_eq!(acct, "me@x.io");
        assert!(split_target("noseparator").is_err());
    }
}
