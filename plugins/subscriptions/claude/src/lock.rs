//! Claude Code's mkdir refresh locks and the profile save lock.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hofund_plugin_api::PluginError;

/// How long a switch waits for Claude Code to release a lock.
const WAIT: Duration = Duration::from_secs(20);
/// How often a held lock is retried.
const POLL: Duration = Duration::from_millis(100);
/// How often a lock hofund holds is touched.
const TOUCH: Duration = Duration::from_secs(2);
/// Stale time for refresh locks.
const REFRESH_STALE: Duration = Duration::from_secs(60);
/// Stale time for the profile save lock.
const CONFIG_STALE: Duration = Duration::from_secs(10);

static TEST_WAIT: AtomicU64 = AtomicU64::new(0);
static TEST_REFRESH_STALE: AtomicU64 = AtomicU64::new(0);
static TEST_CONFIG_STALE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static TEST_RELEASES: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Overrides lock wait and stale durations in tests. Zero restores defaults.
#[cfg(test)]
pub(crate) fn test_lock_timing(wait: Duration, refresh_stale: Duration, config_stale: Duration) {
    TEST_WAIT.store(wait.as_millis() as u64, Ordering::Relaxed);
    TEST_REFRESH_STALE.store(refresh_stale.as_millis() as u64, Ordering::Relaxed);
    TEST_CONFIG_STALE.store(config_stale.as_millis() as u64, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn take_test_releases() -> Vec<&'static str> {
    TEST_RELEASES.with(|seen| std::mem::take(&mut *seen.borrow_mut()))
}

#[cfg(test)]
fn record_release(label: &'static str) {
    TEST_RELEASES.with(|seen| seen.borrow_mut().push(label));
}

fn wait_limit() -> Duration {
    let ms = TEST_WAIT.load(Ordering::Relaxed);
    if ms == 0 {
        WAIT
    } else {
        Duration::from_millis(ms)
    }
}

fn refresh_stale() -> Duration {
    let ms = TEST_REFRESH_STALE.load(Ordering::Relaxed);
    if ms == 0 {
        REFRESH_STALE
    } else {
        Duration::from_millis(ms)
    }
}

fn config_stale() -> Duration {
    let ms = TEST_CONFIG_STALE.load(Ordering::Relaxed);
    if ms == 0 {
        CONFIG_STALE
    } else {
        Duration::from_millis(ms)
    }
}

/// Holds Claude Code's two refresh locks. Lets them go in reverse order on drop.
pub(crate) struct RefreshLocks {
    refresh: LockGuard,
    legacy: LockGuard,
}

/// Holds the lock Claude Code takes before rewriting `.claude.json`.
pub(crate) struct ConfigLock(LockGuard);

impl Drop for RefreshLocks {
    fn drop(&mut self) {
        #[cfg(test)]
        record_release("legacy");
        self.legacy.release();
        #[cfg(test)]
        record_release("refresh");
        self.refresh.release();
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        #[cfg(test)]
        record_release("config");
        self.0.release();
    }
}

struct LockGuard {
    path: PathBuf,
    stop: Option<std::sync::mpsc::Sender<()>>,
    done: Option<std::sync::mpsc::Receiver<()>>,
}

impl LockGuard {
    fn release(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(done) = self.done.take() {
            let _ = done.recv();
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Takes `.oauth_refresh.lock` then `<realpath(config dir)>.lock`.
///
/// # Errors
///
/// Returns [`PluginError`] when a lock stays held past the wait limit.
pub(crate) fn refresh_locks(config_dir: &Path) -> Result<RefreshLocks, PluginError> {
    if !config_dir.is_dir() {
        return Err(PluginError::io(
            "the Claude Code config directory is missing",
        ));
    }
    let mut refresh = take_lock(&config_dir.join(".oauth_refresh.lock"), refresh_stale())?;
    let resolved = std::fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
    let legacy = match take_lock(
        &PathBuf::from(format!("{}.lock", resolved.display())),
        refresh_stale(),
    ) {
        Ok(guard) => guard,
        Err(error) => {
            refresh.release();
            return Err(error);
        }
    };
    Ok(RefreshLocks { refresh, legacy })
}

/// Takes `<realpath(profile)>.lock`.
///
/// # Errors
///
/// Returns [`PluginError`] when the lock stays held past the wait limit.
pub(crate) fn config_lock(profile: &Path) -> Result<ConfigLock, PluginError> {
    let parent = profile
        .parent()
        .filter(|dir| dir.is_dir())
        .ok_or_else(|| PluginError::io("the Claude Code profile directory is missing"))?;
    let base = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let name = profile
        .file_name()
        .ok_or_else(|| PluginError::io("the Claude Code profile path has no file name"))?;
    let lock_target = if profile.exists() {
        std::fs::canonicalize(profile).unwrap_or_else(|_| base.join(name))
    } else {
        base.join(name)
    };
    Ok(ConfigLock(take_lock(
        &PathBuf::from(format!("{}.lock", lock_target.display())),
        config_stale(),
    )?))
}

fn take_lock(path: &Path, stale: Duration) -> Result<LockGuard, PluginError> {
    let deadline = Instant::now() + wait_limit();
    loop {
        match std::fs::create_dir(path) {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_stale(path, stale) && std::fs::remove_dir(path).is_ok() {
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(PluginError::auth(
                        "Claude Code is refreshing its sign-in: try again in a moment",
                    ));
                }
                std::thread::sleep(POLL);
            }
            Err(error) => {
                return Err(PluginError::io(format!(
                    "cannot take Claude Code's lock {}: {error}",
                    path.display()
                )));
            }
        }
    }
    let (stop, stop_rx) = std::sync::mpsc::channel();
    let (done_tx, done) = std::sync::mpsc::channel();
    let touch_path = path.to_path_buf();
    std::thread::spawn(move || {
        while stop_rx.recv_timeout(TOUCH).is_err() {
            touch_dir(&touch_path);
        }
        let _ = done_tx.send(());
    });
    Ok(LockGuard {
        path: path.to_path_buf(),
        stop: Some(stop),
        done: Some(done),
    })
}

fn lock_stale(path: &Path, stale: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified.elapsed().unwrap_or(Duration::ZERO) > stale
}

fn touch_dir(path: &Path) {
    let Ok(file) = std::fs::File::open(path) else {
        return;
    };
    let _ = file.set_modified(std::time::SystemTime::now());
}
