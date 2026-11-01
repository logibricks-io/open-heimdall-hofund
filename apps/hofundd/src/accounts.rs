//! `hofundd accounts`: agent sign-in switching and account import.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hofund_daemon::{accounts_import_in, accounts_switch_in};

pub(crate) const USAGE: &str = "\
Usage: hofundd accounts [--config <path>] switch <agent> <next|account>
       hofundd accounts [--config <path>] import <file>

The hofund CLI runs these for `hofund accounts`.
";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AccountsCommand {
    Switch { agent: String, target: String },
    Import { file: PathBuf },
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
        [command, file] if command == "import" => AccountsCommand::Import {
            file: PathBuf::from(file),
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
        AccountsCommand::Import { file } => match accounts_import_in(config, &file) {
            Ok(ids) => {
                println!("imported {} account(s)", ids.len());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d27_11_accounts_import_parses() {
        let parsed = parse(&[
            OsString::from("--config"),
            OsString::from("hofund.toml"),
            OsString::from("--"),
            OsString::from("import"),
            OsString::from("export.json"),
        ])
        .expect("parse");
        assert_eq!(
            parsed,
            Parsed::Run(
                Some(PathBuf::from("hofund.toml")),
                AccountsCommand::Import {
                    file: PathBuf::from("export.json"),
                }
            )
        );
    }
}
