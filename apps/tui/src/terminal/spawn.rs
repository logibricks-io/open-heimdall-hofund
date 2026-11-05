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

/// Returns whether `tasklist` CSV output lists `pid` as a data row.
#[must_use]
#[cfg_attr(not(windows), allow(dead_code))]
pub fn tasklist_csv_shows_pid(output: &str, pid: u32) -> bool {
    let needle = pid.to_string();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("INFO:") {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        let pid_field = fields
            .get(1)
            .or_else(|| fields.first())
            .map(|field| field.trim().trim_matches('"'));
        if pid_field == Some(needle.as_str()) {
            return true;
        }
    }
    false
}

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
        match Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
        {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                tasklist_csv_shows_pid(&text, pid)
            }
            _ => false,
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

#[cfg(test)]
mod tasklist_tests {
    use super::tasklist_csv_shows_pid;

    #[test]
    fn csv_row_with_matching_pid_is_alive() {
        let sample = "\"hofundd.exe\",\"4242\",\"Console\",\"1\",\"12,345 K\"\n";
        assert!(tasklist_csv_shows_pid(sample, 4242));
    }

    #[test]
    fn info_no_tasks_is_not_alive() {
        let sample = "INFO: No tasks are running which match the specified criteria.\n";
        assert!(!tasklist_csv_shows_pid(sample, 4242));
    }

    #[test]
    fn different_pid_in_csv_is_not_alive() {
        let sample = "\"hofundd.exe\",\"9999\",\"Console\",\"1\",\"12,345 K\"\n";
        assert!(!tasklist_csv_shows_pid(sample, 4242));
    }
}
