//! `hofund tui`: run the terminal UI binary beside this executable.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::serve;

/// The TUI binary name on this platform.
fn tui_file_name() -> OsString {
    let mut name = OsString::from("hofund-tui");
    name.push(std::env::consts::EXE_SUFFIX);
    name
}

fn locate_tui(exe_dir: Option<&Path>, path: Option<&OsStr>) -> Result<PathBuf, String> {
    let name = tui_file_name();
    let beside = exe_dir.map(|dir| dir.join(&name));
    if let Some(candidate) = beside.as_deref()
        && candidate.is_file()
    {
        return Ok(candidate.to_path_buf());
    }
    for dir in path.map(std::env::split_paths).into_iter().flatten() {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let beside = beside.map_or_else(
        || "beside this hofund binary (its directory is unknown)".to_owned(),
        |candidate| format!("at {}", candidate.display()),
    );
    Err(format!(
        "cannot start the terminal UI: {name} is not {beside} and not on PATH.\n\
         Build it with `cargo build -p hofund-tui --locked`.",
        name = name.to_string_lossy(),
    ))
}

/// Runs the TUI for `config`.
pub(crate) fn run(config: &Path, no_color: bool) -> Result<i32, String> {
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe));
    let exe_dir = exe.as_deref().and_then(Path::parent);
    let path = std::env::var_os("PATH");
    let tui = locate_tui(exe_dir, path.as_deref())?;
    let mut command = Command::new(&tui);
    command.arg("--config").arg(config);
    if no_color {
        command.arg("--no-color");
    }
    serve::replace(command, &tui)
}
