//! Command-line interface for hofund.
//!
//! The binary parses arguments and calls the daemon. It does not route and it
//! does not encode a protocol.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hofund_agent::{GatewayTarget, Session, adapters, find};
use hofund_daemon::{check, local_gateway, serve_blocking, token_rotate, token_show, usage_report};

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
        Command::Serve => serve_blocking(&config),
        Command::Check => check(&config),
        Command::TokenShow => token_show(&config).map(|token| println!("{token}")),
        Command::TokenRotate => token_rotate(&config).map(|token| println!("{token}")),
        Command::Usage => usage_report(&config).map(|table| print!("{table}")),
        Command::AgentList => {
            print_agent_list();
            return ExitCode::SUCCESS;
        }
        Command::AgentConnect { name, dry_run } => {
            return agent_command(&name, |adapter| connect(adapter, &config, dry_run));
        }
        Command::AgentDisconnect { name } => {
            return agent_command(&name, |adapter| disconnect(adapter));
        }
        Command::AgentStatus => {
            return finish(agent_status(&config));
        }
    };
    finish(result)
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Version,
    Serve,
    Check,
    TokenShow,
    TokenRotate,
    Usage,
    AgentList,
    AgentConnect { name: String, dry_run: bool },
    AgentDisconnect { name: String },
    AgentStatus,
}

fn parse(arguments: &[String]) -> Result<(PathBuf, Command), String> {
    let mut config = PathBuf::from("hofund.toml");
    let mut dry_run = false;
    let mut words = Vec::new();
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        if argument == "--config" {
            let path = iter
                .next()
                .ok_or_else(|| "--config needs a path".to_owned())?;
            config = PathBuf::from(path);
        } else if argument == "--dry-run" {
            dry_run = true;
        } else if argument.starts_with('-') && argument != "--version" && argument != "-V" {
            return Err(format!("unknown flag `{argument}`"));
        } else {
            words.push(argument.as_str());
        }
    }
    let command = match words.as_slice() {
        [] | ["version"] | ["--version"] | ["-V"] => Command::Version,
        ["serve"] => Command::Serve,
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
        },
        ["agent", "status"] => Command::AgentStatus,
        _ => {
            return Err(
                "usage: hofund [version] | serve | config check | token show | token rotate | usage | agent list | agent connect <name> [--dry-run] | agent disconnect <name> | agent status [--config PATH]"
                    .to_owned(),
            );
        }
    };
    if dry_run && !matches!(command, Command::AgentConnect { .. }) {
        return Err("--dry-run is only valid with agent connect".to_owned());
    }
    Ok((config, command))
}

fn finish<E: std::fmt::Display>(result: Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

fn agent_command(
    name: &str,
    run: impl FnOnce(&dyn hofund_agent::AgentAdapter) -> Result<(), String>,
) -> ExitCode {
    let Some(adapter) = find(name) else {
        eprintln!("unknown agent `{name}`");
        return ExitCode::from(2);
    };
    finish(run(adapter))
}

fn print_agent_list() {
    let session = Session::from_process_env();
    println!("id\tname\tinstalled");
    for adapter in adapters() {
        let installed = if adapter.detect(&session) {
            "yes"
        } else {
            "no"
        };
        println!("{}\t{}\t{installed}", adapter.id(), adapter.display_name());
    }
}

fn connect(
    adapter: &dyn hofund_agent::AgentAdapter,
    config: &Path,
    dry_run: bool,
) -> Result<(), String> {
    let session = Session::from_process_env();
    let local = local_gateway(config).map_err(|error| error.to_string())?;
    let target = GatewayTarget {
        base_url: local.base_url,
        token: local.token,
    };
    let plan = if dry_run {
        adapter.plan(&session, &target)
    } else {
        adapter.apply(&session, &target)
    }
    .map_err(|error| error.to_string())?;
    if plan.changes.is_empty() {
        println!("{}: already matches", adapter.id());
        return Ok(());
    }
    for change in &plan.changes {
        if dry_run {
            print!("{}", change.diff);
        } else {
            println!("{}: wrote {}", adapter.id(), change.path.display());
        }
    }
    Ok(())
}

fn disconnect(adapter: &dyn hofund_agent::AgentAdapter) -> Result<(), String> {
    let session = Session::from_process_env();
    adapter
        .revert(&session)
        .map_err(|error| error.to_string())?;
    println!("{}: restored", adapter.id());
    Ok(())
}

fn agent_status(config: &Path) -> Result<(), String> {
    let session = Session::from_process_env();
    let local = local_gateway(config).map_err(|error| error.to_string())?;
    let target = GatewayTarget {
        base_url: local.base_url,
        token: local.token,
    };
    println!("id\tinstalled\tconnected");
    for adapter in adapters() {
        let status = adapter
            .status(&session, &target)
            .map_err(|error| error.to_string())?;
        let installed = if status.installed { "yes" } else { "no" };
        let connected = if status.connected { "yes" } else { "no" };
        println!("{}\t{installed}\t{connected}", status.id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Command, parse};

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
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
            }
        );
        assert_eq!(
            parse(&words(&["agent", "status"])).expect("parse").1,
            Command::AgentStatus
        );
        assert!(parse(&words(&["nope"])).is_err());
        assert!(parse(&words(&["agent", "status", "--dry-run"])).is_err());
    }
}
