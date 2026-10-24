//! Local maintenance tasks for the hofund workspace.
//!
//! Run `cargo xtask help` for the task list. This crate is not part of the gateway
//! runtime, and no workspace crate may depend on it. Architecture checks live in
//! this crate's integration tests.

use std::process::ExitCode;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None | Some("help" | "--help" | "-h") => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(task) => {
            eprintln!("unknown xtask `{task}`");
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "\
hofund xtask

Tasks:
  help    Show this help

Dependency direction and the vendor-name rule are enforced by
`cargo test --workspace`."
    );
}
