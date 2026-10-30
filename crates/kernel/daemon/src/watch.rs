//! Follows hand edits to the configuration file (L6c).
//!
//! The parent directories of the configuration path and of its resolved
//! symlink target are watched, not the file: an editor that saves through a
//! temporary file and a rename replaces the file, and a watch on the old one
//! would go quiet. Events for any other name in those directories (the usage
//! ledger, the token, the sidecar `.lock`, editor swap files) are ignored.
//! Matching events are debounced on the trailing edge, so a partial save
//! followed by the final write reloads once. A rescan or overflow notice, or
//! a watcher error, forces an immediate check because events may be lost.
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

type CheckFuture = Pin<Box<dyn Future<Output = ()> + Send>>;
/// One look at the file on disk: a watcher-origin reload in the daemon.
pub(crate) type Check = Arc<dyn Fn() -> CheckFuture + Send + Sync>;
type Events = mpsc::UnboundedReceiver<notify::Result<Event>>;

/// Starts following `path` for the daemon's lifetime.
pub(crate) fn spawn(ops: Arc<ConfigOps>, path: PathBuf) -> JoinHandle<()> {
    let check: Check = Arc::new(move || {
        let ops = Arc::clone(&ops);
        Box::pin(async move {
            if let Err(failure) = ops.reload(None, ReloadOrigin::Watcher).await {
                tracing::warn!(
                    kind = ?failure.error.kind,
                    "configuration edit on disk was not applied; the last good configuration stays"
                );
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
        Ok(event) if event.paths.iter().any(|path| targets.matches(path)) => Step::Settle,
        Ok(_) => Step::Ignore,
    }
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
    let mut deadline: Option<Instant> = None;
    loop {
        let next = match deadline {
            Some(at) => tokio::select! {
                event = events.recv() => event,
                () = tokio::time::sleep_until(at) => {
                    deadline = None;
                    look(path, check, &mut targets, watcher.as_mut()).await;
                    continue;
                }
            },
            None => events.recv().await,
        };
        let Some(event) = next else { break };
        match classify(&targets, &event) {
            Step::Settle => deadline = Some(Instant::now() + timing.debounce),
            Step::Force => {
                deadline = None;
                look(path, check, &mut targets, watcher.as_mut()).await;
            }
            Step::Ignore => {}
        }
    }
    drop(watcher);
    tracing::warn!(
        path = %path.display(),
        "configuration watcher stopped; polling it every {} s",
        timing.poll.as_secs_f32()
    );
    // A change may have landed while the stream was ending.
    check().await;
    poll(path, timing.poll, check).await;
}

/// Checks the file, then follows a symlink that now points somewhere else.
async fn look(
    path: &Path,
    check: &Check,
    targets: &mut Targets,
    watcher: Option<&mut RecommendedWatcher>,
) {
    check().await;
    let Ok(current) = Targets::resolve(path) else {
        return;
    };
    if let Some(watcher) = watcher {
        for dir in current.dirs.difference(&targets.dirs) {
            if let Err(error) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                tracing::warn!(dir = %dir.display(), %error, "cannot watch the new configuration target");
            }
        }
    }
    *targets = current;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use notify::event::{Flag, ModifyKind};

    use super::*;

    fn counter() -> (Arc<AtomicUsize>, Check) {
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let check: Check = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
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

    /// Waits until `count` holds still for 500 ms and returns it. macOS
    /// FSEvents can deliver the config file's own creation late, so a test
    /// that asserts "no new check" compares against this baseline (#281).
    async fn settled(count: &AtomicUsize) -> usize {
        let mut last = count.load(Ordering::SeqCst);
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = count.load(Ordering::SeqCst);
            if now == last {
                return now;
            }
            last = now;
        }
        last
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

    #[tokio::test]
    async fn watcher_ignores_sibling_usage_and_token_writes() {
        let (dir, path) = scratch();
        let (count, check) = counter();
        let task = spawn_with(path.clone(), fast(), check);
        let baseline = settled(&count).await;
        write_siblings(dir.path());
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            baseline,
            "a sibling write triggered a check"
        );
        fs::write(&path, "listen = \"127.0.0.1:2\"\n").expect("edit");
        assert!(
            reaches(&count, baseline + 1, Duration::from_secs(2)).await,
            "edit missed"
        );
        task.abort();
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
        sender
            .send(Ok(Event::new(EventKind::Other)))
            .expect("other");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // A watcher error (queue overflow) also forces a check, and cancels a
        // pending debounce rather than checking twice.
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
