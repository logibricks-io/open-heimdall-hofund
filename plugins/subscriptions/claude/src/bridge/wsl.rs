//! WSL path rewrite and argv wrapping for a Claude Code spawn.

use std::ffi::OsString;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

/// Rewrites a Windows path to the WSL mount form (`C:\foo` → `/mnt/c/foo`).
///
/// Non-Windows paths, including already-POSIX paths, are returned unchanged.
#[must_use]
#[cfg(test)]
pub(crate) fn linux_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    let Some(rest) = drive_path(&raw) else {
        return path.to_path_buf();
    };
    let drive = raw.chars().next().expect("drive").to_ascii_lowercase();
    let rest = rest.trim_start_matches(['\\', '/']).replace('\\', "/");
    PathBuf::from(format!("/mnt/{drive}/{rest}"))
}

/// The remainder after a Windows drive prefix such as `C:` or `C:\`.
#[cfg(test)]
fn drive_path(raw: &str) -> Option<&str> {
    let mut chars = raw.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    if chars.next()? != ':' {
        return None;
    }
    match chars.next() {
        Some('\\') | Some('/') | None => Some(&raw[2..]),
        _ => None,
    }
}

/// Argv that runs `program` with `args` inside `distro` via `wsl.exe`.
///
/// `wsl_program` is the absolute Windows path of `wsl.exe`.
#[must_use]
pub(crate) fn wrap_argv(
    wsl_program: &Path,
    distro: &str,
    program: &Path,
    args: &[OsString],
) -> Vec<OsString> {
    let mut argv = vec![
        wsl_program.as_os_str().to_owned(),
        OsString::from("-d"),
        OsString::from(distro),
        OsString::from("--exec"),
        program.as_os_str().to_owned(),
    ];
    argv.extend(args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::{linux_path, wrap_argv};
    use std::ffi::OsString;
    use std::path::Path;
    #[cfg(test)]
    use std::path::PathBuf;

    #[test]
    fn windows_paths_become_mnt_drives() {
        assert_eq!(
            linux_path(Path::new(r"C:\Users\me\AppData\Local\Temp\prompt.txt")),
            PathBuf::from("/mnt/c/Users/me/AppData/Local/Temp/prompt.txt")
        );
        assert_eq!(
            linux_path(Path::new(r"D:\Program Files\hofund\hofund.exe")),
            PathBuf::from("/mnt/d/Program Files/hofund/hofund.exe")
        );
        assert_eq!(
            linux_path(Path::new("/tmp/acct")),
            PathBuf::from("/tmp/acct")
        );
    }

    #[test]
    fn wrap_argv_uses_exec_and_keeps_args() {
        let argv = wrap_argv(
            Path::new(r"C:\Windows\System32\wsl.exe"),
            "Ubuntu",
            Path::new(r"C:\claude\bin\claude.exe"),
            &[
                OsString::from("-p"),
                OsString::from("--model"),
                OsString::from("claude-sonnet-4-5"),
            ],
        );
        let text: Vec<String> = argv
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text,
            vec![
                r"C:\Windows\System32\wsl.exe".to_owned(),
                "-d".to_owned(),
                "Ubuntu".to_owned(),
                "--exec".to_owned(),
                r"C:\claude\bin\claude.exe".to_owned(),
                "-p".to_owned(),
                "--model".to_owned(),
                "claude-sonnet-4-5".to_owned(),
            ]
        );
    }
}
