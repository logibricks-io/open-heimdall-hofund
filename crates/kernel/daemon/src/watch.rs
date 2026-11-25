//! Follows hand edits to the configuration file (L6c).
//!
//! The parent directories of the configuration path and of its resolved
//! symlink target are watched, not the file: an editor that saves through a
//! temporary file and a rename replaces the file, and a watch on the old one
//! would go quiet. Events for any other name in those directories (the usage
//! ledger, the token, the sidecar `.lock`, editor swap files) are ignored.
//! Matching events are debounced on the trailing edge. One in-flight check is
//! followed by a bounded disk recheck, and later events still debounce. When
//! a check leaves an edit unapplied, disk-stamp comparisons stay armed at the
//! same quiet-period cadence until a stable correction is applied, even if
//! notify delivers no further event. A healthy stable file consumes the arm.
//! A rescan or overflow notice, or a watcher error, forces an immediate check
//! because events may be lost.
//! When the watcher cannot be set up, or its event stream ends, the file is
//! polled instead by length, modification time, and content revision.
//!
//! Every check is a watcher-origin reload. It skips a revision that is
//! already published, so the daemon's own apply writes are not reloaded a
//! second time, and an invalid edit keeps the last good configuration and
//! emits `config_rejected`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::config_ops::{ConfigOps, ReloadOrigin};

/// Quiet period after the last matching event before the file is checked.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(400);
/// Interval of the stat poll used when no watcher is available.
pub(crate) const POLL: Duration = Duration::from_secs(2);

/// Debounce and poll intervals; tests shorten the poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WatchTiming {
    pub(crate) debounce: Duration,
    pub(crate) poll: Duration,
}

impl Default for WatchTiming {
    fn default() -> Self {
        Self {
            debounce: DEBOUNCE,
            poll: POLL,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckOutcome {
    Applied,
    Unapplied,
}

type CheckFuture = Pin<Box<dyn Future<Output = CheckOutcome> + Send>>;
/// One look at the file on disk: a watcher-origin reload in the daemon.
pub(crate) type Check = Arc<dyn Fn() -> CheckFuture + Send + Sync>;
type Events = mpsc::UnboundedReceiver<notify::Result<Event>>;

/// Starts following `path` for the daemon's lifetime.
pub(crate) fn spawn(ops: Arc<ConfigOps>, path: PathBuf) -> JoinHandle<()> {
    let check: Check = Arc::new(move || {
        let ops = Arc::clone(&ops);
        Box::pin(async move {
            match ops.reload(None, ReloadOrigin::Watcher).await {
                Ok(_) => CheckOutcome::Applied,
                Err(failure) => {
                    tracing::warn!(
                        kind = ?failure.error.kind,
                        "configuration edit on disk was not applied; the last good configuration stays"
                    );
                    CheckOutcome::Unapplied
                }
            }
        })
    });
    spawn_with(path, WatchTiming::default(), check)
}

/// Starts following `path`, calling `check` for each settled change.
pub(crate) fn spawn_with(path: PathBuf, timing: WatchTiming, check: Check) -> JoinHandle<()> {
    tokio::spawn(async move {
        match setup(&path) {
            Ok((watcher, targets, events)) => {
                follow(&path, timing, &check, targets, events, Some(watcher)).await;
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "cannot watch the configuration; polling it every {} s",
                    timing.poll.as_secs_f32()
                );
                poll(&path, timing.poll, &check).await;
            }
        }
    })
}

/// The names that count as the configuration file and the directories that
/// hold them, all canonicalized so that `/var` and `/private/var` agree.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Targets {
    files: HashSet<PathBuf>,
    names: HashSet<OsString>,
    dirs: HashSet<PathBuf>,
}

/// Uses the spelling on disk when a case-insensitive file system accepted a
/// differently-cased configuration path. An exact entry always wins.
fn on_disk_name(name: &std::ffi::OsStr, entries: impl IntoIterator<Item = OsString>) -> OsString {
    let mut folded = None;
    for entry in entries {
        if entry == name {
            return entry;
        }
        if folded.is_none()
            && entry
                .as_encoded_bytes()
                .eq_ignore_ascii_case(name.as_encoded_bytes())
        {
            folded = Some(entry);
        }
    }
    folded.unwrap_or_else(|| name.to_owned())
}

impl Targets {
    pub(crate) fn resolve(path: &Path) -> io::Result<Self> {
        let absolute = std::path::absolute(path)?;
        let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the configuration path names no file",
            ));
        };
        let parent = fs::canonicalize(parent)?;
        let entries = fs::read_dir(&parent)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name());
        let name = on_disk_name(name, entries);
        let mut targets = Self::default();
        targets.add(parent.join(name));
        // A symlinked configuration is written through to its target, so the
        // target's directory is watched too.
        if let Ok(real) = fs::canonicalize(&absolute) {
            targets.add(real);
        }
        Ok(targets)
    }

    fn add(&mut self, file: PathBuf) {
        if let Some(dir) = file.parent() {
            self.dirs.insert(dir.to_path_buf());
        }
        if let Some(name) = file.file_name() {
            self.names.insert(name.to_owned());
        }
        self.files.insert(file);
    }

    /// Whether an event path names the configuration file. Only a path with a
    /// matching file name pays for canonicalizing its directory.
    pub(crate) fn matches(&self, path: &Path) -> bool {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        if !self.names.contains(name) {
            return false;
        }
        let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        self.files.contains(&parent.join(name))
    }
}

fn setup(path: &Path) -> notify::Result<(RecommendedWatcher, Targets, Events)> {
    let targets = Targets::resolve(path)?;
    let (sender, events) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })?;
    for dir in &targets.dirs {
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
    }
    Ok((watcher, targets, events))
}

enum Step {
    Settle,
    Force,
    Ignore,
}

fn classify(targets: &Targets, event: &notify::Result<Event>) -> Step {
    match event {
        // A watcher error may have dropped events.
        Err(_) => Step::Force,
        // A rescan flag, or an event kind the backend could not classify, may
        // stand for changes whose paths were not reported.
        Ok(event) if event.need_rescan() || matches!(event.kind, EventKind::Other) => Step::Force,
        // Reads, opens, and closes without a write must not schedule a check.
        // inotify reports a finished write as Access(Close(Write)).
        Ok(event)
            if matches!(
                event.kind,
                EventKind::Access(
                    AccessKind::Any
                        | AccessKind::Read
                        | AccessKind::Open(_)
                        | AccessKind::Close(
                            AccessMode::Any
                                | AccessMode::Execute
                                | AccessMode::Read
                                | AccessMode::Other
                        )
                )
            ) =>
        {
            Step::Ignore
        }
        Ok(event) if event.paths.iter().any(|path| targets.matches(path)) => Step::Settle,
        Ok(_) => Step::Ignore,
    }
}

/// Why the quiet-period timer is running.
enum Wake {
    /// Trailing edge of a matching notify event. The expiry always checks.
    Event,
    /// Disk stamp sampled when the in-flight check returned. The expiry
    /// checks only when the file has moved since that sample.
    Disk(DiskArm),
}

struct DiskArm {
    previous: Option<Stamp>,
    persistent: bool,
}

struct Quiet {
    at: Instant,
    wake: Wake,
}

enum Incoming {
    Event(Option<notify::Result<Event>>),
    Tick,
}

/// Debounces watcher events until the stream ends, then polls.
pub(crate) async fn follow(
    path: &Path,
    timing: WatchTiming,
    check: &Check,
    mut targets: Targets,
    mut events: Events,
    mut watcher: Option<RecommendedWatcher>,
) {
    let mut quiet: Option<Quiet> = None;
    loop {
        let pending = quiet.take();
        let at = pending.as_ref().map(|pending| pending.at);
        // Prefer a queued watcher event over the quiet-period tick so a
        // rescan, a watcher error, or a new edit is not stuck behind a stat.
        let incoming = match at {
            Some(at) => tokio::select! {
                biased;
                event = events.recv() => Incoming::Event(event),
                () = tokio::time::sleep_until(at) => Incoming::Tick,
            },
            None => Incoming::Event(events.recv().await),
        };
        match incoming {
            Incoming::Tick => {
                let Some(pending) = pending else {
                    continue;
                };
                match pending.wake {
                    Wake::Event => {
                        quiet = Some(
                            look_and_arm(
                                path,
                                check,
                                &mut targets,
                                watcher.as_mut(),
                                timing.debounce,
                            )
                            .await,
                        );
                    }
                    Wake::Disk(arm) => {
                        if stamp(path) != arm.previous {
                            quiet = Some(
                                look_and_arm(
                                    path,
                                    check,
                                    &mut targets,
                                    watcher.as_mut(),
                                    timing.debounce,
                                )
                                .await,
                            );
                        } else if arm.persistent {
                            quiet = Some(Quiet {
                                at: Instant::now() + timing.debounce,
                                wake: Wake::Disk(arm),
                            });
                        }
                    }
                }
            }
            Incoming::Event(event) => {
                let Some(event) = event else { break };
                match classify(&targets, &event) {
                    Step::Settle | Step::Force
                        if matches!(
                            &pending,
                            Some(Quiet {
                                wake: Wake::Disk(arm),
                                ..
                            }) if stamp(path) == arm.previous
                        ) =>
                    {
                        // A late event for bytes already checked must not repeat
                        // a rejection. Keep the existing recovery arm.
                        quiet = pending;
                    }
                    Step::Settle => {
                        quiet = Some(Quiet {
                            at: Instant::now() + timing.debounce,
                            wake: Wake::Event,
                        });
                    }
                    Step::Force => {
                        quiet = Some(
                            look_and_arm(
                                path,
                                check,
                                &mut targets,
                                watcher.as_mut(),
                                timing.debounce,
                            )
                            .await,
                        );
                    }
                    Step::Ignore => quiet = pending,
                }
            }
        }
    }
    drop(watcher);
    tracing::warn!(
        path = %path.display(),
        "configuration watcher stopped; polling it every {} s",
        timing.poll.as_secs_f32()
    );
    // A change may have landed while the stream was ending. Retry a rejection
    // so a correction written during that check is not absorbed by poll's baseline.
    if check().await == CheckOutcome::Unapplied {
        check().await;
    }
    poll(path, timing.poll, check).await;
}

struct Look {
    baseline: Option<Stamp>,
    persistent: bool,
}

/// Runs one check and arms its stamp for the quiet period.
async fn look_and_arm(
    path: &Path,
    check: &Check,
    targets: &mut Targets,
    watcher: Option<&mut RecommendedWatcher>,
    debounce: Duration,
) -> Quiet {
    let looked = look(path, check, targets, watcher).await;
    Quiet {
        at: Instant::now() + debounce,
        wake: Wake::Disk(DiskArm {
            previous: looked.baseline,
            persistent: looked.persistent,
        }),
    }
}

/// Runs the in-flight check, then at most one immediate disk recheck.
///
/// The baseline is the stamp sampled after the in-flight check, before that
/// recheck. Recovery remains armed after an unapplied or unstable check until
/// a later check applies a stable file. Watcher events keep being read.
async fn look(
    path: &Path,
    check: &Check,
    targets: &mut Targets,
    watcher: Option<&mut RecommendedWatcher>,
) -> Look {
    let observed = stamp(path);
    let mut outcome = check().await;
    let after = stamp(path);
    let mut unstable = false;
    // Cap the tight loop at one recheck. A save storm must return to `follow`
    // so later writes still debounce and queued events are still serviced.
    if after != observed {
        outcome = check().await;
        unstable = stamp(path) != after;
    }
    let persistent = outcome == CheckOutcome::Unapplied || unstable;
    let Ok(current) = Targets::resolve(path) else {
        return Look {
            baseline: after,
            persistent,
        };
    };
    if let Some(watcher) = watcher {
        for dir in current.dirs.difference(&targets.dirs) {
            if let Err(error) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                tracing::warn!(dir = %dir.display(), %error, "cannot watch the new configuration target");
            }
        }
    }
    *targets = current;
    Look {
        baseline: after,
        persistent,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    revision: String,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = fs::metadata(path).ok()?;
    let bytes = fs::read(path).ok()?;
    Some(Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        revision: crate::control::revision(&bytes),
    })
}

/// Checks the file whenever its length, modification time, or content changes.
pub(crate) async fn poll(path: &Path, every: Duration, check: &Check) {
    let mut last = stamp(path);
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticks.tick().await;
    loop {
        ticks.tick().await;
        let current = stamp(path);
        if current != last {
            last = current;
            check().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use notify::event::{AccessKind, AccessMode, Flag, ModifyKind};
    use tokio::task::JoinHandle;

    use super::*;

    /// Aborts the watcher task on every exit, including an assertion failure.
    struct AbortOnDrop(JoinHandle<()>);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    fn counter() -> (Arc<AtomicUsize>, Check) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let check: Check = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { CheckOutcome::Applied })
        });
        (count, check)
    }

    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("scratch");
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
        (dir, path)
    }

    fn fast() -> WatchTiming {
        WatchTiming {
            debounce: DEBOUNCE,
            poll: Duration::from_millis(100),
        }
    }

    fn modified(path: &Path) -> notify::Result<Event> {
        Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.to_path_buf()))
    }

    /// Writes files and directories next to the configuration that must not
    /// count as configuration changes.
    fn write_siblings(dir: &Path) {
        for name in [
            "hofund.usage.jsonl",
            "usage.sqlite",
            "usage.sqlite-wal",
            "usage.sqlite-journal",
            "hofund.token",
            "hofund.toml.lock",
            "hofund.toml.tmp",
            ".hofund.toml.swp",
        ] {
            fs::write(dir.join(name), "x").expect("sibling");
        }
        fs::create_dir(dir.join(".hofund")).expect("discovery dir");
        fs::write(dir.join(".hofund").join("daemon.json"), "{}").expect("discovery");
        fs::create_dir(dir.join("sub")).expect("subdir");
        fs::write(dir.join("sub").join("hofund.toml"), "x").expect("nested");
    }

    /// Waits up to `limit` for `count` to reach `want`.
    async fn reaches(count: &AtomicUsize, want: usize, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if count.load(Ordering::SeqCst) >= want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        count.load(Ordering::SeqCst) >= want
    }

    /// Replaces `path` the way editors do: write a temporary file beside it,
    /// then rename it over the original.
    fn save_by_rename(path: &Path, text: &str) {
        let temporary = path.with_extension("toml.tmp");
        fs::write(&temporary, text).expect("temporary");
        fs::rename(&temporary, path).expect("rename");
    }

    #[test]
    fn default_timing_is_400_ms_debounce_and_2_s_poll() {
        assert_eq!(
            WatchTiming::default(),
            WatchTiming {
                debounce: Duration::from_millis(400),
                poll: Duration::from_secs(2),
            }
        );
    }

    #[test]
    fn same_name_in_another_directory_is_not_the_configuration() {
        let (dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        fs::create_dir(dir.path().join("sub")).expect("subdir");
        assert!(targets.matches(&path));
        assert!(!targets.matches(&dir.path().join("sub").join("hofund.toml")));
        assert!(!targets.matches(&dir.path().join("hofund.toml.tmp")));
    }

    #[test]
    fn configuration_access_does_not_schedule_a_check() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let event = Ok(Event::new(EventKind::Access(AccessKind::Any)).add_path(path));
        assert!(matches!(classify(&targets, &event), Step::Ignore));
    }

    #[test]
    fn configuration_close_after_write_schedules_a_check() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        for kind in [
            AccessKind::Read,
            AccessKind::Open(AccessMode::Write),
            AccessKind::Close(AccessMode::Any),
            AccessKind::Close(AccessMode::Execute),
            AccessKind::Close(AccessMode::Read),
            AccessKind::Close(AccessMode::Other),
        ] {
            let event = Ok(Event::new(EventKind::Access(kind)).add_path(path.clone()));
            assert!(matches!(classify(&targets, &event), Step::Ignore));
        }
        let event =
            Ok(Event::new(EventKind::Access(AccessKind::Close(AccessMode::Write))).add_path(path));
        assert!(matches!(classify(&targets, &event), Step::Settle));
    }

    #[test]
    fn resolve_targets_the_on_disk_spelling_of_a_differently_cased_name() {
        let (dir, path) = scratch();
        let upper = dir.path().join("HOFUND.TOML");
        let targets = Targets::resolve(&upper).expect("targets");
        assert!(targets.matches(&path), "the on-disk name is watched");
        // The given spelling is matched only when no entry has it.
        let missing = Targets::resolve(&dir.path().join("Other.toml")).expect("targets");
        assert!(missing.matches(&dir.path().join("Other.toml")));
    }

    #[test]
    fn on_disk_name_prefers_exact_then_ascii_case_and_keeps_unmatched() {
        let entries = |names: &[&str]| {
            names
                .iter()
                .map(|name| OsString::from(*name))
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert_eq!(
            on_disk_name(
                std::ffi::OsStr::new("hofund.toml"),
                entries(&["HOFUND.TOML", "hofund.toml"]),
            ),
            "hofund.toml"
        );
        assert_eq!(
            on_disk_name(
                std::ffi::OsStr::new("HOFUND.TOML"),
                entries(&["other.toml", "hofund.toml"]),
            ),
            "hofund.toml"
        );
        // Two case variants with no exact match: the first listed wins, so
        // the choice does not drift as later entries are read.
        assert_eq!(
            on_disk_name(
                std::ffi::OsStr::new("Hofund.toml"),
                entries(&["HOFUND.TOML", "hofund.toml"]),
            ),
            "HOFUND.TOML"
        );
        assert_eq!(
            on_disk_name(
                std::ffi::OsStr::new("missing.toml"),
                entries(&["hofund.toml"]),
            ),
            "missing.toml"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn debounce_is_trailing_edge_and_ignores_siblings() {
        let (dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let (count, check) = counter();
        let (sender, events) = mpsc::unbounded_channel();
        let task = {
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        };
        for _ in 0..4 {
            sender.send(modified(&path)).expect("send");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(count.load(Ordering::SeqCst), 0, "fired before quiet");
        }
        sender
            .send(modified(&dir.path().join("hofund.usage.jsonl")))
            .expect("send");
        tokio::time::sleep(Duration::from_millis(99)).await;
        assert_eq!(count.load(Ordering::SeqCst), 0, "a sibling reset or fired");
        tokio::time::sleep(Duration::from_millis(2)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        task.abort();
    }

    /// Virtual bound slightly past one quiet period. Paused time makes the
    /// debounce immediate, and the extra millisecond fails the test instead
    /// of hanging when a check never arrives.
    fn quiet_bound() -> Duration {
        DEBOUNCE + Duration::from_millis(1)
    }

    #[tokio::test(start_paused = true)]
    async fn change_during_check_is_rechecked_without_another_event() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let mtime = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .expect("mtime");
        let calls = Arc::new(AtomicUsize::new(0));
        let (checked, mut checks) = mpsc::unbounded_channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let gate = Arc::new(Mutex::new(Some((started_tx, release_rx))));
        let check: Check = {
            let calls = Arc::clone(&calls);
            let gate = Arc::clone(&gate);
            Arc::new(move || {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                checked.send(call).expect("check record");
                let gate = gate.lock().expect("check gate").take();
                Box::pin(async move {
                    if let Some((started, release)) = gate {
                        started.send(()).expect("check started");
                        release.await.expect("release check");
                    }
                    CheckOutcome::Applied
                })
            })
        };
        let (sender, events) = mpsc::unbounded_channel();
        let task = AbortOnDrop({
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        });

        sender.send(modified(&path)).expect("modify");
        assert_eq!(
            tokio::time::timeout(quiet_bound(), checks.recv())
                .await
                .expect("first check timed out"),
            Some(0)
        );
        started_rx.await.expect("first check dropped");

        fs::write(&path, "listen = \"127.0.0.1:1\"\n").expect("second write");
        fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(mtime))
            .expect("restore mtime");
        release_tx.send(()).expect("release first check");

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(1), checks.recv())
                .await
                .expect("second write was not rechecked"),
            Some(1)
        );
        drop(task);
    }

    #[tokio::test(start_paused = true)]
    async fn late_duplicate_events_do_not_repeat_an_unchanged_rejection() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let (checked, mut checks) = mpsc::unbounded_channel();
        let check: Check = Arc::new(move || {
            let checked = checked.clone();
            Box::pin(async move {
                checked.send(()).expect("check record");
                CheckOutcome::Unapplied
            })
        });
        let (sender, events) = mpsc::unbounded_channel();
        let task = AbortOnDrop({
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        });

        sender.send(modified(&path)).expect("modify");
        tokio::time::timeout(quiet_bound(), checks.recv())
            .await
            .expect("rejection was not observed")
            .expect("rejection check dropped");
        tokio::time::advance(quiet_bound()).await;
        tokio::task::yield_now().await;
        assert!(checks.try_recv().is_err());

        sender.send(modified(&path)).expect("late duplicate");
        tokio::task::yield_now().await;
        tokio::time::advance(quiet_bound()).await;
        tokio::task::yield_now().await;
        assert!(
            checks.try_recv().is_err(),
            "a late duplicate repeated the unchanged rejection"
        );

        sender
            .send(Err(notify::Error::generic("late overflow notice")))
            .expect("forced duplicate");
        tokio::task::yield_now().await;
        tokio::time::advance(quiet_bound()).await;
        tokio::task::yield_now().await;
        assert!(
            checks.try_recv().is_err(),
            "a forced event repeated the unchanged rejection"
        );
        drop(task);
    }

    /// The control-watch integration order: the check that publishes
    /// `config_rejected` has already returned, the recovery arm has expired
    /// unchanged more than once, an ignored event has arrived, and only then
    /// does the correcting write land without a matching notify event.
    #[tokio::test(start_paused = true)]
    async fn correcting_write_after_rejection_is_rechecked_without_another_event() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let calls = Arc::new(AtomicUsize::new(0));
        let (checked, mut checks) = mpsc::unbounded_channel();
        let check: Check = {
            let calls = Arc::clone(&calls);
            Arc::new(move || {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let checked = checked.clone();
                Box::pin(async move {
                    // The send is the end of the check, where `config_rejected`
                    // is published. `follow` samples the stamp and arms the
                    // quiet period before this receive resumes.
                    checked.send(call).expect("check record");
                    if call == 0 {
                        CheckOutcome::Unapplied
                    } else {
                        CheckOutcome::Applied
                    }
                })
            })
        };
        let (sender, events) = mpsc::unbounded_channel();
        let task = AbortOnDrop({
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        });

        sender.send(modified(&path)).expect("modify");
        assert_eq!(
            tokio::time::timeout(quiet_bound(), checks.recv())
                .await
                .expect("rejection was not observed"),
            Some(0)
        );
        tokio::time::advance(quiet_bound()).await;
        tokio::task::yield_now().await;
        assert!(
            checks.try_recv().is_err(),
            "an unchanged rejection was checked again"
        );

        sender
            .send(Ok(
                Event::new(EventKind::Access(AccessKind::Any)).add_path(path.clone())
            ))
            .expect("ignored event");
        tokio::task::yield_now().await;
        tokio::time::advance(quiet_bound()).await;
        tokio::task::yield_now().await;
        assert!(
            checks.try_recv().is_err(),
            "an unchanged rejection was checked after an ignored event"
        );

        fs::write(&path, "listen = \"127.0.0.1:1\"\n").expect("correcting write");
        assert_eq!(
            tokio::time::timeout(quiet_bound(), checks.recv())
                .await
                .expect("correcting write was not rechecked without another event"),
            Some(1)
        );
        tokio::time::advance(quiet_bound()).await;
        tokio::task::yield_now().await;
        assert!(
            checks.try_recv().is_err(),
            "a stable applied file kept being checked"
        );
        drop(task);
    }

    /// A stamp that changes on every pass must not keep `look` from returning.
    /// The write that arrives during the one recheck is picked up on the
    /// quiet period, and a further notify event is still debounced.
    #[tokio::test(start_paused = true)]
    async fn recheck_returns_to_follow_while_writes_continue() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let calls = Arc::new(AtomicUsize::new(0));
        let releases = Arc::new(Mutex::new(VecDeque::new()));
        let (started_tx, mut started) = mpsc::unbounded_channel();
        let check: Check = {
            let calls = Arc::clone(&calls);
            let releases = Arc::clone(&releases);
            Arc::new(move || {
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                releases.lock().expect("releases").push_back(release_tx);
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let started_tx = started_tx.clone();
                Box::pin(async move {
                    started_tx.send(call).expect("started");
                    release_rx.await.expect("release check");
                    CheckOutcome::Applied
                })
            })
        };
        let (sender, events) = mpsc::unbounded_channel();
        let task = AbortOnDrop({
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        });
        let release_next = |releases: &Mutex<VecDeque<tokio::sync::oneshot::Sender<()>>>| {
            let release = releases
                .lock()
                .expect("releases")
                .pop_front()
                .expect("queued release");
            release.send(()).expect("release check");
        };

        sender.send(modified(&path)).expect("modify");
        assert_eq!(
            tokio::time::timeout(quiet_bound(), started.recv())
                .await
                .expect("in-flight check"),
            Some(0)
        );
        fs::write(&path, "listen = \"127.0.0.1:1\"\n").expect("write during check");
        release_next(&releases);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(1), started.recv())
                .await
                .expect("the one disk recheck did not run"),
            Some(1)
        );
        fs::write(&path, "listen = \"127.0.0.1:2\"\n").expect("write during recheck");
        release_next(&releases);
        tokio::task::yield_now().await;
        assert!(
            started.try_recv().is_err(),
            "look kept rechecking while writes were still arriving"
        );
        assert_eq!(
            tokio::time::timeout(quiet_bound(), started.recv())
                .await
                .expect("the write during the recheck was dropped"),
            Some(2)
        );

        // Returning to `follow` must keep servicing watcher events.
        fs::write(&path, "listen = \"127.0.0.1:3\"\n").expect("later write");
        sender.send(modified(&path)).expect("later modify");
        release_next(&releases);
        assert_eq!(
            tokio::time::timeout(quiet_bound(), started.recv())
                .await
                .expect("a later event was not debounced"),
            Some(3)
        );
        drop(task);
    }

    #[tokio::test]
    async fn watcher_debounces_parent_directory_rename_and_partial_save() {
        let (_dir, path) = scratch();
        let (count, check) = counter();
        let task = spawn_with(path.clone(), fast(), check);
        tokio::time::sleep(Duration::from_millis(200)).await;
        // A partial save, then the editor's rename of the finished file.
        fs::write(&path, "listen = ").expect("partial");
        tokio::time::sleep(Duration::from_millis(50)).await;
        save_by_rename(&path, "listen = \"127.0.0.1:1\"\n");
        assert!(reaches(&count, 1, Duration::from_secs(2)).await, "no check");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "partial save checked separately"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_ignores_sibling_usage_and_token_writes() {
        let (dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let (checked, mut checks) = mpsc::unbounded_channel();
        let check: Check = Arc::new(move || {
            let checked = checked.clone();
            Box::pin(async move {
                checked.send(()).expect("check record");
                CheckOutcome::Applied
            })
        });
        let (sender, events) = mpsc::unbounded_channel();
        let task = AbortOnDrop({
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        });

        sender.send(modified(&path)).expect("baseline modify");
        tokio::time::timeout(quiet_bound(), checks.recv())
            .await
            .expect("baseline check timed out")
            .expect("baseline check dropped");
        write_siblings(dir.path());
        for entry in fs::read_dir(dir.path()).expect("siblings") {
            let sibling = entry.expect("sibling").path();
            if sibling != path {
                sender.send(modified(&sibling)).expect("sibling event");
            }
        }
        for nested in [".hofund/daemon.json", "sub/hofund.toml"] {
            sender
                .send(modified(&dir.path().join(nested)))
                .expect("nested sibling event");
        }
        tokio::time::advance(quiet_bound()).await;
        tokio::task::yield_now().await;
        assert!(
            checks.try_recv().is_err(),
            "a sibling write triggered a check"
        );

        fs::write(&path, "listen = \"127.0.0.1:2\"\n").expect("edit");
        sender.send(modified(&path)).expect("config event");
        tokio::time::timeout(quiet_bound(), checks.recv())
            .await
            .expect("edit missed")
            .expect("edit check dropped");
        drop(task);
    }

    /// The backend-level half of the sibling test: no event the real watcher
    /// reports for sibling writes leads to a check. Events that arrive
    /// before the writes, such as a late report of the file's own creation on
    /// macOS, are drained first (#281).
    #[tokio::test]
    async fn sibling_writes_report_no_configuration_path() {
        let (dir, path) = scratch();
        let (_watcher, targets, mut events) = setup(&path).expect("watcher");
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if events.try_recv().is_err() {
                break;
            }
            while events.try_recv().is_ok() {}
        }
        write_siblings(dir.path());
        tokio::time::sleep(Duration::from_millis(900)).await;
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            seen.push(event);
        }
        let acted = seen
            .iter()
            .filter(|event| !matches!(classify(&targets, event), Step::Ignore))
            .count();
        assert_eq!(acted, 0, "events: {seen:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_follows_symlink_target_dir() {
        let link_dir = tempfile::tempdir().expect("link dir");
        let target_dir = tempfile::tempdir().expect("target dir");
        let target = target_dir.path().join("real.toml");
        fs::write(&target, "listen = \"127.0.0.1:0\"\n").expect("target");
        let path = link_dir.path().join("hofund.toml");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        let (count, check) = counter();
        let task = spawn_with(path.clone(), fast(), check);
        tokio::time::sleep(Duration::from_millis(200)).await;
        save_by_rename(&target, "listen = \"127.0.0.1:3\"\n");
        assert!(
            reaches(&count, 1, Duration::from_secs(2)).await,
            "target edit missed"
        );
        // Retarget the link to a third directory and edit there.
        let moved_dir = tempfile::tempdir().expect("moved dir");
        let moved = moved_dir.path().join("moved.toml");
        fs::write(&moved, "listen = \"127.0.0.1:0\"\n").expect("moved");
        let staged = link_dir.path().join("hofund.toml.new");
        std::os::unix::fs::symlink(&moved, &staged).expect("staged link");
        fs::rename(&staged, &path).expect("swap link");
        assert!(
            reaches(&count, 2, Duration::from_secs(2)).await,
            "link swap missed"
        );
        // The new target's directory is watched right after that check.
        tokio::time::sleep(Duration::from_millis(150)).await;
        fs::write(&moved, "listen = \"127.0.0.1:4\"\n").expect("edit moved");
        assert!(
            reaches(&count, 3, Duration::from_secs(2)).await,
            "new target edit missed"
        );
        task.abort();
    }

    #[tokio::test]
    async fn watcher_setup_or_termination_uses_two_second_stat_polling() {
        assert_eq!(POLL, Duration::from_secs(2));
        // Setup fails: the directory does not exist yet.
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("later").join("hofund.toml");
        let (count, check) = counter();
        let task = spawn_with(path.clone(), fast(), check);
        tokio::time::sleep(Duration::from_millis(150)).await;
        fs::create_dir(root.path().join("later")).expect("dir");
        fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("appears");
        assert!(
            reaches(&count, 1, Duration::from_secs(1)).await,
            "poll missed creation"
        );
        // Same length and a coarse clock: only the revision differs.
        let modified = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .expect("mtime");
        fs::write(&path, "listen = \"127.0.0.1:9\"\n").expect("same length");
        fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(modified))
            .expect("restore mtime");
        assert!(
            reaches(&count, 2, Duration::from_secs(1)).await,
            "revision change missed"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "unchanged file checked again"
        );
        task.abort();

        // The event stream ends: one catch-up check, then polling.
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let (count, check) = counter();
        let (sender, events) = mpsc::unbounded_channel::<notify::Result<Event>>();
        let task = {
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        };
        drop(sender);
        assert!(
            reaches(&count, 1, Duration::from_secs(1)).await,
            "no catch-up check"
        );
        fs::write(&path, "listen = \"127.0.0.1:5\"\n").expect("edit");
        assert!(
            reaches(&count, 2, Duration::from_secs(1)).await,
            "poll missed edit"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn correction_after_ended_stream_catch_up_read_is_retried() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, mut started) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(Mutex::new(Some(release_rx)));
        let check: Check = {
            let calls = Arc::clone(&calls);
            let check_path = path.clone();
            Arc::new(move || {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let path = check_path.clone();
                let started_tx = started_tx.clone();
                let release = if call == 0 {
                    Some(
                        release
                            .lock()
                            .expect("release gate")
                            .take()
                            .expect("first release"),
                    )
                } else {
                    None
                };
                Box::pin(async move {
                    let text = fs::read_to_string(path).expect("catch-up read");
                    if call == 0 {
                        assert_eq!(text, "listen = \"127.0.0.1:0\"\n");
                    } else {
                        assert_eq!(text, "listen = \"127.0.0.1:1\"\n");
                    }
                    started_tx.send(call).expect("check started");
                    if let Some(release) = release {
                        release.await.expect("release catch-up check");
                    }
                    if call == 0 {
                        CheckOutcome::Unapplied
                    } else {
                        CheckOutcome::Applied
                    }
                })
            })
        };
        let (sender, events) = mpsc::unbounded_channel::<notify::Result<Event>>();
        let task = AbortOnDrop({
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        });

        drop(sender);
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(1), started.recv())
                .await
                .expect("catch-up check timed out"),
            Some(0)
        );
        fs::write(&path, "listen = \"127.0.0.1:1\"\n").expect("correcting write");
        release_tx.send(()).expect("release catch-up check");
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(1), started.recv())
                .await
                .expect("correction was absorbed by the poll baseline"),
            Some(1)
        );
        drop(task);
    }

    #[tokio::test(start_paused = true)]
    async fn watcher_rescan_event_forces_stat_check() {
        let (_dir, path) = scratch();
        let targets = Targets::resolve(&path).expect("targets");
        let (count, check) = counter();
        let (sender, events) = mpsc::unbounded_channel();
        let task = {
            let path = path.clone();
            tokio::spawn(async move { follow(&path, fast(), &check, targets, events, None).await })
        };
        // A rescan notice names no path and is not debounced.
        sender
            .send(Ok(Event::new(EventKind::Any).set_flag(Flag::Rescan)))
            .expect("rescan");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // So is an event kind the backend could not classify.
        fs::write(&path, "listen = \"127.0.0.1:1\"\n").expect("write before other");
        sender
            .send(Ok(Event::new(EventKind::Other)))
            .expect("other");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // A watcher error (queue overflow) also forces a changed-file check and
        // cancels a pending debounce rather than checking twice.
        fs::write(&path, "listen = \"127.0.0.1:2\"\n").expect("write before error");
        sender.send(modified(&path)).expect("modify");
        sender
            .send(Err(notify::Error::generic("queue overflow")))
            .expect("error");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(count.load(Ordering::SeqCst), 3);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(count.load(Ordering::SeqCst), 3);
        task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_matches_canonicalized_macos_private_var_paths() {
        // On macOS the temporary directory lives under `/var`, a symlink to
        // `/private/var`, and FSEvents reports the canonical path. A
        // symlinked parent reproduces that on every Unix.
        let root = tempfile::tempdir().expect("root");
        let real = root.path().join("private");
        fs::create_dir(&real).expect("real dir");
        let alias = root.path().join("var");
        std::os::unix::fs::symlink(&real, &alias).expect("alias");
        let path = alias.join("hofund.toml");
        // Mid-save the file can be missing; the parent is still canonical.
        let missing = Targets::resolve(&path).expect("targets without the file");
        assert!(missing.matches(&real.join("hofund.toml")));
        fs::write(&path, "listen = \"127.0.0.1:0\"\n").expect("config");
        let targets = Targets::resolve(&path).expect("targets");
        assert!(targets.matches(&real.join("hofund.toml")));
        assert!(targets.matches(&path));
        assert!(!targets.matches(&real.join("hofund.token")));
        let (count, check) = counter();
        let task = spawn_with(path.clone(), fast(), check);
        tokio::time::sleep(Duration::from_millis(200)).await;
        fs::write(&path, "listen = \"127.0.0.1:6\"\n").expect("edit");
        assert!(
            reaches(&count, 1, Duration::from_secs(2)).await,
            "canonical event missed"
        );
        task.abort();
    }
}
