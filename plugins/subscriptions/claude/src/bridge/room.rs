//! Concurrent Claude Code process cap (reference `claudeRoom`).
//!
//! A fresh start waits up to [`ROOM_WAIT`] for a slot, then returns 503
//! without cooldown. Idle reuse does not take a new slot: the parked process
//! already holds one. `skip_failover_same_plugin` stays in R-6g.
//!
//! Images, effort, and StillWorking stay in R-6f.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hofund_plugin_api::PluginError;

/// How long a fresh start waits for room (reference `claudeRoomWait`).
pub(crate) const ROOM_WAIT: Duration = Duration::from_secs(120);

/// English sentence every room-full 503 contains (reference `claudeNoRoomSaid`).
pub(crate) const NO_ROOM_SAID: &str =
    "Claude Code processes are running, as many as may run at once";

/// Memory reserved for hofund and the rest of the machine, in MiB.
const RESERVE_MIB: i64 = 256;
/// Memory one Claude Code process is given, in MiB.
const RUN_MIB: i64 = 128;

#[derive(Debug)]
struct Inner {
    live: usize,
    waiting: usize,
}

/// Counting semaphore for Claude Code processes owned by one plugin instance.
#[derive(Debug, Clone)]
pub(crate) struct Room {
    inner: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<Inner>,
    cv: Condvar,
    /// 0 means unbounded.
    most: usize,
    wait: Duration,
    why: String,
}

impl Default for Room {
    fn default() -> Self {
        Self::unbounded()
    }
}

impl Room {
    /// No cap: every take succeeds immediately.
    #[must_use]
    pub(crate) fn unbounded() -> Self {
        Self::new(0, ROOM_WAIT, "unbounded")
    }

    /// A cap of `most` (or unbounded when `most == 0`) with the reference wait.
    #[must_use]
    pub(crate) fn with_most(most: usize, why: impl Into<String>) -> Self {
        Self::new(most, ROOM_WAIT, why)
    }

    /// Like [`Self::with_most`] but with an injected wait, for tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_most_and_wait(most: usize, wait: Duration, why: impl Into<String>) -> Self {
        Self::new(most, wait, why)
    }

    fn new(most: usize, wait: Duration, why: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Shared {
                state: Mutex::new(Inner {
                    live: 0,
                    waiting: 0,
                }),
                cv: Condvar::new(),
                most,
                wait,
                why: why.into(),
            }),
        }
    }

    /// Default cap from a configured `claude-runs-most`, else this machine's
    /// memory heuristic, else unbounded.
    #[must_use]
    pub(crate) fn from_config(configured: Option<usize>) -> Self {
        if let Some(most) = configured.filter(|most| *most > 0) {
            return Self::with_most(most, "claude-runs-most in config");
        }
        match mem_runs() {
            Some((most, why)) => Self::with_most(most, why),
            None => Self::unbounded(),
        }
    }

    /// Current live count (tests).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn live(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .live
    }

    /// Waits for a slot. `evict` is called while the room is full and may
    /// free an idle run; it should return whether it let one go.
    pub(crate) fn take(&self, mut evict: impl FnMut() -> bool) -> Result<RoomGuard, PluginError> {
        let shared = &self.inner;
        if shared.most == 0 {
            let mut inner = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.live += 1;
            return Ok(RoomGuard {
                inner: Arc::clone(shared),
                released: false,
            });
        }
        let started = Instant::now();
        let mut inner = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut waiting = false;
        loop {
            if inner.live < shared.most {
                inner.live += 1;
                if waiting {
                    inner.waiting = inner.waiting.saturating_sub(1);
                }
                return Ok(RoomGuard {
                    inner: Arc::clone(shared),
                    released: false,
                });
            }
            if !waiting {
                waiting = true;
                inner.waiting += 1;
            }
            drop(inner);
            let _ = evict();
            inner = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.live < shared.most {
                continue;
            }
            let remaining = shared.wait.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                inner.waiting = inner.waiting.saturating_sub(1);
                let live = inner.live;
                drop(inner);
                return Err(full_error(live, shared.most, &shared.why, shared.wait));
            }
            let (guard, timeout) = shared
                .cv
                .wait_timeout(inner, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner = guard;
            if timeout.timed_out() && inner.live >= shared.most {
                inner.waiting = inner.waiting.saturating_sub(1);
                let live = inner.live;
                drop(inner);
                return Err(full_error(live, shared.most, &shared.why, shared.wait));
            }
        }
    }

    fn release_shared(shared: &Shared) {
        let mut inner = shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.live = inner.live.saturating_sub(1);
        shared.cv.notify_one();
    }

    /// Wakes waiters so they can evict a newly idle run.
    pub(crate) fn nudge(&self) {
        self.inner.cv.notify_all();
    }
}

/// Frees a room slot when dropped.
#[derive(Debug)]
pub(crate) struct RoomGuard {
    inner: Arc<Shared>,
    released: bool,
}

impl Drop for RoomGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        Room::release_shared(&self.inner);
    }
}

fn full_error(live: usize, most: usize, why: &str, waited: Duration) -> PluginError {
    let waited = if waited.as_secs() >= 1 {
        format!("{}s", waited.as_secs())
    } else {
        format!("{}ms", waited.as_millis())
    };
    PluginError::status_error(
        503,
        format!(
            "{live} {NO_ROOM_SAID} ({why}, cap {most}); none ended within {waited}. \
Set how many may run with `claude-runs-most`"
        ),
    )
    .without_cooldown()
}

/// How many Claude Code processes `limit_bytes` of memory hold.
#[must_use]
pub(crate) fn runs_for(limit_bytes: u64) -> usize {
    let mib = (limit_bytes / (1024 * 1024)) as i64;
    ((mib - RESERVE_MIB) / RUN_MIB).max(1) as usize
}

/// Reads this machine's memory cap, if it can be known on Linux.
fn mem_runs() -> Option<(usize, String)> {
    let (limit, why) = mem_limit()?;
    let most = runs_for(limit);
    Some((
        most,
        format!(
            "{} MiB of memory ({why}) holds {most}",
            limit / (1024 * 1024)
        ),
    ))
}

fn mem_limit() -> Option<(u64, String)> {
    #[cfg(not(unix))]
    {
        None
    }
    #[cfg(unix)]
    {
        if std::env::consts::OS != "linux" {
            return None;
        }
        let mut limit = 0_u64;
        let mut why = String::new();
        let mut consider = |n: u64, from: &str| {
            if n > 0 && (limit == 0 || n < limit) {
                limit = n;
                why = from.to_owned();
            }
        };
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    let kb: u64 = rest
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if kb > 0 {
                        consider(kb * 1024, "MemTotal");
                    }
                }
            }
        }
        if limit == 0 {
            return None;
        }
        Some((limit, why))
    }
}

#[cfg(test)]
mod tests {
    use super::{NO_ROOM_SAID, Room, runs_for};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn runs_for_matches_the_reference_heuristic() {
        assert_eq!(runs_for(512 * 1024 * 1024), 2);
        assert_eq!(runs_for(1024 * 1024 * 1024), 6);
        assert_eq!(runs_for(128 * 1024 * 1024), 1);
    }

    #[test]
    fn unbounded_take_always_succeeds() {
        let room = Room::unbounded();
        let a = room.take(|| false).expect("a");
        let b = room.take(|| false).expect("b");
        assert_eq!(room.live(), 2);
        drop(a);
        drop(b);
        assert_eq!(room.live(), 0);
    }

    #[test]
    fn a_full_room_returns_503_without_cooldown() {
        let room = Room::with_most_and_wait(1, Duration::from_millis(30), "test cap");
        let _hold = room.take(|| false).expect("first");
        let error = room.take(|| false).expect_err("full");
        assert_eq!(error.status(), Some(503));
        assert!(!error.cools());
        assert!(
            error.message().contains(NO_ROOM_SAID),
            "message was {}",
            error.message()
        );
        assert!(error.message().contains("claude-runs-most"));
    }

    #[test]
    fn evicting_an_idle_run_frees_a_slot() {
        let room = Arc::new(Room::with_most_and_wait(
            1,
            Duration::from_millis(200),
            "test cap",
        ));
        let hold = room.take(|| false).expect("first");
        let waiter = room.clone();
        let evicted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&evicted);
        let room_for_thread = room.clone();
        let handle = std::thread::spawn(move || {
            let mut hold = Some(hold);
            waiter
                .take(|| {
                    if let Some(guard) = hold.take() {
                        drop(guard);
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        room_for_thread.nudge();
                        return true;
                    }
                    false
                })
                .map(|guard| {
                    drop(guard);
                    true
                })
        });
        let got = handle.join().expect("thread").expect("room after evict");
        assert!(got);
        assert!(evicted.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(room.live(), 0);
    }
}
