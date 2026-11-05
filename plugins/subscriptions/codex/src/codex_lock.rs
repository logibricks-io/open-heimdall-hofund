//! Cross-process lock beside a Codex home, matching the reference codex refresh lock.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_millis(50);

/// Holds `{home}/.codex.lock` until dropped.
pub(crate) struct CodexLock {
    file: File,
}

impl Drop for CodexLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Waits for and takes the Codex home lock, or returns [`None`] when another
/// holder does not release in time (refresh write-back still proceeds).
pub(crate) async fn acquire(home: &Path) -> Option<CodexLock> {
    std::fs::create_dir_all(home).ok();
    let path = home.join(".codex.lock");
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match try_take(&path) {
            Ok(lock) => return Some(lock),
            Err(TakeError::Busy) if std::time::Instant::now() < deadline => {
                hofund_plugin_sdk::sleep(RETRY).await;
            }
            Err(TakeError::Busy) | Err(TakeError::Io) => return None,
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
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|_| TakeError::Io)?;
    match file.try_lock() {
        Ok(()) => Ok(CodexLock { file }),
        Err(std::fs::TryLockError::WouldBlock) => Err(TakeError::Busy),
        Err(std::fs::TryLockError::Error(_)) => Err(TakeError::Io),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_home(prefix: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join(format!(
            "{prefix}-{}",
            hofund_plugin_sdk::uuid::v4().expect("uuid")
        ));
        (dir, home)
    }

    #[test]
    fn r47b_r1_codex_lock_blocks_second_taker() {
        let (_dir, home) = lock_home("hofund-codex-lock");
        std::fs::create_dir_all(&home).expect("home");
        let path = home.join(".codex.lock");
        let first = try_take(&path).expect("first lock");
        assert!(path.is_file());
        assert!(matches!(try_take(&path), Err(TakeError::Busy)));
        drop(first);
        assert!(path.is_file(), "lock file stays after release");
    }

    #[test]
    fn r47b_rr2_lock_does_not_truncate_existing_mark() {
        let (_dir, home) = lock_home("hofund-codex-lock-mark");
        std::fs::create_dir_all(&home).expect("home");
        let path = home.join(".codex.lock");
        std::fs::write(&path, b"held-mark").expect("seed lock file");
        let lock = try_take(&path).expect("lock");
        let body = std::fs::read(&path).expect("read lock body");
        assert!(
            body.windows(b"held-mark".len())
                .any(|window| window == b"held-mark"),
            "lock take must not truncate the lock file"
        );
        drop(lock);
    }

    #[test]
    fn r47b_rr2_acquire_creates_codex_home() {
        let (_dir, home) = lock_home("hofund-codex-home-create");
        assert!(!home.exists());
        let lock = hofund_kit_subscription::block_on(acquire(&home));
        assert!(lock.is_some());
        assert!(home.is_dir());
        drop(lock);
    }
}
