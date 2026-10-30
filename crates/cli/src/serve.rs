//! `hofund serve`: hand the config to `hofundd`, the daemon binary.
//!
//! The CLI looks for `hofundd` beside its own executable first and on `PATH`
//! second. It passes `--config <path>` exactly once and leaves the
//! environment as it is. On Unix the CLI process becomes the daemon through
//! `exec`, so signals and the exit status belong to `hofundd`. On Windows the
//! CLI starts `hofundd.exe`, keeps console events from ending the CLI while it
//! waits, and exits with the daemon's numeric exit code. The daemon handles
//! Ctrl-C, Ctrl-Break, and console close itself.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The daemon file name on this platform.
pub(crate) fn daemon_file_name() -> OsString {
    let mut name = OsString::from("hofundd");
    name.push(std::env::consts::EXE_SUFFIX);
    name
}

/// Finds the daemon binary: in `exe_dir` first, then in each `PATH` entry.
///
/// # Errors
///
/// Returns a message that names both places it looked and how to install the
/// daemon when neither has it.
pub(crate) fn locate(exe_dir: Option<&Path>, path: Option<&OsStr>) -> Result<PathBuf, String> {
    let name = daemon_file_name();
    let beside = exe_dir.map(|dir| dir.join(&name));
    if let Some(candidate) = beside.as_deref()
        && runnable(candidate)
    {
        return Ok(candidate.to_path_buf());
    }
    for dir in path.map(std::env::split_paths).into_iter().flatten() {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(&name);
        if runnable(&candidate) {
            return Ok(candidate);
        }
    }
    let beside = beside.map_or_else(
        || "beside this hofund binary (its directory is unknown)".to_owned(),
        |candidate| format!("at {}", candidate.display()),
    );
    Err(format!(
        "cannot start the daemon: {name} is not {beside} and not on PATH.\n\
         Install the hofund-daemon-bin package into the same directory as hofund, \
         so hofund and {name} sit side by side. From a source checkout, run \
         `cargo install --path apps/hofundd --locked`.\n\
         `hofund serve --in-process` still serves without the daemon binary until \
         the next release.",
        name = name.to_string_lossy(),
    ))
}

#[cfg(unix)]
fn runnable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(candidate)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(candidate: &Path) -> bool {
    candidate.is_file()
}

/// The daemon command for `config`: `--config <config>` and nothing else.
pub(crate) fn command(daemon: &Path, config: &Path) -> Command {
    let mut command = Command::new(daemon);
    command.arg("--config").arg(config);
    command
}

/// Runs the daemon for `config` and returns the exit code to leave with.
///
/// On Unix this returns only when `exec` fails.
///
/// # Errors
///
/// Returns a message when the daemon cannot be found or started, or when it
/// stopped without an exit code.
pub(crate) fn run(config: &Path) -> Result<i32, String> {
    // Resolve symlinks: macOS reports the invoked path (for example under
    // /var rather than /private/var, or a Homebrew bin link), and the
    // daemon sits beside the real install.
    let exe = std::env::current_exe()
        .ok()
        .map(|exe| std::fs::canonicalize(&exe).unwrap_or(exe));
    let exe_dir = exe.as_deref().and_then(Path::parent);
    let path = std::env::var_os("PATH");
    let daemon = locate(exe_dir, path.as_deref())?;
    replace(command(&daemon, config), &daemon)
}

#[cfg(unix)]
fn replace(mut command: Command, daemon: &Path) -> Result<i32, String> {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    Err(format!("cannot run {}: {error}", daemon.display()))
}

#[cfg(windows)]
fn replace(command: Command, daemon: &Path) -> Result<i32, String> {
    let _console = console::hold().map_err(|error| {
        format!(
            "cannot keep console events for {}: {error}",
            daemon.display()
        )
    })?;
    wait(command, daemon)
}

#[cfg(not(any(unix, windows)))]
fn replace(command: Command, daemon: &Path) -> Result<i32, String> {
    wait(command, daemon)
}

/// Starts `command`, waits for it, and returns its exit code.
///
/// # Errors
///
/// Returns a message when the child cannot start or stops without a code.
#[cfg_attr(
    all(unix, not(test)),
    expect(dead_code, reason = "Unix replaces the process; the tests use it")
)]
pub(crate) fn wait(mut command: Command, daemon: &Path) -> Result<i32, String> {
    let status = command
        .status()
        .map_err(|error| format!("cannot run {}: {error}", daemon.display()))?;
    status.code().ok_or_else(|| {
        format!(
            "{} stopped without an exit code ({status})",
            daemon.display()
        )
    })
}

#[cfg(windows)]
mod console {
    //! Keeps console control events from ending the waiting CLI.
    //!
    //! Each listener tells Windows the event was handled, so the CLI stays
    //! alive while `hofundd.exe`, which shares the console, gets the same
    //! event and decides the exit code. For console close, the handler
    //! blocks until Windows ends the process, which leaves the daemon time
    //! to drain.

    use tokio::signal::windows::{
        CtrlBreak, CtrlC, CtrlClose, CtrlLogoff, CtrlShutdown, ctrl_break, ctrl_c, ctrl_close,
        ctrl_logoff, ctrl_shutdown,
    };

    pub(super) struct Held {
        _listeners: (CtrlC, CtrlBreak, CtrlClose, CtrlLogoff, CtrlShutdown),
        _runtime: tokio::runtime::Runtime,
    }

    pub(super) fn hold() -> std::io::Result<Held> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let listeners = {
            let _entered = runtime.enter();
            (
                ctrl_c()?,
                ctrl_break()?,
                ctrl_close()?,
                ctrl_logoff()?,
                ctrl_shutdown()?,
            )
        };
        Ok(Held {
            _listeners: listeners,
            _runtime: runtime,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::{command, daemon_file_name, locate, wait};

    fn put_daemon(dir: &Path) -> std::path::PathBuf {
        let file = dir.join(daemon_file_name());
        fs::write(&file, "#!/bin/sh\nexit 0\n").expect("write daemon");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        file
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).expect("join PATH")
    }

    #[test]
    fn the_daemon_beside_the_cli_wins_over_path() {
        let beside = tempfile::tempdir().expect("tempdir");
        let on_path = tempfile::tempdir().expect("tempdir");
        let expected = put_daemon(beside.path());
        put_daemon(on_path.path());
        let path = path_of(&[on_path.path()]);
        assert_eq!(
            locate(Some(beside.path()), Some(&path)).expect("found"),
            expected
        );
    }

    #[test]
    fn path_entries_are_searched_in_order_when_nothing_is_beside() {
        let beside = tempfile::tempdir().expect("tempdir");
        let empty = tempfile::tempdir().expect("tempdir");
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let expected = put_daemon(first.path());
        put_daemon(second.path());
        let path = path_of(&[empty.path(), first.path(), second.path()]);
        assert_eq!(
            locate(Some(beside.path()), Some(&path)).expect("found"),
            expected
        );
        assert_eq!(locate(None, Some(&path)).expect("found"), expected);
    }

    #[test]
    fn a_directory_named_like_the_daemon_is_not_the_daemon() {
        let beside = tempfile::tempdir().expect("tempdir");
        let on_path = tempfile::tempdir().expect("tempdir");
        fs::create_dir(beside.path().join(daemon_file_name())).expect("mkdir");
        let expected = put_daemon(on_path.path());
        let path = path_of(&[on_path.path()]);
        assert_eq!(
            locate(Some(beside.path()), Some(&path)).expect("found"),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_execute_permission_is_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let beside = tempfile::tempdir().expect("tempdir");
        let on_path = tempfile::tempdir().expect("tempdir");
        let plain = put_daemon(beside.path());
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).expect("chmod");
        let expected = put_daemon(on_path.path());
        let path = path_of(&[on_path.path()]);
        assert_eq!(
            locate(Some(beside.path()), Some(&path)).expect("found"),
            expected
        );
        assert!(locate(Some(beside.path()), None).is_err());
    }

    #[test]
    fn a_missing_daemon_names_both_places_and_the_package() {
        let beside = tempfile::tempdir().expect("tempdir");
        let on_path = tempfile::tempdir().expect("tempdir");
        let path = path_of(&[on_path.path()]);
        let error = locate(Some(beside.path()), Some(&path)).expect_err("missing");
        let name = daemon_file_name();
        let name = name.to_string_lossy();
        assert!(
            error.contains(&format!(
                "at {}",
                beside.path().join(name.as_ref()).display()
            )),
            "{error}"
        );
        assert!(error.contains("not on PATH"), "{error}");
        assert!(error.contains("hofund-daemon-bin"), "{error}");
        assert!(
            error.contains("cargo install --path apps/hofundd --locked"),
            "{error}"
        );
        assert!(error.contains("--in-process"), "{error}");
        let error = locate(None, None).expect_err("missing");
        assert!(error.contains("its directory is unknown"), "{error}");
        assert!(error.contains("not on PATH"), "{error}");
    }

    #[test]
    fn the_command_passes_only_the_config() {
        let built = command(Path::new("/opt/hofundd"), Path::new("/etc/hofund.toml"));
        assert_eq!(built.get_program(), Path::new("/opt/hofundd").as_os_str());
        let args: Vec<_> = built.get_args().collect();
        assert_eq!(args, ["--config", "/etc/hofund.toml"]);
        assert_eq!(built.get_envs().count(), 0);
    }

    fn shell(script: &str) -> Command {
        if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", script]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        }
    }

    #[test]
    fn wait_returns_the_child_exit_code() {
        let daemon = Path::new("child");
        assert_eq!(wait(shell("exit 7"), daemon), Ok(7));
        assert_eq!(wait(shell("exit 0"), daemon), Ok(0));
        assert_eq!(wait(shell("exit 143"), daemon), Ok(143));
    }

    #[test]
    fn wait_reports_a_child_that_cannot_start() {
        let missing = Path::new("/nonexistent/hofundd-for-test");
        let error = wait(Command::new(missing), missing).expect_err("cannot start");
        assert!(
            error.starts_with("cannot run /nonexistent/hofundd-for-test: "),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_child_killed_by_a_signal_is_a_failure() {
        let error = wait(shell("kill -9 $$"), Path::new("child")).expect_err("no code");
        assert!(
            error.starts_with("child stopped without an exit code"),
            "{error}"
        );
    }
}
