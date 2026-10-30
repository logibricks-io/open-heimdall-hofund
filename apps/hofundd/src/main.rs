//! `hofundd`: the daemon process.
//!
//! This binary is the composition root. It links the plugin registry, builds
//! the async runtime, installs the log subscriber, serves the configuration
//! with local control enabled, and turns the termination into the process
//! exit status. `hofund serve` runs this binary.

mod logging;
mod registry;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use tracing::level_filters::LevelFilter;

use hofund_daemon::{
    DaemonError, StartOptions, Termination, TolerantWriter, finish, serve_until_signal,
    start_linked, user_config_path,
};

const USAGE: &str = "\
hofundd — the hofund daemon

Usage: hofundd [--config <path>]
       hofundd one-shot [--config <path>]

Serves the configuration until a termination signal, with local control
enabled. Without --config, HOFUND_CONFIG or the per-user default is used.

`one-shot` answers control requests for the configuration on standard input
and output without serving it; the hofund CLI uses it when no daemon runs.

Options:
  --config <path>  The configuration file
  -h, --help       Show this help
  -V, --version    Show the version

Environment:
  HOFUND_LOG       off, error, warn, info (default; warn for one-shot),
                   debug, or trace
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Serve(Option<PathBuf>),
    OneShot(Option<PathBuf>),
}

fn parse(args: &[OsString]) -> Result<Command, String> {
    let mut config = None;
    let one_shot = args.first().and_then(|word| word.to_str()) == Some("one-shot");
    let mut words = args.iter().skip(usize::from(one_shot));
    while let Some(word) = words.next() {
        match word.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help),
            Some("-V" | "--version") => return Ok(Command::Version),
            Some("--config") => {
                if config.is_some() {
                    return Err("--config is given more than once".to_owned());
                }
                let Some(path) = words.next() else {
                    return Err("--config needs a path".to_owned());
                };
                config = Some(PathBuf::from(path));
            }
            _ => {
                return Err(format!(
                    "unexpected argument `{}`\n\n{USAGE}",
                    word.to_string_lossy()
                ));
            }
        }
    }
    Ok(if one_shot {
        Command::OneShot(config)
    } else {
        Command::Serve(config)
    })
}

fn say(mut sink: impl Write, line: &str) {
    let _ = writeln!(sink, "{line}");
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match parse(&args) {
        Ok(Command::Help) => {
            say(TolerantWriter(io::stdout()), USAGE.trim_end());
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            say(
                TolerantWriter(io::stdout()),
                &format!("hofundd {}", env!("CARGO_PKG_VERSION")),
            );
            ExitCode::SUCCESS
        }
        Ok(Command::Serve(config)) => serve(&config.unwrap_or_else(user_config_path)),
        Ok(Command::OneShot(config)) => one_shot(&config.unwrap_or_else(user_config_path)),
        Err(message) => {
            say(TolerantWriter(io::stderr()), &format!("hofundd: {message}"));
            ExitCode::from(2)
        }
    }
}

fn stderr() -> TolerantWriter<io::Stderr> {
    TolerantWriter(io::stderr())
}

/// Installs logging at `HOFUND_LOG` (or `default`) and builds the runtime,
/// or returns the exit code for a failure already reported.
fn prepare(default: LevelFilter) -> Result<tokio::runtime::Runtime, ExitCode> {
    let level = match logging::level(std::env::var(logging::LOG_ENV).ok().as_deref(), default) {
        Ok(level) => level,
        Err(message) => {
            say(stderr(), &format!("hofundd: {message}"));
            return Err(ExitCode::from(2));
        }
    };
    if let Err(message) = logging::install(level, logging::Stderr) {
        say(
            stderr(),
            &format!("hofundd: cannot install logging: {message}"),
        );
        return Err(ExitCode::FAILURE);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            say(
                stderr(),
                &format!("hofundd: cannot start the runtime: {error}"),
            );
            ExitCode::FAILURE
        })
}

fn one_shot(path: &Path) -> ExitCode {
    let runtime = match prepare(logging::ONE_SHOT_LEVEL) {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    let answered = runtime.block_on(hofund_daemon::one_shot_stdio(
        path,
        Arc::new(registry::LinkedRegistry),
    ));
    drop(runtime);
    match answered {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            say(stderr(), &format!("hofundd: {error}"));
            ExitCode::FAILURE
        }
    }
}

fn serve(path: &Path) -> ExitCode {
    let runtime = match prepare(logging::DEFAULT_LEVEL) {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    let served = runtime.block_on(async {
        let options = StartOptions {
            control_enabled: true,
            ..StartOptions::default()
        };
        let running = start_linked(path, Arc::new(registry::LinkedRegistry), options).await?;
        say(
            stderr(),
            &format!("listening on http://{}", running.local_addr),
        );
        Ok::<Termination, DaemonError>(serve_until_signal(running).await)
    });
    // Background tasks end here, before a signal is re-raised.
    drop(runtime);
    match served {
        Ok(termination) => {
            if let Termination::Fatal(message) = &termination {
                say(stderr(), &format!("hofundd: {message}"));
            }
            finish(termination)
        }
        Err(error) => {
            say(stderr(), &format!("hofundd: {error}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_the_serve_forms() {
        assert_eq!(parse(&args(&[])), Ok(Command::Serve(None)));
        assert_eq!(
            parse(&args(&["--config", "/etc/hofund.toml"])),
            Ok(Command::Serve(Some(PathBuf::from("/etc/hofund.toml"))))
        );
        assert_eq!(parse(&args(&["--help"])), Ok(Command::Help));
        assert_eq!(parse(&args(&["-h"])), Ok(Command::Help));
        assert_eq!(parse(&args(&["--version"])), Ok(Command::Version));
        assert_eq!(parse(&args(&["-V"])), Ok(Command::Version));
        assert_eq!(parse(&args(&["one-shot"])), Ok(Command::OneShot(None)));
        assert_eq!(
            parse(&args(&["one-shot", "--config", "/etc/hofund.toml"])),
            Ok(Command::OneShot(Some(PathBuf::from("/etc/hofund.toml"))))
        );
    }

    #[test]
    fn rejects_bad_command_lines() {
        assert_eq!(
            parse(&args(&["--config"])),
            Err("--config needs a path".to_owned())
        );
        assert_eq!(
            parse(&args(&["--config", "a", "--config", "b"])),
            Err("--config is given more than once".to_owned())
        );
        let error = parse(&args(&["--config", "a", "one-shot"])).expect_err("late one-shot");
        assert!(
            error.starts_with("unexpected argument `one-shot`"),
            "{error}"
        );
        let error = parse(&args(&["serve"])).expect_err("unknown word");
        assert!(error.starts_with("unexpected argument `serve`"), "{error}");
        assert!(error.contains("Usage: hofundd"), "{error}");
    }
}
