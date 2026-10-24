//! Command-line interface for hofund.
//!
//! The `hofund` binary parses user commands and delegates to the daemon and to
//! coding-agent config adapters. It contains no routing policy and no protocol
//! codecs of its own.

use std::process::ExitCode;

// Referenced so this crate's manifest dependencies are compiled, not merely declared.
use hofund_agent as _;
use hofund_daemon as _;

fn main() -> ExitCode {
    let command = std::env::args().nth(1);
    match command.as_deref() {
        None | Some("--version" | "-V" | "version") => {
            println!("hofund {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("`{other}` is not implemented");
            ExitCode::from(2)
        }
    }
}
