//! Detached `hofundd` spawn (mirrors `apps/cli/src/events.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hofund_control_client::resolve_config_id;

/// Spawns the daemon beside `hofund` / on `PATH` with `HOFUND_SERVES=tui`.
pub fn spawn_detached(config: &Path) -> Result<u32, String> {
    let exe = locate_daemon()?;
    let mut command = Command::new(&exe);
    command.arg("--config").arg(config);
    command.env("HOFUND_SERVES", "tui");
    command.env("HOFUND_OWNER_PID", std::process::id().to_string());
    command.stdin(Stdio::null());
    if let Some((path, file)) = open_log(config) {
        if let Ok(copy) = file.try_clone() {
            command.stdout(file).stderr(copy);
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let _ = path;
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    detach(&mut command);
    Ok(command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", exe.display()))?
        .id())
}

/// Path to the daemon log beside the config, when it can be resolved.
#[must_use]
pub fn daemon_log_path(config: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(config).ok()?;
    let dir = resolved.parent()?.join(".hofund");
    let id = resolve_config_id(config).ok()?;
    Some(dir.join(format!("daemon-{id}.log")))
}

fn locate_daemon() -> Result<std::path::PathBuf, String> {
    let name = daemon_name();
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&name)));
    if let Some(path) = exe.as_ref()
        && path.is_file()
    {
        return Ok(path.clone());
    }
    which_daemon(&name)
}

fn which_daemon(name: &str) -> Result<std::path::PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "cannot find {name}; install hofundd beside hofund-tui or on PATH"
    ))
}

fn daemon_name() -> String {
    format!("hofundd{}", std::env::consts::EXE_SUFFIX)
}

fn open_log(config: &Path) -> Option<(PathBuf, std::fs::File)> {
    let resolved = std::fs::canonicalize(config).ok()?;
    let dir = resolved.parent()?.join(".hofund");
    let path = dir.join(format!("daemon-{}.log", resolve_config_id(config).ok()?));
    let mut builder = std::fs::DirBuilder::new();
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        builder.mode(0o700);
        options.mode(0o600);
    }
    match builder.create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return None,
    }
    let file = options.open(&path).ok()?;
    Some((path, file))
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {}

/// Best-effort liveness check for a spawned pid.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: Win32 process query; invalid handles yield false.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle == 0 {
                return false;
            }
            let mut code = 0_u32;
            if GetExitCodeProcess(handle, &mut code) == 0 {
                CloseHandle(handle);
                return false;
            }
            CloseHandle(handle);
            code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(all(unix, test))]
mod unix_tests {
    use super::open_log;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn open_log_uses_restrictive_modes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("hofund.toml");
        std::fs::write(&config, "token_file = \"t\"\nlisten = \"127.0.0.1:9\"\n").expect("write");
        let (path, _file) = open_log(&config).expect("open log");
        let meta = std::fs::metadata(path.parent().expect("parent")).expect("dir meta");
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        let file_meta = std::fs::metadata(&path).expect("file meta");
        assert_eq!(file_meta.permissions().mode() & 0o777, 0o600);
    }
}

#[cfg(windows)]
mod windows_tests {
    use super::process_alive;

    #[test]
    fn missing_pid_is_not_alive() {
        assert!(!process_alive(4_194_304));
    }
}
