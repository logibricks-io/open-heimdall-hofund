//! Command-line interface for hofund.
//!
//! The binary parses arguments and calls the daemon. It does not route and it
//! does not encode a protocol.

mod commands;
mod connect;
mod local;
mod serve;
mod subscription;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hofund_config::user_config_path;
use hofund_daemon::serve_blocking;
use subscription::{SubscriptionCommand, SubscriptionFlags};

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (config, command) = match parse(&arguments) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Version => {
            println!("hofund {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Command::Serve { in_process: true } => {
            finish(serve_blocking(&config).map(|()| String::new()))
        }
        Command::Serve { in_process: false } => match serve::run(&config) {
            Ok(code) => std::process::exit(code),
            Err(message) => {
                eprintln!("{message}");
                ExitCode::FAILURE
            }
        },
        Command::Help(topic) => {
            println!("{}", help_text(topic));
            ExitCode::SUCCESS
        }
        Command::TokenShow => finish(local::token_show(&config)),
        Command::Subscription(command) => subscription::run(&config, command),
        other => finish(run_control(&config, other)),
    }
}

/// Runs a command that goes through the control plane.
fn run_control(config: &Path, command: Command) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the async runtime: {error}"))?;
    runtime.block_on(async {
        match command {
            Command::Check => commands::check(config).await,
            Command::ConfigApply { source } => commands::apply(config, &source).await,
            Command::ConfigReload => commands::reload(config).await,
            Command::Status => commands::status(config).await,
            Command::Shutdown => commands::shutdown(config).await,
            Command::Healthcheck => commands::healthcheck(config).await,
            Command::TokenRotate => commands::token_rotate(config).await,
            Command::Usage => commands::usage(config)
                .await
                .map(|table| table.trim_end_matches('\n').to_owned()),
            Command::AgentList => commands::agent_list(config).await,
            Command::AgentConnect { name, dry_run } => {
                commands::agent_connect(config, &name, dry_run).await
            }
            Command::AgentDisconnect { name, force } => {
                commands::agent_disconnect(config, &name, force).await
            }
            Command::AgentStatus { name } => commands::agent_status(config, name.as_deref()).await,
            Command::Version
            | Command::Help(_)
            | Command::Serve { .. }
            | Command::TokenShow
            | Command::Subscription(_) => Err("not a control command".to_owned()),
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help(HelpTopic),
    Version,
    /// `in_process` keeps the pre-daemon server; it goes away in the next release.
    Serve {
        in_process: bool,
    },
    Check,
    ConfigApply {
        source: PathBuf,
    },
    ConfigReload,
    Status,
    Shutdown,
    Healthcheck,
    TokenShow,
    TokenRotate,
    Usage,
    AgentList,
    AgentConnect {
        name: String,
        dry_run: bool,
    },
    AgentDisconnect {
        name: String,
        force: bool,
    },
    AgentStatus {
        name: Option<String>,
    },
    Subscription(SubscriptionCommand),
}

#[derive(Debug, PartialEq, Eq)]
enum HelpTopic {
    Overview,
    Serve,
    ConfigCheck,
    ConfigApply,
    ConfigReload,
    Status,
    Shutdown,
    Healthcheck,
    Token,
    TokenShow,
    TokenRotate,
    Usage,
    Agent,
    AgentList,
    AgentConnect,
    AgentDisconnect,
    AgentStatus,
    Subscription,
    SubscriptionList,
    SubscriptionSignIn,
    SubscriptionSignOut,
    SubscriptionRefresh,
    SubscriptionQuota,
}

fn parse(arguments: &[String]) -> Result<(PathBuf, Command), String> {
    let mut config = None;
    let mut dry_run = false;
    let mut force = false;
    let mut in_process = false;
    let mut help = false;
    let mut flags = SubscriptionFlags::default();
    let mut words = Vec::new();
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--config" => {
                let path = iter
                    .next()
                    .ok_or_else(|| "--config needs a path".to_owned())?;
                config = Some(PathBuf::from(path));
            }
            "--dry-run" => dry_run = true,
            "--force" => force = true,
            "--in-process" => in_process = true,
            "--json" => flags.json = true,
            "--host" => {
                let host = iter
                    .next()
                    .ok_or_else(|| "--host needs a value".to_owned())?;
                flags.host = Some(host.clone());
            }
            "--label" => {
                let label = iter
                    .next()
                    .ok_or_else(|| "--label needs a value".to_owned())?;
                flags.label = Some(label.clone());
            }
            "-h" | "--help" => help = true,
            "--version" | "-V" => words.push(argument.as_str()),
            other if other.starts_with('-') => {
                return Err(format!("unknown flag `{other}`"));
            }
            other => words.push(other),
        }
    }
    let config = config.unwrap_or_else(user_config_path);
    if help || words.first() == Some(&"help") {
        let topic_words: Vec<&str> = if words.first() == Some(&"help") {
            words[1..].to_vec()
        } else {
            words.clone()
        };
        return match help_topic(&topic_words) {
            Ok(topic) => Ok((config, Command::Help(topic))),
            Err(message) => Err(message),
        };
    }
    let subscription_flags = flags.json || flags.host.is_some() || flags.label.is_some();
    let command = match words.as_slice() {
        [] | ["version"] | ["--version"] | ["-V"] => Command::Version,
        ["serve"] => Command::Serve { in_process },
        ["config", "check"] => Command::Check,
        ["config", "apply", source] => Command::ConfigApply {
            source: PathBuf::from(source),
        },
        ["config", "reload"] => Command::ConfigReload,
        ["status"] => Command::Status,
        ["shutdown"] => Command::Shutdown,
        ["healthcheck"] => Command::Healthcheck,
        ["token", "show"] => Command::TokenShow,
        ["token", "rotate"] => Command::TokenRotate,
        ["usage"] => Command::Usage,
        ["agent", "list"] => Command::AgentList,
        ["agent", "connect", name] => Command::AgentConnect {
            name: (*name).to_owned(),
            dry_run,
        },
        ["agent", "disconnect", name] => Command::AgentDisconnect {
            name: (*name).to_owned(),
            force,
        },
        ["agent", "status"] => Command::AgentStatus { name: None },
        ["agent", "status", name] => Command::AgentStatus {
            name: Some((*name).to_owned()),
        },
        ["subscription", rest @ ..] => match subscription::parse(rest, flags) {
            Some(parsed) => Command::Subscription(parsed?),
            None => return Err(help_text(HelpTopic::Subscription)),
        },
        _ => return Err(help_text(HelpTopic::Overview)),
    };
    if subscription_flags && !matches!(command, Command::Subscription(_)) {
        return Err(
            "--json, --host, and --label are only valid with subscription commands".to_owned(),
        );
    }
    if dry_run && !matches!(command, Command::AgentConnect { .. }) {
        return Err("--dry-run is only valid with agent connect".to_owned());
    }
    if force && !matches!(command, Command::AgentDisconnect { .. }) {
        return Err("--force is only valid with agent disconnect".to_owned());
    }
    if in_process && !matches!(command, Command::Serve { .. }) {
        return Err("--in-process is only valid with serve".to_owned());
    }
    Ok((config, command))
}

fn help_topic(words: &[&str]) -> Result<HelpTopic, String> {
    let topic = match words {
        [] | ["version"] => HelpTopic::Overview,
        ["serve", ..] => HelpTopic::Serve,
        ["config", "apply", ..] => HelpTopic::ConfigApply,
        ["config", "reload", ..] => HelpTopic::ConfigReload,
        ["config", ..] => HelpTopic::ConfigCheck,
        ["status", ..] => HelpTopic::Status,
        ["shutdown", ..] => HelpTopic::Shutdown,
        ["healthcheck", ..] => HelpTopic::Healthcheck,
        ["token", "show", ..] => HelpTopic::TokenShow,
        ["token", "rotate", ..] => HelpTopic::TokenRotate,
        ["token", ..] => HelpTopic::Token,
        ["usage", ..] => HelpTopic::Usage,
        ["agent", "list", ..] => HelpTopic::AgentList,
        ["agent", "connect", ..] => HelpTopic::AgentConnect,
        ["agent", "disconnect", ..] => HelpTopic::AgentDisconnect,
        ["agent", "status", ..] => HelpTopic::AgentStatus,
        ["agent", ..] => HelpTopic::Agent,
        ["subscription", "list", ..] => HelpTopic::SubscriptionList,
        ["subscription", "sign-in", ..] => HelpTopic::SubscriptionSignIn,
        ["subscription", "sign-out", ..] => HelpTopic::SubscriptionSignOut,
        ["subscription", "refresh", ..] => HelpTopic::SubscriptionRefresh,
        ["subscription", "quota", ..] => HelpTopic::SubscriptionQuota,
        ["subscription", ..] => HelpTopic::Subscription,
        _ => {
            return Err(format!(
                "{}\nunknown command `{}`",
                help_text(HelpTopic::Overview),
                words.join(" ")
            ));
        }
    };
    Ok(topic)
}

fn help_text(topic: HelpTopic) -> String {
    let body = match topic {
        HelpTopic::Overview => OVERVIEW,
        HelpTopic::Serve => SERVE_HELP,
        HelpTopic::ConfigCheck => CHECK_HELP,
        HelpTopic::ConfigApply => APPLY_HELP,
        HelpTopic::ConfigReload => RELOAD_HELP,
        HelpTopic::Status => STATUS_HELP,
        HelpTopic::Shutdown => SHUTDOWN_HELP,
        HelpTopic::Healthcheck => HEALTHCHECK_HELP,
        HelpTopic::Token => TOKEN_HELP,
        HelpTopic::TokenShow => TOKEN_SHOW_HELP,
        HelpTopic::TokenRotate => TOKEN_ROTATE_HELP,
        HelpTopic::Usage => USAGE_HELP,
        HelpTopic::Agent => AGENT_HELP,
        HelpTopic::AgentList => AGENT_LIST_HELP,
        HelpTopic::AgentConnect => AGENT_CONNECT_HELP,
        HelpTopic::AgentDisconnect => AGENT_DISCONNECT_HELP,
        HelpTopic::AgentStatus => AGENT_STATUS_HELP,
        HelpTopic::Subscription => subscription::SUBSCRIPTION_HELP,
        HelpTopic::SubscriptionList => subscription::LIST_HELP,
        HelpTopic::SubscriptionSignIn => subscription::SIGN_IN_HELP,
        HelpTopic::SubscriptionSignOut => subscription::SIGN_OUT_HELP,
        HelpTopic::SubscriptionRefresh => subscription::REFRESH_HELP,
        HelpTopic::SubscriptionQuota => subscription::QUOTA_HELP,
    };
    body.to_owned()
}

const OVERVIEW: &str = "\
hofund — local LLM gateway

usage: hofund [--config PATH] <command>

The default config is hofund/hofund.toml under the platform config directory
(XDG_CONFIG_HOME or ~/.config on Unix, ~/Library/Application Support on macOS,
%APPDATA% on Windows). HOFUND_CONFIG overrides that path. --config overrides both.

commands:
  version                         print the version
  serve                           bind the listen address and serve
  status                          print the daemon status, or a local report
  shutdown                        stop a running daemon
  healthcheck                     probe the model gateway's /health
  config check                    validate the config and build its providers
  config apply <file>             replace the config with <file>
  config reload                   make a running daemon reread the config
  token show                      print the bearer token
  token rotate                    replace the bearer token
  usage                           print the usage table
  agent list                      list coding-agent adapters
  agent connect <name> [--dry-run]
  agent disconnect <name> [--force]
  agent status [<name>]
  subscription list [--json]      list subscriptions and their accounts
  subscription sign-in <id> [--host HOST] [--label LABEL]
  subscription sign-out <id> <account>
  subscription refresh <id> [<account>]
  subscription quota [<id>] [--json]

Commands that read or change runtime state talk to a running daemon. With
no daemon they run hofundd once for the command, from hofund's directory or
PATH. When a daemon holds the config's lock but does not answer within 5 s,
the command fails instead.

hofund <command> --help prints that command.
examples/hofund.toml is a documented config with [[providers]] and [[routes]].
";

const SERVE_HELP: &str = "\
usage: hofund serve [--config PATH] [--in-process]

Run hofundd, the daemon, on the config. hofundd binds the listen address in
the config and serves until Ctrl-C. hofund looks for hofundd in its own
directory first and on PATH second, and passes it --config with the resolved
path. The default config path is the platform config directory. --config
overrides it.

--in-process serves inside this process without hofundd. It is a temporary
compatibility option and goes away in the next release.
";

const CHECK_HELP: &str = "\
usage: hofund config check [--config PATH]

Validate the config and build the providers it names, through the running
daemon or one-shot hofundd. On success the command prints `config ok:` and
the path it read. On failure the message starts with that path. When no
daemon runs and hofundd is not installed, only the file's structure is
checked, and the output says so.
";

const APPLY_HELP: &str = "\
usage: hofund config apply <file> [--config PATH]

Replace the config with the text of <file>. The write is refused when the
config changed on disk since it was read. A running daemon applies what it
can live and lists fields that need a restart.
";

const RELOAD_HELP: &str = "\
usage: hofund config reload [--config PATH]

Make a running daemon reread its config file. With no daemon running this
prints `no daemon running; nothing to reload` and exits 0.
";

const STATUS_HELP: &str = "\
usage: hofund status [--config PATH]

Print the running daemon's pid, model address, config generation, and
revisions. With no daemon running, print a local report: the config path
and id, its disk revision, and the model address with its source
(environment or config), marked `daemon not running`.
";

const SHUTDOWN_HELP: &str = "\
usage: hofund shutdown [--config PATH]

Ask the running daemon to finish in-flight requests and exit. With no daemon
running this prints `no daemon running; nothing to shut down` and exits 0.
";

const HEALTHCHECK_HELP: &str = "\
usage: hofund healthcheck [--config PATH]

Exit 0 when a hofund model gateway answers GET /health at the model address
within 3 s: HOFUND_ADDR when set, otherwise listen in the config. An
unspecified host is probed on loopback. It needs no token and no daemon
discovery, so a container HEALTHCHECK can run it.
";

const TOKEN_HELP: &str = "\
usage: hofund token show [--config PATH]
       hofund token rotate [--config PATH]

`show` prints the bearer token, creating the token file on first use.
`rotate` replaces the token. A running gateway reads the new file on the
next request. Agents that are already connected have their token keys
rewritten. When that edit fails, the command prints `hofund agent connect <name>`.
";

const TOKEN_SHOW_HELP: &str = "\
usage: hofund token show [--config PATH]

Print the bearer token. The token file is created on first use.
";

const TOKEN_ROTATE_HELP: &str = "\
usage: hofund token rotate [--config PATH]

Replace the bearer token. A running gateway reads the token file on the
next request. Connected agents have their token keys rewritten. When an
agent file cannot be updated, the command prints `hofund agent connect <name>`.
";

const USAGE_HELP: &str = "\
usage: hofund usage [--config PATH]

Print one row per recorded request, with local time, plus a totals line.
The newest rows are shown when there are too many to list.
";

const AGENT_HELP: &str = "\
usage: hofund agent list [--config PATH]
       hofund agent connect <name> [--dry-run] [--config PATH]
       hofund agent disconnect <name> [--force] [--config PATH]
       hofund agent status [<name>] [--config PATH]

Adapters: claude-code, codex, gemini-cli, opencode.
";

const AGENT_LIST_HELP: &str = "\
usage: hofund agent list [--config PATH]

Print each adapter id, display name, and whether it is installed.
";

const AGENT_CONNECT_HELP: &str = "\
usage: hofund agent connect <name> [--dry-run] [--config PATH]

Point <name> at the local gateway. --dry-run prints the diff and writes
nothing. The bearer token is shown as [redacted] in that diff. When the
tool's configured model has no route, a warning is printed.
";

const AGENT_DISCONNECT_HELP: &str = "\
usage: hofund agent disconnect <name> [--force] [--config PATH]

Remove the keys connect wrote, on the files as they are now. Edits made
while connected stay. --force restores the connect-time backup instead.
A second disconnect prints a note and exits 0.
";

const AGENT_STATUS_HELP: &str = "\
usage: hofund agent status [<name>] [--config PATH]

Print each adapter's state (connected, disconnected, or drifted) and the
model its files select. <name> limits the output to one adapter.
";

fn finish<E: std::fmt::Display>(result: Result<String, E>) -> ExitCode {
    match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, HelpTopic, SubscriptionCommand, help_text, help_topic, parse};
    use hofund_daemon::SignInOptions;
    use std::path::PathBuf;

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn serve_runs_the_daemon_unless_in_process_is_given() {
        let parsed = parse(&words(&["serve", "--config", "/tmp/h.toml"])).expect("parse");
        assert_eq!(
            parsed,
            (
                PathBuf::from("/tmp/h.toml"),
                Command::Serve { in_process: false }
            )
        );
        let parsed = parse(&words(&["serve", "--in-process"])).expect("parse");
        assert_eq!(parsed.1, Command::Serve { in_process: true });
        assert_eq!(
            parse(&words(&["usage", "--in-process"])),
            Err("--in-process is only valid with serve".to_owned())
        );
        let help = help_text(help_topic(&["serve"]).expect("topic"));
        assert!(help.contains("[--in-process]"), "{help}");
        assert!(help.contains("hofundd"), "{help}");
    }

    fn subscription(values: &[&str]) -> Result<Command, String> {
        parse(&words(values)).map(|(_, command)| command)
    }

    #[test]
    fn subscription_commands_parse() {
        assert_eq!(
            subscription(&["subscription", "list"]),
            Ok(Command::Subscription(SubscriptionCommand::List {
                json: false
            }))
        );
        assert_eq!(
            subscription(&["subscription", "list", "--json"]),
            Ok(Command::Subscription(SubscriptionCommand::List {
                json: true
            }))
        );
        assert_eq!(
            subscription(&[
                "subscription",
                "sign-in",
                "codex-sub",
                "--host",
                "corp.example.test",
                "--label",
                "work",
            ]),
            Ok(Command::Subscription(SubscriptionCommand::SignIn {
                id: "codex-sub".to_owned(),
                options: SignInOptions {
                    host: Some("corp.example.test".to_owned()),
                    label: Some("work".to_owned()),
                },
            }))
        );
        assert_eq!(
            subscription(&["subscription", "sign-out", "codex-sub", "a-1"]),
            Ok(Command::Subscription(SubscriptionCommand::SignOut {
                id: "codex-sub".to_owned(),
                account: "a-1".to_owned(),
            }))
        );
        assert_eq!(
            subscription(&["subscription", "refresh", "codex-sub"]),
            Ok(Command::Subscription(SubscriptionCommand::Refresh {
                id: "codex-sub".to_owned(),
                account: None,
            }))
        );
        assert_eq!(
            subscription(&["subscription", "refresh", "codex-sub", "a-1"]),
            Ok(Command::Subscription(SubscriptionCommand::Refresh {
                id: "codex-sub".to_owned(),
                account: Some("a-1".to_owned()),
            }))
        );
        assert_eq!(
            subscription(&["subscription", "quota", "--json"]),
            Ok(Command::Subscription(SubscriptionCommand::Quota {
                id: None,
                json: true,
            }))
        );
        assert_eq!(
            subscription(&["subscription", "quota", "codex-sub"]),
            Ok(Command::Subscription(SubscriptionCommand::Quota {
                id: Some("codex-sub".to_owned()),
                json: false,
            }))
        );
        assert_eq!(
            subscription(&["subscription", "sign-in", "--help"]),
            Ok(Command::Help(HelpTopic::SubscriptionSignIn))
        );
        assert_eq!(
            subscription(&["help", "subscription"]),
            Ok(Command::Help(HelpTopic::Subscription))
        );
        assert!(subscription(&["subscription", "sign-out", "codex-sub"]).is_err());
        assert!(subscription(&["subscription", "nope"]).is_err());
        assert!(subscription(&["subscription", "refresh", "x", "--json"]).is_err());
        assert!(subscription(&["subscription", "list", "--label", "work"]).is_err());
        assert!(subscription(&["subscription", "quota", "--host", "h"]).is_err());
        assert!(subscription(&["subscription", "sign-in", "x", "--host"]).is_err());
        assert!(subscription(&["agent", "status", "--json"]).is_err());
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parse(&words(&[])).expect("parse").1, Command::Version);
        assert_eq!(
            parse(&words(&["serve", "--config", "a.toml"]))
                .expect("parse")
                .0,
            std::path::PathBuf::from("a.toml")
        );
        assert_eq!(
            parse(&words(&["config", "check"])).expect("parse").1,
            Command::Check
        );
        assert_eq!(
            parse(&words(&["token", "rotate"])).expect("parse").1,
            Command::TokenRotate
        );
        assert_eq!(
            parse(&words(&["agent", "list"])).expect("parse").1,
            Command::AgentList
        );
        assert_eq!(
            parse(&words(&["agent", "connect", "codex", "--dry-run"]))
                .expect("parse")
                .1,
            Command::AgentConnect {
                name: "codex".to_owned(),
                dry_run: true,
            }
        );
        assert_eq!(
            parse(&words(&["agent", "disconnect", "opencode"]))
                .expect("parse")
                .1,
            Command::AgentDisconnect {
                name: "opencode".to_owned(),
                force: false,
            }
        );
        assert_eq!(
            parse(&words(&["agent", "disconnect", "codex", "--force"]))
                .expect("parse")
                .1,
            Command::AgentDisconnect {
                name: "codex".to_owned(),
                force: true,
            }
        );
        assert_eq!(
            parse(&words(&["agent", "status"])).expect("parse").1,
            Command::AgentStatus { name: None }
        );
        assert_eq!(
            parse(&words(&["agent", "status", "codex"]))
                .expect("parse")
                .1,
            Command::AgentStatus {
                name: Some("codex".to_owned())
            }
        );
        assert_eq!(
            parse(&words(&["config", "apply", "new.toml"]))
                .expect("parse")
                .1,
            Command::ConfigApply {
                source: PathBuf::from("new.toml")
            }
        );
        assert_eq!(
            parse(&words(&["config", "reload"])).expect("parse").1,
            Command::ConfigReload
        );
        assert_eq!(
            parse(&words(&["status"])).expect("parse").1,
            Command::Status
        );
        assert_eq!(
            parse(&words(&["shutdown"])).expect("parse").1,
            Command::Shutdown
        );
        assert_eq!(
            parse(&words(&["healthcheck"])).expect("parse").1,
            Command::Healthcheck
        );
        assert!(parse(&words(&["config", "apply"])).is_err());
        for (topic, needle) in [
            (vec!["config", "apply"], "config apply <file>"),
            (vec!["config", "reload"], "nothing to reload"),
            (vec!["status"], "daemon not running"),
            (vec!["shutdown"], "nothing to shut down"),
            (vec!["healthcheck"], "within 3 s"),
            (vec!["config", "check"], "structure"),
        ] {
            let help = help_text(help_topic(&topic).expect("topic"));
            assert!(help.contains(needle), "{topic:?}: {help}");
        }
        assert_eq!(
            parse(&words(&[])).expect("parse").0,
            hofund_config::user_config_path()
        );
        assert_eq!(
            parse(&words(&["--help"])).expect("parse").1,
            Command::Help(HelpTopic::Overview)
        );
        assert_eq!(
            parse(&words(&["serve", "-h"])).expect("parse").1,
            Command::Help(HelpTopic::Serve)
        );
        assert_eq!(
            parse(&words(&["help", "agent", "connect"]))
                .expect("parse")
                .1,
            Command::Help(HelpTopic::AgentConnect)
        );
        assert_eq!(
            parse(&words(&["config", "check", "--help"]))
                .expect("parse")
                .1,
            Command::Help(HelpTopic::ConfigCheck)
        );
        assert!(parse(&words(&["nope"])).is_err());
        assert!(parse(&words(&["agent", "status", "--dry-run"])).is_err());
        assert!(parse(&words(&["serve", "--force"])).is_err());
    }
}
