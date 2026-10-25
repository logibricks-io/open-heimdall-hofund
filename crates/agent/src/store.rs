//! Backup-once and atomic replace for agent config files.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::AgentError;

pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, AgentError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(path, error)),
    }
}

pub(crate) fn backup_once(path: &Path) -> Result<(), AgentError> {
    let backup = sibling(path, ".hofund.bak");
    let absent = sibling(path, ".hofund.absent");
    if path.is_file() {
        if !backup.exists() {
            let bytes = fs::read(path).map_err(|error| io(path, error))?;
            write_atomic(&backup, &bytes)?;
        }
        return Ok(());
    }
    if !absent.exists() && !backup.exists() {
        write_atomic(&absent, b"")?;
    }
    Ok(())
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AgentError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    }
    let mut temporary_name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("hofund.tmp"), ToOwned::to_owned);
    temporary_name.push(".tmp");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(temporary_name);
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| io(&temporary, error))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io(&temporary, error))?;
    }
    #[cfg(unix)]
    copy_mode(path, &temporary)?;
    replace_file(&temporary, path)
}

/// Restores `path` from its backup. Returns whether a backup was present.
pub(crate) fn revert_path(path: &Path) -> Result<bool, AgentError> {
    let backup = sibling(path, ".hofund.bak");
    let absent = sibling(path, ".hofund.absent");
    if backup.is_file() {
        let bytes = fs::read(&backup).map_err(|error| io(&backup, error))?;
        write_atomic(path, &bytes)?;
        fs::remove_file(&backup).map_err(|error| io(&backup, error))?;
        if absent.is_file() {
            fs::remove_file(&absent).map_err(|error| io(&absent, error))?;
        }
        return Ok(true);
    }
    if absent.is_file() {
        if path.exists() {
            fs::remove_file(path).map_err(|error| io(path, error))?;
        }
        fs::remove_file(&absent).map_err(|error| io(&absent, error))?;
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn unified_diff(path: &Path, before: &[u8], after: &[u8]) -> String {
    if before == after {
        return String::new();
    }
    let before = String::from_utf8_lossy(before);
    let after = String::from_utf8_lossy(after);
    let before_lines: Vec<&str> = before.split('\n').collect();
    let after_lines: Vec<&str> = after.split('\n').collect();
    let mut start = 0;
    while start < before_lines.len()
        && start < after_lines.len()
        && before_lines[start] == after_lines[start]
    {
        start += 1;
    }
    let mut end_before = before_lines.len();
    let mut end_after = after_lines.len();
    while end_before > start
        && end_after > start
        && before_lines[end_before - 1] == after_lines[end_after - 1]
    {
        end_before -= 1;
        end_after -= 1;
    }
    let mut out = format!("--- {}\n+++ {}\n", path.display(), path.display());
    for line in &before_lines[start..end_before] {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in &after_lines[start..end_after] {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("hofund"), ToOwned::to_owned);
    name.push(suffix);
    path.with_file_name(name)
}

fn replace_file(temporary: &Path, path: &Path) -> Result<(), AgentError> {
    match fs::rename(temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(path).map_err(|error| io(path, error))?;
            fs::rename(temporary, path).map_err(|error| io(path, error))
        }
        Err(error) => {
            let _ = fs::remove_file(temporary);
            Err(io(path, error))
        }
    }
}

#[cfg(unix)]
fn copy_mode(destination: &Path, temporary: &Path) -> Result<(), AgentError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if destination.exists() {
        fs::metadata(destination)
            .map_err(|error| io(destination, error))?
            .permissions()
            .mode()
    } else {
        0o600
    };
    let mut permissions = fs::metadata(temporary)
        .map_err(|error| io(temporary, error))?
        .permissions();
    permissions.set_mode(mode);
    fs::set_permissions(temporary, permissions).map_err(|error| io(temporary, error))
}

fn io(path: &Path, error: std::io::Error) -> AgentError {
    AgentError::message(format!("{}: {error}", path.display()))
}
