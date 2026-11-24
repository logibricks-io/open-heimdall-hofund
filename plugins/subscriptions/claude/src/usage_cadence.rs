//! Background `/usage` cadence for the active Claude Code account (the reference
//! `claudeWindows` rules).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::AccountId;

use crate::cli;
use crate::creds::Profile;

/// Minimum gap between two `/usage` runs.
pub(crate) const ASK_FLOOR: Duration = Duration::from_secs(30);

const WAIT_MIN: Duration = Duration::from_secs(5 * 60);
const WAIT_MAX: Duration = Duration::from_secs(15 * 60);
const WAIT_SPAN_MINUTES: u64 = WAIT_MAX.as_secs() / 60 - WAIT_MIN.as_secs() / 60 + 1;

/// How the daemon or user reached the quota call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuotaIntent {
    /// `hofund subscription quota`, sign-in listing, or an explicit card refresh.
    Asked,
    /// Subscription quota tick with no card refresh.
    UnaskedPoll,
}

/// Whether Claude Code's `/usage` should run now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadDecision {
    pub(crate) run_usage: bool,
}

static ASK_GENERATION: AtomicU64 = AtomicU64::new(0);

type WaitDraw = fn() -> Duration;
type UsedSinceFn = fn(SystemTime, &Path) -> bool;

struct HookState {
    wait: WaitDraw,
    used_since: UsedSinceFn,
}

static HOOKS: std::sync::Mutex<HookState> = std::sync::Mutex::new(HookState {
    wait: random_usage_wait,
    used_since: default_used_since,
});

/// Draws the post-run wait: a whole minute between five and fifteen minutes.
#[must_use]
pub(crate) fn usage_wait() -> Duration {
    let draw = HOOKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .wait;
    draw()
}

fn random_usage_wait() -> Duration {
    let span = WAIT_SPAN_MINUTES;
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    static SALT: AtomicU64 = AtomicU64::new(1);
    let mixed = seed ^ u128::from(SALT.fetch_add(1, Ordering::Relaxed));
    let offset = (mixed % u128::from(span)) as u32;
    WAIT_MIN + Duration::from_secs(u64::from(offset) * 60)
}

/// Whether a `.jsonl` session under Claude Code's `projects/` was touched after `since`.
#[must_use]
pub(crate) fn claude_used_since(since: SystemTime, projects_root: &Path) -> bool {
    let used = HOOKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .used_since;
    used(since, projects_root)
}

fn default_used_since(since: SystemTime, projects_root: &Path) -> bool {
    let projects = projects_root.join("projects");
    let Ok(entries) = std::fs::read_dir(projects) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|ty| ty.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for file in files.flatten() {
            if file.path().extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(meta) = file.metadata() else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            if modified > since {
                return true;
            }
        }
    }
    false
}

/// Whether `account` is the one Claude Code is signed in to under the agent home.
#[must_use]
pub(crate) fn is_active_account(account: &AccountId) -> bool {
    let Some(home) = cli::agent_home() else {
        return false;
    };
    match Profile::read(&home).account_id() {
        Ok(id) => id == *account,
        Err(_) => false,
    }
}

/// Projects root to watch for Claude Code activity on the active account.
#[must_use]
pub(crate) fn activity_root_for_active() -> Option<std::path::PathBuf> {
    cli::agent_home()
}

pub(crate) fn mark_usage_asked() -> u64 {
    ASK_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

pub(crate) fn current_ask_generation() -> u64 {
    ASK_GENERATION.load(Ordering::SeqCst)
}

/// Decides whether `/usage` should run for `account`.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub(crate) fn decide_read(
    tried: Option<SystemTime>,
    heard_at: Option<SystemTime>,
    wait: Duration,
    last_asked: u64,
    now: SystemTime,
    active: bool,
    card_refresh: bool,
    intent: QuotaIntent,
) -> ReadDecision {
    let asked = matches!(intent, QuotaIntent::Asked);
    let ask_gen = current_ask_generation();

    let floor_ok =
        tried.is_none_or(|at| now.duration_since(at).unwrap_or(Duration::ZERO) >= ASK_FLOOR);

    let used_since_tried = tried.is_some_and(|at| {
        let root = activity_root_for_active().unwrap_or_default();
        claude_used_since(at, &root)
    });
    let due = tried.is_none()
        || (asked && ask_gen > last_asked)
        || active
            && tried.is_some_and(|at| now.duration_since(at).unwrap_or(Duration::ZERO) >= wait)
            && (heard_at.is_some_and(|h| h > tried.unwrap()) || used_since_tried);

    let may_read_saved = card_refresh && due && floor_ok;
    let may_read_active = active && due && floor_ok;

    let run_usage = match intent {
        QuotaIntent::Asked => due && floor_ok,
        QuotaIntent::UnaskedPoll => may_read_active || may_read_saved,
    };

    ReadDecision { run_usage }
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use super::*;

    pub(crate) fn set_wait_draw(draw: WaitDraw) {
        HOOKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .wait = draw;
    }

    pub(crate) fn set_used_since(check: UsedSinceFn) {
        HOOKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .used_since = check;
    }

    pub(crate) fn reset_hooks() {
        ASK_GENERATION.store(0, Ordering::SeqCst);
        let mut hooks = HOOKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        hooks.wait = random_usage_wait;
        hooks.used_since = default_used_since;
    }

    pub(crate) fn lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;
    use test_hooks::{reset_hooks, set_used_since, set_wait_draw};

    const TEST_WAIT: Duration = Duration::from_secs(10 * 60);

    #[test]
    fn r5_cadence_1_unasked_inside_wait_without_use_skips_read() {
        let _lock = test_hooks::lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        set_used_since(|_, _| false);
        let tried = UNIX_EPOCH + Duration::from_secs(1_000);
        let now = tried + Duration::from_secs(60);
        let decision = decide_read(
            Some(tried),
            None,
            TEST_WAIT,
            0,
            now,
            true,
            false,
            QuotaIntent::UnaskedPoll,
        );
        assert!(!decision.run_usage);
    }

    #[test]
    fn r5_cadence_2_wait_due_and_used_since_allows_read() {
        let _lock = test_hooks::lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        set_used_since(|_, _| true);
        let tried = UNIX_EPOCH + Duration::from_secs(1_000);
        let now = tried + TEST_WAIT;
        let decision = decide_read(
            Some(tried),
            None,
            TEST_WAIT,
            0,
            now,
            true,
            false,
            QuotaIntent::UnaskedPoll,
        );
        assert!(decision.run_usage);
        let wait = usage_wait();
        assert!(wait >= WAIT_MIN && wait <= WAIT_MAX);
        assert_eq!(wait.as_secs() % 60, 0);
    }

    #[test]
    fn r5_cadence_3_ask_floor_blocks_back_to_back_asks() {
        let _lock = test_hooks::lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        set_used_since(|_, _| true);
        let tried = UNIX_EPOCH + Duration::from_secs(2_000);
        let now = tried + Duration::from_secs(5);
        mark_usage_asked();
        let too_soon = decide_read(
            Some(now),
            Some(tried),
            TEST_WAIT,
            0,
            now + Duration::from_secs(5),
            true,
            true,
            QuotaIntent::Asked,
        );
        assert!(!too_soon.run_usage);
    }

    #[test]
    fn r5_cadence_4_saved_account_unasked_poll_never_reads() {
        let _lock = test_hooks::lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        let tried = UNIX_EPOCH + Duration::from_secs(1_000);
        let now = tried + TEST_WAIT + Duration::from_secs(60);
        let decision = decide_read(
            Some(tried),
            None,
            TEST_WAIT,
            0,
            now,
            false,
            false,
            QuotaIntent::UnaskedPoll,
        );
        assert!(!decision.run_usage);
    }

    #[test]
    fn r5_cadence_5_user_asked_saved_runs_without_card_refresh_flag() {
        let _lock = test_hooks::lock();
        reset_hooks();
        set_wait_draw(|| TEST_WAIT);
        let decision = decide_read(
            None,
            None,
            TEST_WAIT,
            0,
            UNIX_EPOCH + Duration::from_secs(5),
            false,
            false,
            QuotaIntent::Asked,
        );
        assert!(decision.run_usage);
    }
}
