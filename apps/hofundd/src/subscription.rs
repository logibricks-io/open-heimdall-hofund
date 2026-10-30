//! `hofundd subscription`: the subscription commands, run in this process
//! because sign-in and token refresh need the linked plugin registry.
//!
//! `hofund subscription` validates its own arguments and hands a canonical
//! command line to this entry point, which parses it again and does the work.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hofund_daemon::{
    QuotaListing, SignInOptions, SubscriptionListing, quota_json, subscription_list,
    subscription_quota, subscription_refresh, subscription_sign_in, subscription_sign_out,
    subscriptions_json,
};

/// Usage of the subscription entry point.
pub(crate) const USAGE: &str = "\
Usage: hofundd subscription [--config <path>] list [--json]
       hofundd subscription [--config <path>] sign-in <id> [--host <host>] [--label <label>]
       hofundd subscription [--config <path>] sign-out <id> <account>
       hofundd subscription [--config <path>] refresh <id> [<account>]
       hofundd subscription [--config <path>] quota [<id>] [--json]

The hofund CLI runs these for `hofund subscription`; see `hofund subscription --help`.
";

/// One parsed subscription command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionCommand {
    /// `list [--json]`
    List { json: bool },
    /// `sign-in <id> [--host H] [--label L]`
    SignIn {
        id: String,
        host: Option<String>,
        label: Option<String>,
    },
    /// `sign-out <id> <account>`
    SignOut { id: String, account: String },
    /// `refresh <id> [<account>]`
    Refresh { id: String, account: Option<String> },
    /// `quota [<id>] [--json]`
    Quota { id: Option<String>, json: bool },
}

/// What `hofundd subscription ...` asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    /// `--help` anywhere.
    Help,
    /// A command and its optional config path.
    Run(Option<PathBuf>, SubscriptionCommand),
}

/// Parses the arguments after `subscription`.
///
/// # Errors
///
/// Returns the message to print, with exit status 2, for a malformed line.
pub(crate) fn parse(args: &[OsString]) -> Result<Parsed, String> {
    let mut config = None;
    let mut json = false;
    let mut host = None;
    let mut label = None;
    let mut words: Vec<String> = Vec::new();
    let mut operands_only = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            return Err(format!("argument `{}` is not UTF-8", arg.to_string_lossy()));
        };
        if operands_only {
            words.push(text.to_owned());
            continue;
        }
        let mut value = |flag: &str| {
            args.next()
                .and_then(|value| value.to_str())
                .map(str::to_owned)
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match text {
            "--" => operands_only = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "--config" => {
                if config.is_some() {
                    return Err("--config is given more than once".to_owned());
                }
                config = Some(PathBuf::from(value("--config")?));
            }
            "--json" => json = true,
            "--host" => host = Some(value("--host")?),
            "--label" => label = Some(value("--label")?),
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            word => words.push(word.to_owned()),
        }
    }
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let owned = |word: &&str| (*word).to_owned();
    let command = match words.as_slice() {
        ["list"] => SubscriptionCommand::List { json },
        ["sign-in", id] => SubscriptionCommand::SignIn {
            id: owned(id),
            host: host.take(),
            label: label.take(),
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
        ["quota"] => SubscriptionCommand::Quota { id: None, json },
        ["quota", id] => SubscriptionCommand::Quota {
            id: Some(owned(id)),
            json,
        },
        _ => return Err(format!("unknown subscription command\n\n{USAGE}")),
    };
    let takes_json = matches!(
        command,
        SubscriptionCommand::List { .. } | SubscriptionCommand::Quota { .. }
    );
    if json && !takes_json {
        return Err("--json is only valid with list and quota".to_owned());
    }
    if host.is_some() || label.is_some() {
        return Err("--host and --label are only valid with sign-in".to_owned());
    }
    Ok(Parsed::Run(config, command))
}

/// Runs one subscription command and returns the process exit code:
/// 0 on success, 1 on failure, 130 when a sign-in was interrupted.
pub(crate) fn run(config: &Path, command: SubscriptionCommand) -> ExitCode {
    let result = match command {
        SubscriptionCommand::List { json } => subscription_list(config).map(|listings| {
            if json {
                println!("{}", subscriptions_json(&listings));
            } else {
                print!("{}", list_table(&listings));
            }
        }),
        SubscriptionCommand::SignIn { id, host, label } => {
            let options = SignInOptions { host, label };
            return match subscription_sign_in(config, &id, &options) {
                Ok(account) => {
                    println!("{id}: signed in as {account}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::from(error.exit_code())
                }
            };
        }
        SubscriptionCommand::SignOut { id, account } => {
            subscription_sign_out(config, &id, &account)
                .map(|()| println!("{id}: signed out {account}"))
        }
        SubscriptionCommand::Refresh { id, account } => {
            subscription_refresh(config, &id, account.as_deref()).map(|refreshed| {
                for account in refreshed {
                    println!("{id}: refreshed {account}");
                }
            })
        }
        SubscriptionCommand::Quota { id, json } => {
            subscription_quota(config, id.as_deref()).map(|listings| {
                if json {
                    println!("{}", quota_json(&listings));
                } else {
                    print!("{}", quota_table(&listings));
                }
            })
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

/// The `subscription list` table: one row per account, and one `-` row for
/// a subscription with no account.
pub(crate) fn list_table(listings: &[SubscriptionListing]) -> String {
    let mut table = String::from("subscription\tflow\taccount\tlabel\tstatus\n");
    for listing in listings {
        if listing.accounts.is_empty() {
            table.push_str(&format!(
                "{}\t{}\t-\t-\tno account\n",
                listing.id, listing.flow
            ));
        }
        for account in &listing.accounts {
            let status = account.needs_sign_in.as_ref().map_or_else(
                || "ready".to_owned(),
                |reason| format!("needs sign-in: {reason}"),
            );
            table.push_str(&format!(
                "{}\t{}\t{}\t{}\t{status}\n",
                listing.id, listing.flow, account.id, account.label
            ));
        }
    }
    table
}

/// The `subscription quota` table: one row per window, or an error row.
pub(crate) fn quota_table(listings: &[QuotaListing]) -> String {
    let mut table =
        String::from("subscription\taccount\twindow\tused\tlimit\tresets in\tblocking\n");
    for listing in listings {
        match &listing.windows {
            Ok(windows) if windows.is_empty() => table.push_str(&format!(
                "{}\t{}\t-\t-\t-\t-\t-\n",
                listing.subscription, listing.account
            )),
            Ok(windows) => {
                for window in windows {
                    let limit = window
                        .limit
                        .map_or_else(|| "-".to_owned(), |limit| limit.to_string());
                    let resets = window
                        .resets_in_secs
                        .map_or_else(|| "-".to_owned(), |secs| format!("{secs}s"));
                    let name = window.scope_model.as_ref().map_or_else(
                        || window.name.clone(),
                        |model| format!("{} ({model})", window.name),
                    );
                    let blocking = if window.blocking { "yes" } else { "no" };
                    table.push_str(&format!(
                        "{}\t{}\t{name}\t{}\t{limit}\t{resets}\t{blocking}\n",
                        listing.subscription, listing.account, window.used
                    ));
                }
            }
            Err(error) => table.push_str(&format!(
                "{}\t{}\terror: {error}\n",
                listing.subscription, listing.account
            )),
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use hofund_daemon::{AccountListing, QuotaLine, QuotaListing, SubscriptionListing};

    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{Parsed, SubscriptionCommand, list_table, parse, quota_table};

    #[test]
    fn list_table_rows() {
        let listings = vec![
            SubscriptionListing {
                id: "codex-sub".to_owned(),
                name: "Codex".to_owned(),
                flow: "pkce",
                accounts: vec![
                    AccountListing {
                        id: "a-1".to_owned(),
                        label: "work".to_owned(),
                        needs_sign_in: None,
                    },
                    AccountListing {
                        id: "a-2".to_owned(),
                        label: "home".to_owned(),
                        needs_sign_in: Some("token expired".to_owned()),
                    },
                ],
            },
            SubscriptionListing {
                id: "empty".to_owned(),
                name: "Empty".to_owned(),
                flow: "device",
                accounts: Vec::new(),
            },
        ];
        assert_eq!(
            list_table(&listings),
            "subscription\tflow\taccount\tlabel\tstatus\n\
             codex-sub\tpkce\ta-1\twork\tready\n\
             codex-sub\tpkce\ta-2\thome\tneeds sign-in: token expired\n\
             empty\tdevice\t-\t-\tno account\n"
        );
    }

    #[test]
    fn quota_table_rows() {
        let listings = vec![
            QuotaListing {
                subscription: "s".to_owned(),
                account: "a".to_owned(),
                windows: Ok(vec![QuotaLine {
                    name: "weekly".to_owned(),
                    used: 4,
                    limit: Some(10),
                    resets_in_secs: Some(60),
                    blocking: true,
                    scope_model: Some("m".to_owned()),
                }]),
            },
            QuotaListing {
                subscription: "s".to_owned(),
                account: "b".to_owned(),
                windows: Err("offline".to_owned()),
            },
        ];
        assert_eq!(
            quota_table(&listings),
            "subscription\taccount\twindow\tused\tlimit\tresets in\tblocking\n\
             s\ta\tweekly (m)\t4\t10\t60s\tyes\n\
             s\tb\terror: offline\n"
        );
    }

    fn parsed(words: &[&str]) -> Result<Parsed, String> {
        let args: Vec<OsString> = words.iter().map(OsString::from).collect();
        parse(&args)
    }

    #[test]
    fn parses_every_canonical_form() {
        assert_eq!(
            parsed(&["--config", "/c.toml", "list", "--json"]),
            Ok(Parsed::Run(
                Some(PathBuf::from("/c.toml")),
                SubscriptionCommand::List { json: true }
            ))
        );
        assert_eq!(
            parsed(&["sign-in", "s", "--host", "h", "--label", "l"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::SignIn {
                    id: "s".to_owned(),
                    host: Some("h".to_owned()),
                    label: Some("l".to_owned()),
                }
            ))
        );
        assert_eq!(
            parsed(&["sign-out", "s", "a"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::SignOut {
                    id: "s".to_owned(),
                    account: "a".to_owned(),
                }
            ))
        );
        assert_eq!(
            parsed(&["refresh", "s"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::Refresh {
                    id: "s".to_owned(),
                    account: None,
                }
            ))
        );
        assert_eq!(
            parsed(&["refresh", "s", "a"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::Refresh {
                    id: "s".to_owned(),
                    account: Some("a".to_owned()),
                }
            ))
        );
        assert_eq!(
            parsed(&["quota"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::Quota {
                    id: None,
                    json: false,
                }
            ))
        );
        assert_eq!(
            parsed(&["quota", "s", "--json"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::Quota {
                    id: Some("s".to_owned()),
                    json: true,
                }
            ))
        );
        assert_eq!(parsed(&["list", "--help"]), Ok(Parsed::Help));
        assert_eq!(
            parsed(&["--label", "l", "--", "sign-in", "-s"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::SignIn {
                    id: "-s".to_owned(),
                    host: None,
                    label: Some("l".to_owned()),
                }
            ))
        );
        assert_eq!(
            parsed(&["--", "sign-out", "s", "--help"]),
            Ok(Parsed::Run(
                None,
                SubscriptionCommand::SignOut {
                    id: "s".to_owned(),
                    account: "--help".to_owned(),
                }
            ))
        );
    }

    #[test]
    fn rejects_misplaced_flags_and_unknown_forms() {
        assert_eq!(
            parsed(&["sign-out", "s", "a", "--json"]),
            Err("--json is only valid with list and quota".to_owned())
        );
        assert_eq!(
            parsed(&["list", "--host", "h"]),
            Err("--host and --label are only valid with sign-in".to_owned())
        );
        assert_eq!(
            parsed(&["quota", "--label", "l"]),
            Err("--host and --label are only valid with sign-in".to_owned())
        );
        assert_eq!(
            parsed(&["list", "--config", "a", "--config", "b"]),
            Err("--config is given more than once".to_owned())
        );
        assert_eq!(
            parsed(&["sign-in", "s", "--host"]),
            Err("--host needs a value".to_owned())
        );
        assert_eq!(
            parsed(&["list", "--all"]),
            Err("unknown flag `--all`".to_owned())
        );
        let error = parsed(&["list", "extra"]).expect_err("extra word");
        assert!(error.starts_with("unknown subscription command"), "{error}");
        assert!(error.contains("Usage: hofundd subscription"), "{error}");
    }
}
