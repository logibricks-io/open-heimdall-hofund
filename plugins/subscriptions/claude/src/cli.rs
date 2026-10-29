//! Running the `claude` command line and reading what it prints.

use std::ffi::OsString;
use std::path::Path;

use hofund_plugin_api::{BodyStream, PluginError, SpawnRequest};

/// Signs in with a Claude subscription, not an API key.
pub(crate) const LOGIN_ARGS: [&str; 3] = ["auth", "login", "--claudeai"];

/// Reports the signed-in account as JSON.
pub(crate) const STATUS_ARGS: [&str; 3] = ["auth", "status", "--json"];

/// Where Claude Code keeps its sign-in and settings.
pub(crate) const CONFIG_DIR_VAR: &str = "CLAUDE_CONFIG_DIR";

/// How much of a printed line an error quotes.
const LINE_CAP: usize = 200;

/// How much `auth status` output is read.
const STATUS_CAP: usize = 64 * 1024;

/// A spawn of `program` with `args`, run in `dir` and keeping its state
/// there. The kernel clears the rest of the environment.
pub(crate) fn request(
    program: &Path,
    args: &[&str],
    dir: &Path,
) -> Result<SpawnRequest, PluginError> {
    SpawnRequest::new(
        program,
        args.iter().map(OsString::from).collect(),
        vec![(OsString::from(CONFIG_DIR_VAR), dir.as_os_str().to_owned())],
        dir,
    )
}

/// The next printed line without colour codes or surrounding space. `None`
/// once the program has exited.
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

/// Reads until a line has an `https://` link and returns the link. `None`
/// when the program exited first. `last` keeps the last other line.
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

/// Everything printed until exit, capped.
pub(crate) async fn collect(stdout: &mut BodyStream) -> String {
    let mut text = String::new();
    while let Some(line) = next_line(stdout).await {
        if text.len() + line.len() > STATUS_CAP {
            break;
        }
        text.push_str(&line);
        text.push('\n');
    }
    text
}

fn remember(last: &mut String, line: &str) {
    if line.is_empty() {
        return;
    }
    *last = line.chars().take(LINE_CAP).collect();
}

/// What an error says the command line printed last.
pub(crate) fn said(last: &str) -> String {
    if last.is_empty() {
        "it printed nothing".to_owned()
    } else {
        format!("it said: {last}")
    }
}

/// The first `https://` link in `line`, up to the next space.
pub(crate) fn find_link(line: &str) -> Option<&str> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let link = &rest[..end];
    (link.len() > "https://".len()).then_some(link)
}

/// `text` without terminal control sequences: CSI (`ESC [ ... final`), OSC
/// (`ESC ] ... BEL` or `ESC ] ... ESC \`), and other two-byte escapes.
pub(crate) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if !ch.is_control() || ch == '\t' {
                out.push(ch);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}
