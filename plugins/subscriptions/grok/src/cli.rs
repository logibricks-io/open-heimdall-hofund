//! Running the Grok Build CLI and reading what it prints.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use hofund_plugin_api::{BinaryName, BodyStream, PluginError, SpawnRequest};

/// The CLI command hofund runs.
pub(crate) const COMMAND: BinaryName = BinaryName::new("grok");

/// Device sign-in through the CLI.
pub(crate) const LOGIN_ARGS: [&str; 2] = ["login", "--device-auth"];

/// Renews the sign-in the way any CLI use does.
pub(crate) const MODELS_ARGS: [&str; 1] = ["models"];

/// Environment variable for the CLI home directory.
pub(crate) const HOME_VAR: &str = "GROK_HOME";

const LINE_CAP: usize = 200;

/// Resolves the `grok` program: an absolute `command` path or lookup on PATH.
pub(crate) fn program(command: Option<&PathBuf>) -> Result<PathBuf, PluginError> {
    if let Some(command) = command {
        return Ok(command.clone());
    }
    hofund_plugin_sdk::process::find_program(COMMAND).ok_or_else(|| {
        PluginError::config(
            "Grok Build is not installed: `grok` is not on PATH; install it or set `command`",
        )
    })
}

/// A spawn of `program` with `args`, run with `home` as GROK_HOME.
pub(crate) fn request(
    program: &Path,
    args: &[&str],
    home: &Path,
) -> Result<SpawnRequest, PluginError> {
    SpawnRequest::new(
        program,
        args.iter().map(OsString::from).collect(),
        vec![(OsString::from(HOME_VAR), home.as_os_str().to_owned())],
        home.parent().unwrap_or(home),
    )
}

pub(crate) fn login_request(
    program: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<SpawnRequest, PluginError> {
    SpawnRequest::new(
        program,
        LOGIN_ARGS.iter().map(OsString::from).collect(),
        login_env(home),
        cwd,
    )
}

/// The next printed line without colour codes or surrounding space.
pub(crate) async fn next_line(stdout: &mut BodyStream) -> Option<String> {
    match stdout.next_chunk().await {
        Some(Ok(chunk)) => Some(
            strip_ansi(&String::from_utf8_lossy(&chunk))
                .trim()
                .to_owned(),
        ),
        Some(Err(_)) | None => None,
    }
}

/// Reads until a line has an `https://` link and returns the link.
pub(crate) async fn read_link(stdout: &mut BodyStream, last: &mut String) -> Option<String> {
    while let Some(line) = next_line(stdout).await {
        if let Some(link) = find_link(&line) {
            return Some(link.to_owned());
        }
        remember(last, &line);
    }
    None
}

/// Reads until the program exits. `last` keeps the last line.
pub(crate) async fn drain(stdout: &mut BodyStream, last: &mut String) {
    while let Some(line) = next_line(stdout).await {
        remember(last, &line);
    }
}

pub(crate) fn said(last: &str) -> String {
    if last.is_empty() {
        "it printed nothing".to_owned()
    } else {
        format!("it said: {last}")
    }
}

/// Extra environment for device sign-in on platforms where the CLI does not open a browser.
pub(crate) fn login_env(home: &Path) -> Vec<(OsString, OsString)> {
    let mut env = vec![(OsString::from(HOME_VAR), home.as_os_str().to_owned())];
    if !opens_page_natively() {
        env.push((OsString::from("BROWSER"), OsString::from("true")));
    }
    env
}

pub(crate) fn opens_page_natively() -> bool {
    matches!(std::env::consts::OS, "macos" | "windows")
}

fn remember(last: &mut String, line: &str) {
    if line.is_empty() {
        return;
    }
    *last = line.chars().take(LINE_CAP).collect();
}

fn find_link(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    if lower.split_whitespace().any(|word| word == "error") {
        return None;
    }
    line.split_whitespace()
        .find(|word| word.starts_with("https://"))
}

#[allow(clippy::while_let_on_iterator)]
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            while let Some(next) = chars.next() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::strip_ansi;

    #[test]
    fn strips_ansi_colour_codes() {
        assert_eq!(
            strip_ansi("\u{1b}[4mhttps://example.test/x\u{1b}[24m"),
            "https://example.test/x"
        );
    }
}
