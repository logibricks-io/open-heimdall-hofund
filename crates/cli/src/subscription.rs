//! The `hofund subscription` commands: argument shapes and help. The work
//! happens in `hofundd subscription`, which links the plugin registry; this
//! module hands it a canonical command line and leaves with its exit code.

use std::ffi::OsString;
use std::path::Path;

use crate::serve;

/// One parsed `hofund subscription` command.
#[derive(Debug, PartialEq, Eq)]
pub enum SubscriptionCommand {
    /// `subscription list [--json]`
    List { json: bool },
    /// `subscription sign-in <id> [--host H] [--label L]`
    SignIn {
        id: String,
        host: Option<String>,
        label: Option<String>,
    },
    /// `subscription sign-out <id> <account>`
    SignOut { id: String, account: String },
    /// `subscription refresh <id> [<account>]`
    Refresh { id: String, account: Option<String> },
    /// `subscription quota [<id>] [--json]`
    Quota { id: Option<String>, json: bool },
}

/// Flags only the subscription commands take.
#[derive(Debug, Default)]
pub struct SubscriptionFlags {
    pub json: bool,
    pub host: Option<String>,
    pub label: Option<String>,
}

/// Parses the words after `subscription`. `None` means the words name no
/// subscription command.
pub fn parse(
    words: &[&str],
    flags: SubscriptionFlags,
) -> Option<Result<SubscriptionCommand, String>> {
    let owned = |word: &&str| (*word).to_owned();
    let command = match words {
        ["list"] => SubscriptionCommand::List { json: flags.json },
        ["sign-in", id] => SubscriptionCommand::SignIn {
            id: owned(id),
            host: flags.host.clone(),
            label: flags.label.clone(),
        },
        ["sign-out", id, account] => SubscriptionCommand::SignOut {
            id: owned(id),
            account: owned(account),
        },
        ["refresh", id] => SubscriptionCommand::Refresh {
            id: owned(id),
            account: None,
        },
        ["refresh", id, account] => SubscriptionCommand::Refresh {
            id: owned(id),
            account: Some(owned(account)),
        },
        ["quota"] => SubscriptionCommand::Quota {
            id: None,
            json: flags.json,
        },
        ["quota", id] => SubscriptionCommand::Quota {
            id: Some(owned(id)),
            json: flags.json,
        },
        _ => return None,
    };
    let takes_json = matches!(
        command,
        SubscriptionCommand::List { .. } | SubscriptionCommand::Quota { .. }
    );
    let signs_in = matches!(command, SubscriptionCommand::SignIn { .. });
    if flags.json && !takes_json {
        return Some(Err(
            "--json is only valid with subscription list and subscription quota".to_owned(),
        ));
    }
    if (flags.host.is_some() || flags.label.is_some()) && !signs_in {
        return Some(Err(
            "--host and --label are only valid with subscription sign-in".to_owned(),
        ));
    }
    Some(Ok(command))
}

/// The `hofundd` arguments for `command` on `config`:
/// `subscription --config <config>`, the flags, `--`, then the command words,
/// so an id or account that starts with `-` stays an operand.
pub fn daemon_args(config: &Path, command: &SubscriptionCommand) -> Vec<OsString> {
    let mut flags: Vec<&str> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    match command {
        SubscriptionCommand::List { json } => {
            words.push("list");
            if *json {
                flags.push("--json");
            }
        }
        SubscriptionCommand::SignIn { id, host, label } => {
            words.extend(["sign-in", id.as_str()]);
            if let Some(host) = host {
                flags.extend(["--host", host.as_str()]);
            }
            if let Some(label) = label {
                flags.extend(["--label", label.as_str()]);
            }
        }
        SubscriptionCommand::SignOut { id, account } => {
            words.extend(["sign-out", id.as_str(), account.as_str()]);
        }
        SubscriptionCommand::Refresh { id, account } => {
            words.extend(["refresh", id.as_str()]);
            words.extend(account.as_deref());
        }
        SubscriptionCommand::Quota { id, json } => {
            words.push("quota");
            words.extend(id.as_deref());
            if *json {
                flags.push("--json");
            }
        }
    }
    let mut args: Vec<OsString> = vec!["subscription".into(), "--config".into(), config.into()];
    args.extend(flags.into_iter().map(OsString::from));
    args.push("--".into());
    args.extend(words.into_iter().map(OsString::from));
    args
}

/// Runs `command` through `hofundd subscription` and returns the exit code
/// to leave with: hofundd's own (0, 1, or 130 for an interrupted sign-in).
///
/// # Errors
///
/// Returns a message when `hofundd` cannot be found or started.
pub fn run(config: &Path, command: &SubscriptionCommand) -> Result<i32, String> {
    let daemon = serve::locate_installed()?;
    let mut process = std::process::Command::new(&daemon);
    process.args(daemon_args(config, command));
    serve::replace(process, &daemon)
}

pub const SUBSCRIPTION_HELP: &str = "\
usage: hofund subscription list [--json] [--config PATH]
       hofund subscription sign-in <id> [--host HOST] [--label LABEL] [--config PATH]
       hofund subscription sign-out <id> <account> [--config PATH]
       hofund subscription refresh <id> [<account>] [--config PATH]
       hofund subscription quota [<id>] [--json] [--config PATH]

<id> is a subscription in the config's [[subscriptions]]. A running gateway
picks up a sign-in or sign-out on its next account check.
";

pub const LIST_HELP: &str = "\
usage: hofund subscription list [--json] [--config PATH]

Print each configured subscription, its sign-in flow, and its accounts with
their status. --json prints the same as JSON. No token is printed.
";

pub const SIGN_IN_HELP: &str = "\
usage: hofund subscription sign-in <id> [--host HOST] [--label LABEL] [--config PATH]

Sign in to subscription <id>. The command prints an address to open, or a
device code to enter, and waits up to 10 minutes. When the browser cannot
reach this machine, paste the address it ended on and press Enter.
--host names a vendor host, such as an enterprise domain. --label names the
account. Ctrl-C stops the sign-in and exits 130.
";

pub const SIGN_OUT_HELP: &str = "\
usage: hofund subscription sign-out <id> <account> [--config PATH]

Remove <account> and its stored tokens from subscription <id>.
";

pub const REFRESH_HELP: &str = "\
usage: hofund subscription refresh <id> [<account>] [--config PATH]

Refresh the tokens of <account>, or of every account of <id>.
";

pub const QUOTA_HELP: &str = "\
usage: hofund subscription quota [<id>] [--json] [--config PATH]

Print the quota windows each account reports, for <id> or for every
subscription. --json prints the same as JSON.
";

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;

    use super::{SubscriptionCommand, daemon_args};

    fn args(command: &SubscriptionCommand) -> Vec<String> {
        daemon_args(Path::new("/c.toml"), command)
            .into_iter()
            .map(|arg: OsString| arg.into_string().expect("utf-8"))
            .collect()
    }

    fn expect(tail: &[&str]) -> Vec<String> {
        ["subscription", "--config", "/c.toml"]
            .iter()
            .chain(tail)
            .map(|word| (*word).to_owned())
            .collect()
    }

    #[test]
    fn every_command_becomes_flags_then_words() {
        let id = || "s".to_owned();
        let cases = [
            (
                SubscriptionCommand::List { json: false },
                expect(&["--", "list"]),
            ),
            (
                SubscriptionCommand::List { json: true },
                expect(&["--json", "--", "list"]),
            ),
            (
                SubscriptionCommand::SignIn {
                    id: id(),
                    host: None,
                    label: None,
                },
                expect(&["--", "sign-in", "s"]),
            ),
            (
                SubscriptionCommand::SignIn {
                    id: id(),
                    host: Some("h".to_owned()),
                    label: Some("l".to_owned()),
                },
                expect(&["--host", "h", "--label", "l", "--", "sign-in", "s"]),
            ),
            (
                SubscriptionCommand::SignOut {
                    id: id(),
                    account: "-a".to_owned(),
                },
                expect(&["--", "sign-out", "s", "-a"]),
            ),
            (
                SubscriptionCommand::Refresh {
                    id: id(),
                    account: None,
                },
                expect(&["--", "refresh", "s"]),
            ),
            (
                SubscriptionCommand::Refresh {
                    id: id(),
                    account: Some("a".to_owned()),
                },
                expect(&["--", "refresh", "s", "a"]),
            ),
            (
                SubscriptionCommand::Quota {
                    id: None,
                    json: false,
                },
                expect(&["--", "quota"]),
            ),
            (
                SubscriptionCommand::Quota {
                    id: Some("s".to_owned()),
                    json: true,
                },
                expect(&["--json", "--", "quota", "s"]),
            ),
        ];
        for (command, expected) in cases {
            assert_eq!(args(&command), expected, "{command:?}");
        }
    }
}
