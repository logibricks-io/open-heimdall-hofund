//! Command-line interface for hofund.
//!
//! The binary parses arguments and calls the daemon. It does not route and it
//! does not encode a protocol.

mod serve;
mod subscription;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hofund_agent::{GatewayTarget, Session, catalog};
use hofund_daemon::{
    agent_descriptions, check, local_gateway, route_models, serve_blocking, token_rotate,
    token_show, usage_report, user_config_path,
};
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
    let result = match command {
        Command::Version => {
            println!("hofund {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Command::Serve { in_process: true } => serve_blocking(&config),
        Command::Serve { in_process: false } => match serve::run(&config) {
            Ok(code) => std::process::exit(code),
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::FAILURE;
            }
        },
        Command::Help(topic) => {
            println!("{}", help_text(topic));
            return ExitCode::SUCCESS;
        }
        Command::Check => return finish(check_config(&config)),
        Command::TokenShow => token_show(&config).map(|token| println!("{token}")),
        Command::TokenRotate => return finish(rotate_token(&config)),
        Command::Usage => usage_report(&config).map(|table| print!("{table}")),
        Command::AgentList => return finish(print_agent_list()),
        Command::AgentConnect { name, dry_run } => {
            return agent_command(&name, |adapter| connect(adapter, &config, dry_run));
        }
        Command::AgentDisconnect { name, force } => {
            return agent_command(&name, |adapter| disconnect(adapter, force));
        }
        Command::AgentStatus => {
            return finish(agent_status(&config));
        }
        Command::Subscription(command) => return subscription::run(&config, command),
    };
    finish(result)
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
    AgentStatus,
    Subscription(SubscriptionCommand),
}

#[derive(Debug, PartialEq, Eq)]
enum HelpTopic {
    Overview,
    Serve,
    ConfigCheck,
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
        ["agent", "status"] => Command::AgentStatus,
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
        ["config", ..] => HelpTopic::ConfigCheck,
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
  config check                    load the config and build its providers
  token show                      print the bearer token
  token rotate                    replace the bearer token
  usage                           print the usage table
  agent list                      list coding-agent adapters
  agent connect <name> [--dry-run]
  agent disconnect <name> [--force]
  agent status
  subscription list [--json]      list subscriptions and their accounts
  subscription sign-in <id> [--host HOST] [--label LABEL]
  subscription sign-out <id> <account>
  subscription refresh <id> [<account>]
  subscription quota [<id>] [--json]

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

Load the config and build the providers it names. On success the command
prints `config ok:` and the path it read. On failure the message starts
with that path.
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
";

const AGENT_HELP: &str = "\
usage: hofund agent list
       hofund agent connect <name> [--dry-run] [--config PATH]
       hofund agent disconnect <name> [--force]
       hofund agent status [--config PATH]

Adapters: claude-code, codex, gemini-cli, opencode.
";

const AGENT_LIST_HELP: &str = "\
usage: hofund agent list

Print each adapter id, display name, and whether it is installed.
";

const AGENT_CONNECT_HELP: &str = "\
usage: hofund agent connect <name> [--dry-run] [--config PATH]

Point <name> at the local gateway. --dry-run prints the diff and writes
nothing. The bearer token is shown as [redacted] in that diff. When the
tool's configured model has no route, a warning is printed.
";

const AGENT_DISCONNECT_HELP: &str = "\
usage: hofund agent disconnect <name> [--force]

Remove the keys connect wrote, on the files as they are now. Edits made
while connected stay. --force restores the connect-time backup instead.
A second disconnect prints a note and exits 0.
";

const AGENT_STATUS_HELP: &str = "\
usage: hofund agent status [--config PATH]

Print whether each adapter is installed and whether its files match the
current gateway token.
";

fn finish<E: std::fmt::Display>(result: Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn agent_catalog() -> Result<hofund_agent::Catalog, String> {
    catalog(agent_descriptions()).map_err(|error| error.to_string())
}

fn agent_command(
    name: &str,
    run: impl FnOnce(&dyn hofund_agent::AgentAdapter) -> Result<(), String>,
) -> ExitCode {
    let catalog = match agent_catalog() {
        Ok(catalog) => catalog,
        Err(error) => return finish(Err(error)),
    };
    let Some(adapter) = catalog.find(name) else {
        eprintln!("unknown agent `{name}`");
        return ExitCode::from(2);
    };
    finish(run(adapter))
}

fn print_agent_list() -> Result<(), String> {
    let catalog = agent_catalog()?;
    let session = Session::from_process_env();
    println!("id\tname\tinstalled");
    for adapter in catalog.adapters() {
        let installed = if adapter.detect(&session) {
            "yes"
        } else {
            "no"
        };
        println!("{}\t{}\t{installed}", adapter.id(), adapter.display_name());
    }
    Ok(())
}

fn check_config(config: &Path) -> Result<(), String> {
    match check(config) {
        Ok(()) => {
            println!("config ok: {}", config.display());
            Ok(())
        }
        Err(error) => Err(format!("{}: {error}", config.display())),
    }
}

fn rotate_token(config: &Path) -> Result<(), String> {
    let catalog = agent_catalog()?;
    let session = Session::from_process_env();
    let local = local_gateway(config).map_err(|error| error.to_string())?;
    let models = route_models(config).map_err(|error| error.to_string())?;
    let previous = GatewayTarget::new(local.base_url.clone(), local.token, models.clone());
    let mut connected = Vec::new();
    let mut failed = Vec::new();
    for adapter in catalog.adapters() {
        match adapter.status(&session, &previous) {
            Ok(status) if status.connected() => connected.push(adapter.id()),
            Ok(_) => {}
            Err(error) => failed.push((adapter.id(), error.to_string())),
        }
    }
    let token = token_rotate(config).map_err(|error| error.to_string())?;
    println!("{token}");
    eprintln!("a running gateway reads the token file on the next request");
    let updated = GatewayTarget::new(local.base_url, token, models);
    for id in connected {
        let Some(adapter) = catalog.find(id) else {
            continue;
        };
        match adapter.refresh_token(&session, &updated) {
            Ok(paths) => {
                for path in paths {
                    println!("{id}: updated {}", path.display());
                }
            }
            Err(error) => failed.push((id, error.to_string())),
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    for (id, error) in &failed {
        eprintln!("{id}: {error}");
        eprintln!("run: hofund agent connect {id}");
    }
    Err("the token file was rotated; a connected agent still needs `hofund agent connect`".into())
}

fn connect(
    adapter: &dyn hofund_agent::AgentAdapter,
    config: &Path,
    dry_run: bool,
) -> Result<(), String> {
    let session = Session::from_process_env();
    let target = gateway_target(config)?;
    let plan = if dry_run {
        adapter.plan(&session, &target)
    } else {
        adapter.apply(&session, &target)
    }
    .map_err(|error| error.to_string())?;
    if plan.changes().is_empty() {
        println!("{}: already matches", adapter.id());
    } else {
        for change in plan.changes() {
            if dry_run {
                print!("{}", redact_diff(change.diff(), target.token()));
            } else {
                println!("{}: wrote {}", adapter.id(), change.path().display());
            }
        }
    }
    if let Some(note) = connect_models_note(adapter.id(), target.models().len()) {
        println!("{note}");
    }
    warn_unrouted_model(adapter, &session, config)
}

fn connect_models_note(adapter_id: &str, model_count: usize) -> Option<String> {
    (adapter_id == "opencode").then(|| opencode_models_note(model_count))
}

/// What `hofund agent connect opencode` prints about the models object.
fn opencode_models_note(model_count: usize) -> String {
    if model_count == 0 {
        "opencode: models stays {}. The config has no routes, and this build does not invent a model catalog.".to_owned()
    } else {
        format!(
            "opencode: models lists {model_count} route name(s), each with name and an empty variants object. Modalities, context limits, and reasoning levels are not invented."
        )
    }
}

fn gateway_target(config: &Path) -> Result<GatewayTarget, String> {
    let local = local_gateway(config).map_err(|error| error.to_string())?;
    let models = route_models(config).map_err(|error| error.to_string())?;
    Ok(GatewayTarget::new(local.base_url, local.token, models))
}

fn redact_diff(diff: &str, token: &str) -> String {
    if token.is_empty() {
        diff.to_owned()
    } else {
        diff.replace(token, "[redacted]")
    }
}

fn warn_unrouted_model(
    adapter: &dyn hofund_agent::AgentAdapter,
    session: &Session,
    config: &Path,
) -> Result<(), String> {
    let Some(model) = adapter
        .configured_model(session)
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let routes = route_models(config).map_err(|error| error.to_string())?;
    if !model_has_route(adapter.model_prefix(), &model, &routes) {
        eprintln!(
            "warning: {} model `{model}` has no route in {}",
            adapter.id(),
            config.display()
        );
    }
    Ok(())
}

fn model_has_route(model_prefix: Option<&str>, model: &str, routes: &[String]) -> bool {
    if let Some(prefix) = model_prefix {
        return model
            .strip_prefix(prefix)
            .is_some_and(|model| routes.iter().any(|route| route == model));
    }
    routes.iter().any(|route| route == model)
}

fn disconnect(adapter: &dyn hofund_agent::AgentAdapter, force: bool) -> Result<(), String> {
    let session = Session::from_process_env();
    let outcome = adapter
        .disconnect(&session, force)
        .map_err(|error| error.to_string())?;
    for warning in outcome.warnings() {
        eprintln!("{warning}");
    }
    if outcome.changed() {
        println!("{}: restored", adapter.id());
    } else {
        println!("{}: nothing to restore", adapter.id());
    }
    Ok(())
}

fn agent_status(config: &Path) -> Result<(), String> {
    let catalog = agent_catalog()?;
    let session = Session::from_process_env();
    let target = gateway_target(config)?;
    println!("id\tinstalled\tconnected");
    for adapter in catalog.adapters() {
        let status = adapter
            .status(&session, &target)
            .map_err(|error| error.to_string())?;
        let installed = if status.installed() { "yes" } else { "no" };
        let connected = if status.connected() { "yes" } else { "no" };
        println!("{}\t{installed}\t{connected}", status.id());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Command, HelpTopic, SubscriptionCommand, connect_models_note, help_text, help_topic,
        model_has_route, opencode_models_note, parse, redact_diff,
    };
    use hofund_daemon::SignInOptions;
    use std::path::PathBuf;

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn compiled_opencode_triggers_the_models_note() {
        let catalog = hofund_agent::catalog(hofund_daemon::agent_descriptions())
            .expect("compiled agent catalog");
        let adapter = catalog.find("opencode").expect("compiled opencode");
        for model_count in [0, 2] {
            assert_eq!(
                connect_models_note(adapter.id(), model_count),
                Some(opencode_models_note(model_count))
            );
        }
        for adapter_id in ["codex", "claude-code", "gemini-cli"] {
            assert_eq!(connect_models_note(adapter_id, 2), None);
        }
    }

    #[test]
    fn route_matching_uses_the_compiled_model_prefix() {
        let catalog = hofund_agent::catalog(hofund_daemon::agent_descriptions())
            .expect("compiled agent catalog");
        let opencode = catalog.find("opencode").expect("compiled opencode");
        let codex = catalog.find("codex").expect("compiled codex");
        let routes = vec!["route-a".to_owned()];
        assert!(model_has_route(
            opencode.model_prefix(),
            "hofund/route-a",
            &routes
        ));
        assert!(!model_has_route(
            opencode.model_prefix(),
            "other/route-a",
            &routes
        ));
        // OpenCode spells selections as provider/model, so a bare model warns.
        assert!(!model_has_route(
            opencode.model_prefix(),
            "route-a",
            &routes
        ));
        assert!(!model_has_route(
            codex.model_prefix(),
            "hofund/route-a",
            &routes
        ));
        assert!(model_has_route(codex.model_prefix(), "route-a", &routes));
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
            Command::AgentStatus
        );
        assert_eq!(
            parse(&words(&[])).expect("parse").0,
            hofund_daemon::user_config_path()
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
        assert_eq!(
            redact_diff("token = \"local-token\"\n", "local-token"),
            "token = \"[redacted]\"\n"
        );
        let note = opencode_models_note(0);
        assert!(note.contains("models stays {}"));
        assert!(note.contains("catalog"));
        let listed = opencode_models_note(2);
        assert!(listed.contains("2 route"));
        assert!(listed.contains("variants"));
    }
}
