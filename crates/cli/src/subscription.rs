//! The `hofund subscription` commands: argument shapes, help, and the text
//! tables. The work happens in `hofund-daemon`.

use std::path::Path;
use std::process::ExitCode;

use hofund_daemon::{
    QuotaListing, SignInOptions, SubscriptionListing, quota_json, subscription_list,
    subscription_quota, subscription_refresh, subscription_sign_in, subscription_sign_out,
    subscriptions_json,
};

/// One parsed `hofund subscription` command.
#[derive(Debug, PartialEq, Eq)]
pub enum SubscriptionCommand {
    /// `subscription list [--json]`
    List { json: bool },
    /// `subscription sign-in <id> [--host H] [--label L]`
    SignIn { id: String, options: SignInOptions },
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
            options: SignInOptions {
                host: flags.host.clone(),
                label: flags.label.clone(),
            },
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

/// Runs one subscription command and returns the process exit code:
/// 0 on success, 1 on failure, 130 when a sign-in was interrupted.
pub fn run(config: &Path, command: SubscriptionCommand) -> ExitCode {
    let result = match command {
        SubscriptionCommand::List { json } => subscription_list(config).map(|listings| {
            if json {
                println!("{}", subscriptions_json(&listings));
            } else {
                print!("{}", list_table(&listings));
            }
        }),
        SubscriptionCommand::SignIn { id, options } => {
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
pub fn list_table(listings: &[SubscriptionListing]) -> String {
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
pub fn quota_table(listings: &[QuotaListing]) -> String {
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
    use hofund_daemon::{AccountListing, QuotaLine, QuotaListing, SubscriptionListing};

    use super::{list_table, quota_table};

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
}
