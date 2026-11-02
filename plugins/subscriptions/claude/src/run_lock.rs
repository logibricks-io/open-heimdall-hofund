//! Cross-process locks beside a saved account's config directory.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hofund_plugin_api::{AccountId, HostContext, PluginError};

use crate::account;

const WAIT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(50);
const MARK_ATTEMPTS: usize = 64;

static TEST_WAIT: AtomicU64 = AtomicU64::new(0);
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static TEST_MARK_PID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static TEST_MARK_SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

/// Overrides the run-lock wait in tests. Zero restores the default.
#[cfg(test)]
pub(crate) fn test_run_wait(wait: Duration) {
    TEST_WAIT.store(wait.as_millis() as u64, Ordering::Relaxed);
}

/// Fixes the next mark file's pid and sequence in tests.
#[cfg(test)]
pub(crate) fn test_mark_identity(pid: u32, seq: u64) {
    TEST_MARK_PID.with(|slot| slot.set(pid));
    TEST_MARK_SEQ.with(|slot| slot.set(seq));
}

#[cfg(test)]
pub(crate) fn clear_test_mark_identity() {
    TEST_MARK_PID.with(|slot| slot.set(0));
    TEST_MARK_SEQ.with(|slot| slot.set(u64::MAX));
}

fn wait_limit() -> Duration {
    let ms = TEST_WAIT.load(Ordering::Relaxed);
    if ms == 0 {
        WAIT
    } else {
        Duration::from_millis(ms)
    }
}

fn mark_process_id() -> u32 {
    #[cfg(test)]
    {
        let injected = TEST_MARK_PID.with(|slot| slot.get());
        if injected != 0 {
            return injected;
        }
    }
    mark_process_id_platform()
}

fn mark_process_id_platform() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *ID.get_or_init(|| proc_self_id().unwrap_or_else(process_nonce))
}

#[cfg(unix)]
fn proc_self_id() -> Option<u32> {
    std::fs::read_link("/proc/self").ok().and_then(|path| {
        path.file_name()
            .and_then(|name| name.to_str()?.parse().ok())
            .filter(|id: &u32| *id != 0)
    })
}

#[cfg(not(unix))]
fn proc_self_id() -> Option<u32> {
    None
}

/// Fallback when `/proc` is unavailable (macOS, non-unix): a per-process
/// nonce from the clock and a stack address, so concurrent processes do not
/// share mark names. Never zero.
fn process_nonce() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let marker = 0u8;
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.write_usize(std::ptr::addr_of!(marker) as usize);
    (hasher.finish() as u32).max(1)
}

fn next_mark_seq() -> u64 {
    #[cfg(test)]
    {
        let fixed = TEST_MARK_SEQ.with(|slot| slot.get());
        if fixed != u64::MAX {
            return fixed;
        }
    }
    RUN_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn mark_path(dir: &Path) -> PathBuf {
    let parent = dir.parent().unwrap_or_else(|| Path::new("."));
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("account");
    let pid = mark_process_id();
    let seq = next_mark_seq();
    parent.join(format!("{name}.run-{pid}-{seq}.lock"))
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
    for _ in 0..MARK_ATTEMPTS {
        let path = mark_path(&dir);
        let file = open_lock(&path)?;
        match file.try_lock() {
            Ok(()) => return Ok(RunMark { path, file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(PluginError::io(error.to_string()));
            }
        }
    }
    Err(PluginError::io(
        "could not create a run mark beside the account directory",
    ))
}

enum WaitLockOutcome {
    Acquired,
    TimedOut,
    Gone,
}

/// Waits until no other process runs Claude Code in the account directory.
///
/// # Errors
///
/// Returns [`PluginError`] when a run is still marked past the wait limit.
pub(crate) async fn wait(account: &AccountId, host: &HostContext) -> Result<(), PluginError> {
    let dir = account::account_dir(host, account)?;
    let deadline = Instant::now() + wait_limit();
    for mark in list_marks(&dir) {
        let wait = deadline.saturating_duration_since(Instant::now());
        let file = open_lock(&mark)?;
        match wait_for_lock(&file, &mark, wait).await? {
            WaitLockOutcome::Acquired => {
                let _ = file.unlock();
                let _ = std::fs::remove_file(&mark);
            }
            WaitLockOutcome::TimedOut => {
                return Err(PluginError::auth(
                    "Claude Code is still running on this account for hofund (its usage or a warm-up): try again in a moment",
                ));
            }
            WaitLockOutcome::Gone => {}
        }
    }
    Ok(())
}

async fn wait_for_lock(
    file: &File,
    mark: &Path,
    wait: Duration,
) -> Result<WaitLockOutcome, PluginError> {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(WaitLockOutcome::Acquired),
            Err(std::fs::TryLockError::WouldBlock) => {
                if started.elapsed() >= wait {
                    return Ok(WaitLockOutcome::TimedOut);
                }
                hofund_plugin_sdk::sleep(POLL.min(wait.saturating_sub(started.elapsed()))).await;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                if !mark.exists() {
                    return Ok(WaitLockOutcome::Gone);
                }
                return Err(PluginError::io(error.to_string()));
            }
        }
    }
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
