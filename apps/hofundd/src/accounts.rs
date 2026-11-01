//! `hofundd accounts`: agent sign-in switching, import, and discovery.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hofund_daemon::{accounts_discover_in, accounts_import_in, accounts_switch_in, format_report};

pub(crate) const USAGE: &str = "\
Usage: hofundd accounts [--config <path>] switch <agent> <next|account>
       hofundd accounts [--config <path>] import <file>
       hofundd accounts [--config <path>] discover [--add]

The hofund CLI runs these for `hofund accounts`.
";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AccountsCommand {
    Switch { agent: String, target: String },
    Import { file: PathBuf },
    Discover { import: bool },
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
            "--add" if words == ["discover"] => {
                words.push("--add".to_owned());
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
        [command] if command == "discover" => AccountsCommand::Discover { import: false },
        [command, flag] if command == "discover" && flag == "--add" => {
            AccountsCommand::Discover { import: true }
        }
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
        AccountsCommand::Discover { import } => match accounts_discover_in(config, import) {
            Ok(report) => {
                print!("{}", format_report(&report, import));
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

    #[test]
    fn a1_10_hofundd_accounts_discover_parses() {
        let parsed = parse(&[OsString::from("discover"), OsString::from("--add")]).expect("parse");
        assert_eq!(
            parsed,
            Parsed::Run(None, AccountsCommand::Discover { import: true })
        );
    }
}
