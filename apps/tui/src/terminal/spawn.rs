//! Detached `hofundd` spawn (mirrors `apps/cli/src/events.rs`).

use std::path::Path;
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

fn open_log(config: &Path) -> Option<(std::path::PathBuf, std::fs::File)> {
    let resolved = std::fs::canonicalize(config).ok()?;
    let dir = resolved.parent()?.join(".hofund");
    let path = dir.join(format!("daemon-{}.log", resolve_config_id(config).ok()?));
    std::fs::create_dir_all(&dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
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
        use std::os::windows::process::CommandExt;
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        let mut command = std::process::Command::new("tasklist");
        command.args(["/FI", &format!("PID eq {pid}")]);
        command.creation_flags(PROCESS_QUERY_LIMITED_INFORMATION);
        command
            .output()
            .is_ok_and(|out| out.status.success() && !out.stdout.is_empty())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}
