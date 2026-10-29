//! Finding the vendor binary a plugin spawns.
//!
//! The kernel's process handle takes an absolute program path and never
//! runs a shell, so a plugin resolves its binary name here first.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use hofund_plugin_api::BinaryName;

/// The extensions Windows tries when `PATHEXT` is not set.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// The first `name` on the host's `PATH` that is a program, as an absolute
/// path. `None` when there is no such program.
#[must_use]
pub fn find_program(name: BinaryName) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    let pathext = std::env::var_os("PATHEXT");
    find_in(name, path.as_deref(), pathext.as_deref())
}

/// [`find_program`] against an explicit search path.
///
/// Relative entries in `path` are skipped, so the working directory never
/// supplies the program. `pathext` is the Windows `PATHEXT` list and is
/// ignored on other systems.
#[must_use]
pub fn find_in(name: BinaryName, path: Option<&OsStr>, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    let extensions: Vec<String> = if cfg!(windows) {
        pathext
            .and_then(OsStr::to_str)
            .unwrap_or(DEFAULT_PATHEXT)
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(path) {
        if !dir.is_absolute() {
            continue;
        }
        let plain = dir.join(name.as_str());
        if is_program(&plain) {
            return Some(plain);
        }
        for extension in &extensions {
            let candidate = dir.join(format!("{}{extension}", name.as_str()));
            if is_program(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_program(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_program(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: BinaryName = BinaryName::new("example-agent");

    fn write(dir: &Path, name: &str, executable: bool) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").expect("write program");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
        }
        #[cfg(not(unix))]
        let _ = executable;
        path
    }

    fn joined(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs).expect("join")
    }

    #[test]
    fn first_program_on_the_path_wins() {
        let first = tempfile::tempdir().expect("dir");
        let second = tempfile::tempdir().expect("dir");
        let name = if cfg!(windows) {
            "example-agent.exe"
        } else {
            "example-agent"
        };
        let wanted = write(first.path(), name, true);
        write(second.path(), name, true);
        let path = joined(&[first.path(), second.path()]);
        assert_eq!(find_in(NAME, Some(&path), None), Some(wanted));
    }

    #[test]
    fn missing_program_and_missing_path_are_none() {
        let empty = tempfile::tempdir().expect("dir");
        let path = joined(&[empty.path()]);
        assert_eq!(find_in(NAME, Some(&path), None), None);
        assert_eq!(find_in(NAME, None, None), None);
    }

    #[test]
    fn a_directory_with_the_name_is_not_a_program() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir(dir.path().join("example-agent")).expect("mkdir");
        let path = joined(&[dir.path()]);
        assert_eq!(find_in(NAME, Some(&path), None), None);
    }

    #[test]
    fn a_relative_path_entry_is_skipped() {
        let cwd = std::env::current_dir().expect("cwd");
        let dir = tempfile::tempdir_in(&cwd).expect("dir");
        let name = if cfg!(windows) {
            "example-agent.exe"
        } else {
            "example-agent"
        };
        write(dir.path(), name, true);
        let relative = dir.path().strip_prefix(&cwd).expect("under cwd");
        let path = joined(&[relative]);
        assert_eq!(find_in(NAME, Some(&path), None), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_without_execute_permission_is_skipped() {
        let first = tempfile::tempdir().expect("dir");
        let second = tempfile::tempdir().expect("dir");
        write(first.path(), "example-agent", false);
        let wanted = write(second.path(), "example-agent", true);
        let path = joined(&[first.path(), second.path()]);
        assert_eq!(find_in(NAME, Some(&path), None), Some(wanted));
    }

    #[cfg(windows)]
    #[test]
    fn pathext_finds_the_extension() {
        let dir = tempfile::tempdir().expect("dir");
        let wanted = write(dir.path(), "example-agent.cmd", true);
        let path = joined(&[dir.path()]);
        let pathext = std::ffi::OsString::from(".EXE;.CMD");
        assert_eq!(find_in(NAME, Some(&path), Some(&pathext)), Some(wanted));
    }
}
