//! `hofund accounts`: argument shapes and help.

use std::ffi::OsString;
use std::path::Path;

use crate::serve;

/// `hofund accounts` subcommands.
#[derive(Debug, PartialEq, Eq)]
pub enum AccountsCommand {
    /// Switch an agent sign-in.
    Switch { agent: String, target: String },
    /// Import accounts from an export file.
    Import { file: String },
}

pub fn parse(words: &[&str]) -> Option<Result<AccountsCommand, String>> {
    let command = match words {
        ["switch", agent, target] => AccountsCommand::Switch {
            agent: (*agent).to_owned(),
            target: (*target).to_owned(),
        },
        ["import", file] => AccountsCommand::Import {
            file: (*file).to_owned(),
        },
        _ => return None,
    };
    Some(Ok(command))
}

fn daemon_args(config: &Path, command: &AccountsCommand) -> Vec<OsString> {
    let mut args = vec![
        "accounts".into(),
        "--config".into(),
        config.as_os_str().into(),
        "--".into(),
    ];
    match command {
        AccountsCommand::Switch { agent, target } => {
            args.push("switch".into());
            args.push(agent.into());
            args.push(target.into());
        }
        AccountsCommand::Import { file } => {
            args.push("import".into());
            args.push(file.into());
        }
    }
    args
}

/// Runs the command through `hofundd accounts`.
pub fn run(config: &Path, command: &AccountsCommand) -> Result<i32, String> {
    let daemon = serve::locate_installed()?;
    let mut process = std::process::Command::new(&daemon);
    process.args(daemon_args(config, command));
    serve::replace(process, &daemon)
}

pub const ACCOUNTS_HELP: &str = "\
usage: hofund accounts switch <agent> <next|account> [--config PATH]
       hofund accounts import <file> [--config PATH]

`switch` moves the agent named by a subscription plugin to another remembered
account. `next` picks the next account in order with allowance room, or the
one with the most room left when every account is past its line. An account
name, email, id, or alias works too.

`import` reads an account export and stores it with the subscription that
understands that file.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d27_10_accounts_switch_parses() {
        let command = parse(&["switch", "agent", "next"])
            .expect("some")
            .expect("ok");
        assert_eq!(
            command,
            AccountsCommand::Switch {
                agent: "agent".into(),
                target: "next".into(),
            }
        );
    }

    #[test]
    fn d27_11_accounts_import_parses() {
        let command = parse(&["import", "export.json"])
            .expect("some")
            .expect("ok");
        assert_eq!(
            command,
            AccountsCommand::Import {
                file: "export.json".into(),
            }
        );
    }
}
