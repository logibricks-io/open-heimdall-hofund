//! `hofund-tui` entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use hofund_config::user_config_path;

fn main() -> ExitCode {
    let mut config = user_config_path();
    let mut no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let Some(path) = args.next() else {
                    eprintln!("hofund-tui: --config requires a path");
                    return ExitCode::from(2);
                };
                config = PathBuf::from(path);
            }
            "--no-color" => no_color = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: hofund-tui [--config PATH] [--no-color]\n\
                     Open the hofund terminal UI for the chosen config."
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("hofund-tui: unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    match hofund_tui::terminal::run(&config, no_color) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
