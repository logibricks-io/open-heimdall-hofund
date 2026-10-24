//! Command-line interface for hofund.
//!
//! The binary parses arguments and calls the daemon. It does not route and it
//! does not encode a protocol.

use std::path::PathBuf;
use std::process::ExitCode;

use hofund_daemon::{check, serve_blocking, token_rotate, token_show, usage_report};

// Referenced so the agent adapter dependency stays compiled.
use hofund_agent as _;

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
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Version,
    Serve,
    Check,
    TokenShow,
    TokenRotate,
    Usage,
}

fn parse(arguments: &[String]) -> Result<(PathBuf, Command), String> {
    let mut config = PathBuf::from("hofund.toml");
    let mut words = Vec::new();
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        if argument == "--config" {
            let path = iter
                .next()
                .ok_or_else(|| "--config needs a path".to_owned())?;
            config = PathBuf::from(path);
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
        _ => {
            return Err(
                "usage: hofund [version] | serve | config check | token show | token rotate | usage [--config PATH]"
                    .to_owned(),
            );
        }
    };
    Ok((config, command))
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
        assert!(parse(&words(&["nope"])).is_err());
    }
}
