//! Cross-process lock beside a Codex home, matching the reference codex refresh lock.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_plugin_api::PluginError;

const WAIT: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(50);

/// Holds `{home}/.codex.lock` until dropped.
pub(crate) struct CodexLock {
    file: File,
    path: PathBuf,
}

impl Drop for CodexLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Waits for and takes the Codex home lock.
///
/// # Errors
///
/// Returns [`PluginError`] when the lock is not available in time.
pub(crate) async fn acquire(home: &Path) -> Result<CodexLock, PluginError> {
    let path = home.join(".codex.lock");
    if let Some(parent) = home.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|_| PluginError::io("cannot create the Codex home directory"))?;
    }
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match try_take(&path) {
            Ok(lock) => return Ok(lock),
            Err(TakeError::Busy) if std::time::Instant::now() < deadline => {
                hofund_plugin_sdk::sleep(RETRY).await;
            }
            Err(TakeError::Busy) => {
                return Err(PluginError::io(
                    "another process is refreshing the Codex sign-in",
                ));
            }
            Err(TakeError::Io) => {
                return Err(PluginError::io("cannot take the Codex sign-in lock"));
            }
        }
    }
}

#[derive(Debug)]
enum TakeError {
    Busy,
    Io,
}

fn try_take(path: &Path) -> Result<CodexLock, TakeError> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| TakeError::Io)?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0).map_err(|_| TakeError::Io)?;
            file.write_all(b"1").map_err(|_| TakeError::Io)?;
            file.sync_all().map_err(|_| TakeError::Io)?;
            Ok(CodexLock {
                file,
                path: path.to_path_buf(),
            })
        }
        Err(std::fs::TryLockError::WouldBlock) => Err(TakeError::Busy),
        Err(std::fs::TryLockError::Error(_)) => Err(TakeError::Io),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r47b_r1_codex_lock_blocks_second_taker() {
        let home = std::env::temp_dir().join(format!(
            "hofund-codex-lock-{}",
            hofund_plugin_sdk::uuid::v4().expect("uuid")
        ));
        std::fs::create_dir_all(&home).expect("home");
        let path = home.join(".codex.lock");
        let first = try_take(&path).expect("first lock");
        assert!(path.is_file());
        assert!(matches!(try_take(&path), Err(TakeError::Busy)));
        drop(first);
        let _ = std::fs::remove_dir_all(&home);
    }
}
