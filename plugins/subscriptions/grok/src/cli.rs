//! Running the Grok Build CLI and reading what it prints.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use hofund_plugin_api::{BodyStream, PluginError, SpawnRequest};

/// Device sign-in through the CLI.
pub(crate) const LOGIN_ARGS: [&str; 2] = ["login", "--device-auth"];

/// Renews the sign-in the way any CLI use does.
pub(crate) const MODELS_ARGS: [&str; 1] = ["models"];

/// Environment variable for the CLI home directory.
pub(crate) const HOME_VAR: &str = "GROK_HOME";

const LINE_CAP: usize = 200;

/// Resolves the `grok` program: an absolute `command` path or the CLI's install locations.
pub(crate) fn program(command: Option<&PathBuf>) -> Result<PathBuf, PluginError> {
    if let Some(command) = command {
        return Ok(command.clone());
    }
    let user_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let cli_home = std::env::var_os(HOME_VAR).map_or_else(
        || {
            user_home
                .as_ref()
                .map_or_else(|| PathBuf::from(".grok"), |home| home.join(".grok"))
        },
        PathBuf::from,
    );
    discover(
        std::env::var_os("GROK_BIN_DIR").as_deref(),
        &cli_home,
        user_home.as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
    .ok_or_else(|| PluginError::config(no_cli()))
}

pub(crate) fn no_cli() -> String {
    let install = if cfg!(windows) {
        "irm https://x.ai/cli/install.ps1 | iex"
    } else {
        "curl -fsSL https://x.ai/cli/install.sh | bash"
    };
    format!(
        "install Grok Build first: {install}; or set GROK_BIN_DIR to the folder holding its grok binary"
    )
}

fn discover(
    bin_dir: Option<&OsStr>,
    cli_home: &Path,
    user_home: Option<&Path>,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let name = if cfg!(windows) { "grok.exe" } else { "grok" };
    let mut paths: Vec<(String, PathBuf, bool)> = Vec::new();
    let mut add_path = |dir: &Path, own| {
        if !dir.is_absolute() {
            return;
        }
        let candidate = dir.join(name);
        let key = if cfg!(windows) {
            candidate.to_string_lossy().to_lowercase()
        } else {
            candidate.to_string_lossy().into_owned()
        };
        if !paths
            .iter()
            .any(|(seen, _, _): &(String, PathBuf, bool)| *seen == key)
        {
            paths.push((key, candidate, own));
        }
    };
    if let Some(dir) = bin_dir {
        add_path(Path::new(dir), true);
    }
    add_path(&cli_home.join("bin"), true);
    if let Some(home) = user_home {
        add_path(&home.join(".grok").join("bin"), true);
    }
    for dir in path.into_iter().flat_map(std::env::split_paths) {
        let text = dir.as_os_str().to_string_lossy();
        add_path(Path::new(text.trim()), false);
    }
    if let Some(home) = user_home {
        add_path(&home.join(".local").join("bin"), false);
    }
    paths.into_iter().find_map(|(_, candidate, own)| {
        (candidate.is_file() && (own || is_grok_build(&candidate))).then_some(candidate)
    })
}

fn is_grok_build(path: &Path) -> bool {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    real.to_string_lossy()
        .replace('\\', "/")
        .to_lowercase()
        .contains("/.grok/")
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
#[allow(dead_code)]
pub(crate) async fn drain(stdout: &mut BodyStream, last: &mut String) {
    while drain_once(stdout, last).await {}
}

/// Reads one chunk from stdout. Returns true when the stream ended.
pub(crate) async fn drain_once(stdout: &mut BodyStream, last: &mut String) -> bool {
    match next_line(stdout).await {
        Some(line) => {
            remember(last, &line);
            false
        }
        None => true,
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
    use std::path::{Path, PathBuf};

    use super::{discover, no_cli, strip_ansi};

    fn binary(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).expect("bin dir");
        let path = dir.join(if cfg!(windows) { "grok.exe" } else { "grok" });
        std::fs::write(&path, b"fixture").expect("binary");
        path
    }

    #[test]
    fn strips_ansi_colour_codes() {
        assert_eq!(
            strip_ansi("\u{1b}[4mhttps://example.test/x\u{1b}[24m"),
            "https://example.test/x"
        );
    }

    #[test]
    fn r50_cli_discovery_uses_install_locations_before_path() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path();
        let bin = root.join("chosen-bin");
        let cli_home = root.join("chosen-home");
        let user_home = root.join("user");
        let path_home = user_home.join(".grok").join("path-bin");
        let wanted = binary(&bin);
        let cli_wanted = binary(&cli_home.join("bin"));
        let default_wanted = binary(&user_home.join(".grok").join("bin"));
        let path_wanted = binary(&path_home);
        let path = std::env::join_paths([path_home]).expect("path");

        assert_eq!(
            discover(
                Some(bin.as_os_str()),
                &cli_home,
                Some(&user_home),
                Some(&path),
            ),
            Some(wanted.clone())
        );
        std::fs::remove_file(wanted).expect("remove first");
        assert_eq!(
            discover(None, &cli_home, Some(&user_home), Some(&path)),
            Some(cli_wanted.clone())
        );
        std::fs::remove_file(cli_wanted).expect("remove second");
        assert_eq!(
            discover(None, &cli_home, Some(&user_home), Some(&path)),
            Some(default_wanted.clone())
        );
        std::fs::remove_file(default_wanted).expect("remove third");
        assert_eq!(
            discover(None, &cli_home, Some(&user_home), Some(&path)),
            Some(path_wanted)
        );
    }

    #[test]
    fn r50_path_does_not_accept_an_unrelated_grok_binary() {
        let dir = tempfile::tempdir().expect("dir");
        let other = dir.path().join("other");
        binary(&other);
        let path = std::env::join_paths([other]).expect("path");
        assert_eq!(discover(None, dir.path(), None, Some(&path)), None);
        assert!(
            no_cli().contains("curl -fsSL https://x.ai/cli/install.sh | bash") || cfg!(windows)
        );
        assert!(no_cli().contains("GROK_BIN_DIR to the folder holding its grok binary"));
    }

    #[cfg(unix)]
    #[test]
    fn r50_local_bin_follows_a_cli_symlink() {
        let dir = tempfile::tempdir().expect("dir");
        let user_home = dir.path().join("user");
        let target = binary(&user_home.join(".grok").join("downloads"));
        let local = user_home.join(".local").join("bin");
        std::fs::create_dir_all(&local).expect("local bin");
        std::os::unix::fs::symlink(target, local.join("grok")).expect("symlink");
        assert_eq!(
            discover(None, dir.path(), Some(&user_home), None),
            Some(local.join("grok"))
        );
    }
}
