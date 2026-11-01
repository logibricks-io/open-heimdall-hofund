//! `hofund accounts switch`: argument shapes and help.

use std::ffi::OsString;
use std::path::Path;

use crate::serve;

/// `hofund accounts switch <agent> <next|account>`
#[derive(Debug, PartialEq, Eq)]
pub enum AccountsCommand {
    /// Switch an agent sign-in.
    Switch { agent: String, target: String },
}

pub fn parse(words: &[&str]) -> Option<Result<AccountsCommand, String>> {
    let command = match words {
        ["switch", agent, target] => AccountsCommand::Switch {
            agent: (*agent).to_owned(),
            target: (*target).to_owned(),
        },
        _ => return None,
    };
    Some(Ok(command))
}

fn daemon_args(config: &Path, command: &AccountsCommand) -> Vec<OsString> {
    let (agent, target) = match command {
        AccountsCommand::Switch { agent, target } => (agent.as_str(), target.as_str()),
    };
    vec![
        "accounts".into(),
        "--config".into(),
        config.as_os_str().into(),
        "--".into(),
        "switch".into(),
        agent.into(),
        target.into(),
    ]
}

/// Runs the command through `hofundd accounts`.
pub fn run(config: &Path, command: &AccountsCommand) -> Result<i32, String> {
    let daemon = serve::locate_installed()?;
    let mut process = std::process::Command::new(&daemon);
    process.args(daemon_args(config, command));
    serve::replace(process, &daemon)
}

pub const ACCOUNTS_HELP: &str = "\
usage: hofund accounts switch <claude|codex> <next|account> [--config PATH]

Switch Claude Code or Codex to another remembered subscription account.
`next` picks the next account in order with allowance room, or the one with
the most room left when every account is past its line. An account name,
email, id, or alias works too.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d27_10_accounts_switch_parses() {
        let command = parse(&["switch", "codex", "next"])
            .expect("some")
            .expect("ok");
        assert_eq!(
            command,
            AccountsCommand::Switch {
                agent: "codex".into(),
                target: "next".into(),
            }
        );
    }
}
