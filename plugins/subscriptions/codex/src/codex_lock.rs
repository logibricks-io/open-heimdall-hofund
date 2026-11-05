//! Cross-process lock beside a Codex home, matching the reference codex refresh lock.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Duration;

use crate::store::ensure_codex_home;

const WAIT: Duration = Duration::from_secs(10);
const RETRY: Duration = Duration::from_millis(50);

/// Holds the Codex home lock until dropped.
pub(crate) struct CodexLock {
    file: File,
    #[cfg(unix)]
    #[allow(dead_code)]
    lock_path: std::path::PathBuf,
}

impl Drop for CodexLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Waits for and takes the Codex home lock, or returns [`None`] when another
/// holder does not release in time (refresh write-back still proceeds).
pub(crate) async fn acquire(home: &Path) -> Option<CodexLock> {
    if ensure_codex_home(home).is_err() {
        return None;
    }
    let lock_path = home.join(".codex.lock");
    let _ = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path);
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        match try_take(home, &lock_path) {
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

fn try_take(home: &Path, lock_path: &Path) -> Result<CodexLock, TakeError> {
    #[cfg(unix)]
    {
        let dir = OpenOptions::new()
            .read(true)
            .open(home)
            .map_err(|_| TakeError::Io)?;
        match dir.try_lock() {
            Ok(()) => {
                let _ = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .open(lock_path);
                Ok(CodexLock {
                    file: dir,
                    lock_path: lock_path.to_path_buf(),
                })
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(TakeError::Busy),
            Err(std::fs::TryLockError::Error(_)) => Err(TakeError::Io),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = home;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        let file = options.open(lock_path).map_err(|_| TakeError::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(CodexLock { file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(TakeError::Busy),
            Err(std::fs::TryLockError::Error(_)) => Err(TakeError::Io),
        }
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
        let first = try_take(&home, &path).expect("first lock");
        assert!(path.is_file());
        assert!(matches!(try_take(&home, &path), Err(TakeError::Busy)));
        drop(first);
        assert!(path.is_file(), "lock file stays after release");
    }

    #[test]
    fn r47b_rr3_lock_recreate_while_held_blocks_second_writer() {
        let (_dir, home) = lock_home("hofund-codex-lock-recreate");
        assert!(ensure_codex_home(&home).is_ok());
        let path = home.join(".codex.lock");
        let first = try_take(&home, &path).expect("first lock");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"replaced").expect("recreate lock file");
        assert!(
            matches!(try_take(&home, &path), Err(TakeError::Busy)),
            "replacing .codex.lock must not admit a second writer while the home lock lives"
        );
        drop(first);
        assert!(path.is_file(), "lock file stays after release");
    }

    #[test]
    fn r47b_rr2_lock_does_not_truncate_existing_mark() {
        let (_dir, home) = lock_home("hofund-codex-lock-mark");
        assert!(ensure_codex_home(&home).is_ok());
        let path = home.join(".codex.lock");
        std::fs::write(&path, b"held-mark").expect("seed lock file");
        let lock = try_take(&home, &path).expect("lock");
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

    #[test]
    fn r47b_rr3_acquire_timeout_returns_none_without_panic() {
        let (_dir, home) = lock_home("hofund-codex-lock-timeout");
        assert!(ensure_codex_home(&home).is_ok());
        let path = home.join(".codex.lock");
        let first = try_take(&home, &path).expect("first lock");
        let lock = hofund_kit_subscription::block_on(acquire(&home));
        assert!(
            lock.is_none(),
            "second acquire should time out and return None"
        );
        drop(first);
    }
}
