//! `hofundd accounts`: agent sign-in switching.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hofund_daemon::accounts_switch_in;

pub(crate) const USAGE: &str = "\
Usage: hofundd accounts [--config <path>] switch <claude|codex> <next|account>

The hofund CLI runs these for `hofund accounts`; see `hofund accounts switch --help`.
";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AccountsCommand {
    Switch { agent: String, target: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parsed {
    Help,
    Run(Option<PathBuf>, AccountsCommand),
}

pub(crate) fn parse(args: &[OsString]) -> Result<Parsed, String> {
    let mut config = None;
    let mut words = Vec::new();
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
        match text {
            "--" => operands_only = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "--config" => {
                config = Some(PathBuf::from(args.next().ok_or("--config needs a value")?));
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            word => words.push(word.to_owned()),
        }
    }
    let command = match words.as_slice() {
        [command, agent, target] if command == "switch" => AccountsCommand::Switch {
            agent: agent.clone(),
            target: target.clone(),
        },
        _ => return Err(USAGE.to_owned()),
    };
    Ok(Parsed::Run(config, command))
}

pub(crate) fn run(config: &Path, command: AccountsCommand) -> ExitCode {
    match command {
        AccountsCommand::Switch { agent, target } => {
            match accounts_switch_in(config, &agent, &target) {
                Ok(label) => {
                    println!("signed in to {label}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
