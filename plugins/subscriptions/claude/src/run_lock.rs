//! Cross-process locks beside a saved account's config directory.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hofund_plugin_api::{AccountId, HostContext, PluginError};

use crate::account;

const WAIT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(50);

static TEST_WAIT: AtomicU64 = AtomicU64::new(0);
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Overrides the run-lock wait in tests. Zero restores the default.
#[cfg(test)]
pub(crate) fn test_run_wait(wait: Duration) {
    TEST_WAIT.store(wait.as_millis() as u64, Ordering::Relaxed);
}

fn wait_limit() -> Duration {
    let ms = TEST_WAIT.load(Ordering::Relaxed);
    if ms == 0 {
        WAIT
    } else {
        Duration::from_millis(ms)
    }
}

fn mark_path(dir: &Path) -> PathBuf {
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("account");
    parent.join(format!("{name}.run-{}.lock", RUN_SEQ.fetch_add(1, Ordering::Relaxed)))
}

fn list_marks(dir: &Path) -> Vec<PathBuf> {
    let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let prefix = format!("{name}.run-");
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|file| file.to_str())
                .is_some_and(|file| file.starts_with(&prefix) && file.ends_with(".lock"))
        })
        .collect()
}

/// Holds a run lock until dropped.
pub(crate) struct RunMark {
    path: PathBuf,
    file: File,
}

impl Drop for RunMark {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Marks this process as running Claude Code in the account directory.
pub(crate) fn mark(account: &AccountId, host: &HostContext) -> Result<RunMark, PluginError> {
    let dir = account::account_dir(host, account)?;
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|error| PluginError::io(error.to_string()))?;
    }
    let path = mark_path(&dir);
    let file = open_lock(&path)?;
    file.try_lock()
        .map_err(|error| PluginError::io(error.to_string()))?;
    Ok(RunMark { path, file })
}

/// Waits until no other process runs Claude Code in the account directory.
///
/// # Errors
///
/// Returns [`PluginError`] when a run is still marked past the wait limit.
pub(crate) fn wait(account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
    let dir = account::account_dir(host, account)?;
    let deadline = Instant::now() + wait_limit();
    for mark in list_marks(&dir) {
        let wait = deadline.saturating_duration_since(Instant::now());
        let started = Instant::now();
        let file = open_lock(&mark)?;
        if try_wait_lock(&file, wait).is_err() && started.elapsed() >= wait {
            return Err(PluginError::auth(
                "Claude Code is still running on this account for hofund (its usage or a warm-up): try again in a moment",
            ));
        }
        let _ = file.unlock();
        let _ = std::fs::remove_file(&mark);
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File, PluginError> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| PluginError::io(error.to_string()))
}

fn try_wait_lock(file: &File, wait: Duration) -> Result<(), ()> {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                if started.elapsed() >= wait {
                    return Err(());
                }
                std::thread::sleep(POLL.min(wait.saturating_sub(started.elapsed())));
            }
            Err(std::fs::TryLockError::Error(_)) => return Err(()),
        }
    }
}
