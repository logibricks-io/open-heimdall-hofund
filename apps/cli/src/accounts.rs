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
    /// List or import agent sign-ins.
    Discover { import: bool },
    /// Name a stored Google account's Cloud project.
    Project {
        subscription: String,
        email: String,
        project: String,
    },
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
        ["discover"] => AccountsCommand::Discover { import: false },
        ["discover", "--add"] => AccountsCommand::Discover { import: true },
        ["project", subscription, email, project]
            if matches!(*subscription, "gemini" | "antigravity") =>
        {
            AccountsCommand::Project {
                subscription: (*subscription).to_owned(),
                email: (*email).to_owned(),
                project: (*project).to_owned(),
            }
        }
        ["project", _, _, _] => {
            return Some(Err(
                "only Gemini CLI and Antigravity accounts have a Google Cloud project".to_owned(),
            ));
        }
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
        AccountsCommand::Discover { import: true } => {
            args.push("discover".into());
            args.push("--add".into());
        }
        AccountsCommand::Discover { import: false } => {
            args.push("discover".into());
        }
        AccountsCommand::Project {
            subscription,
            email,
            project,
        } => {
            args.push("project".into());
            args.push(subscription.into());
            args.push(email.into());
            args.push(project.into());
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
       hofund accounts discover [--add] [--config PATH]
       hofund accounts project <gemini|antigravity> <email> <gcp-project-id> [--config PATH]

`switch` moves the agent named by a subscription plugin to another remembered
account. `next` picks the next account in order with allowance room, or the
one with the most room left when every account is past its line. An account
name, email, id, or alias works too.

`import` reads an account export and stores it with the subscription that
understands that file.

`discover` lists accounts found in configured agents' own sign-in files.
`--add` imports any that are not already stored. Set `discover = true` under
`[subscriptions.options]` to import them when the daemon starts instead.
`HOFUND_NO_ACCOUNTS=1` disables every discovery read.

`project` names the Google Cloud project a Gemini or Antigravity account uses.
Surrounding whitespace is trimmed. Pass an empty project to let Code Assist pick.
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

    #[test]
    fn a1_10_accounts_discover_parses() {
        let command = parse(&["discover"]).expect("some").expect("ok");
        assert_eq!(command, AccountsCommand::Discover { import: false });
        let command = parse(&["discover", "--add"]).expect("some").expect("ok");
        assert_eq!(command, AccountsCommand::Discover { import: true });
    }

    #[test]
    fn r49_accounts_project_parses_and_forwards_all_operands() {
        let command = parse(&["project", "gemini", "u@example.test", "project-1"])
            .expect("some")
            .expect("ok");
        assert_eq!(
            command,
            AccountsCommand::Project {
                subscription: "gemini".into(),
                email: "u@example.test".into(),
                project: "project-1".into(),
            }
        );
        let args = daemon_args(Path::new("hofund.toml"), &command);
        let args: Vec<String> = args
            .into_iter()
            .map(|word| word.into_string().expect("utf-8"))
            .collect();
        assert_eq!(
            args,
            [
                "accounts",
                "--config",
                "hofund.toml",
                "--",
                "project",
                "gemini",
                "u@example.test",
                "project-1",
            ]
        );
        assert!(parse(&["project", "gemini", "u@example.test"]).is_none());
        let error = parse(&["project", "codex", "u@example.test", "project-1"])
            .expect("project form")
            .expect_err("not a Google subscription");
        assert_eq!(
            error,
            "only Gemini CLI and Antigravity accounts have a Google Cloud project"
        );
    }
}
