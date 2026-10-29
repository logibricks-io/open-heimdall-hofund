//! Sidecar locking and atomic, byte-exact writes of the configuration file.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// An exclusive advisory lock that serializes every writer of one
/// configuration file: the daemon and one-shot edits alike.
#[derive(Debug)]
pub(crate) struct SidecarLock {
    _file: File,
    path: PathBuf,
}

impl SidecarLock {
    /// Blocks until the lock is held. `primary` sits beside the resolved
    /// configuration file; `fallback` lives in the runtime directory and is
    /// used when the configuration directory is read-only.
    pub(crate) fn acquire(primary: &Path, fallback: Option<&Path>) -> io::Result<Self> {
        match lock_at(primary) {
            Ok(lock) => Ok(lock),
            Err(error) if fallback.is_some() && cannot_create(&error) => {
                lock_at(fallback.expect("fallback checked"))
            }
            Err(error) => Err(error),
        }
    }

    /// The lock file actually held.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

fn cannot_create(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied
            | io::ErrorKind::ReadOnlyFilesystem
            | io::ErrorKind::NotFound
    )
}

fn lock_at(path: &Path) -> io::Result<SidecarLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.lock()?;
    Ok(SidecarLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

/// The primary sidecar lock path for `config`: `<resolved>.lock`.
pub(crate) fn primary_lock_path(config: &Path) -> PathBuf {
    let resolved = resolve(config);
    let mut name = resolved.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    resolved.with_file_name(name)
}

/// Follows symlinks so a write replaces the link's target, not the link.
pub(crate) fn resolve(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A failed write that names the directory it was attempted in.
#[derive(Debug)]
pub(crate) struct WriteFailure {
    pub(crate) directory: PathBuf,
    pub(crate) error: io::Error,
}

/// Replaces the configuration file with exactly `bytes`.
///
/// The temporary file is created in the target's own directory so the rename
/// is atomic. It copies the target's mode and, best-effort, its owner, then is
/// fsynced, renamed over the target, and the directory is fsynced. On failure
/// the old file is untouched.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), WriteFailure> {
    let target = resolve(path);
    let directory = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let fail = |error| WriteFailure {
        directory: directory.clone(),
        error,
    };
    let metadata = fs::metadata(&target).ok();
    let (temporary, mut file) = create_temporary(&directory, &target).map_err(fail)?;
    let written = (|| {
        if let Some(metadata) = &metadata {
            file.set_permissions(metadata.permissions())?;
            preserve_owner(&temporary, metadata);
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &target)?;
        sync_directory(&directory)
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(fail(error));
    }
    Ok(())
}

/// Why a locked one-shot write did not replace the file.
#[derive(Debug)]
#[allow(
    dead_code,
    reason = "the one-shot hofundd (L8) is the first caller; until then the fields are read through Debug"
)]
pub(crate) enum LockedWriteError {
    /// The sidecar lock could not be taken.
    Lock(io::Error),
    /// The file could not be read for the revision check.
    Read(io::Error),
    /// The bytes on disk no longer have the expected revision.
    Stale { disk_revision: String },
    /// The atomic write failed; the old file is intact.
    Write(WriteFailure),
}

/// The write a one-shot `hofundd` config operation performs: take the
/// sidecar lock, recheck the expected revision of the bytes on disk, and
/// write atomically, all while the lock is held so no other writer (the
/// daemon or another one-shot) can interleave. Returns the new revision.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn locked_write(
    path: &Path,
    fallback: Option<&Path>,
    expected_revision: &str,
    bytes: &[u8],
) -> Result<String, LockedWriteError> {
    let _lock =
        SidecarLock::acquire(&primary_lock_path(path), fallback).map_err(LockedWriteError::Lock)?;
    let disk = fs::read(path).map_err(LockedWriteError::Read)?;
    let disk_revision = crate::control::revision(&disk);
    if disk_revision != expected_revision {
        return Err(LockedWriteError::Stale { disk_revision });
    }
    atomic_write(path, bytes).map_err(LockedWriteError::Write)?;
    Ok(crate::control::revision(bytes))
}

fn create_temporary(directory: &Path, target: &Path) -> io::Result<(PathBuf, File)> {
    let stem = target.file_name().unwrap_or_default().to_string_lossy();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let mut attempt = 0_u32;
    loop {
        let candidate = directory.join(format!(
            ".{stem}.{}.{nanos}.{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempt < 16 => {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn preserve_owner(temporary: &Path, metadata: &fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    // Changing the owner needs privilege. Without it the file keeps the
    // caller's owner, which in practice is the same user.
    if let Err(error) =
        std::os::unix::fs::chown(temporary, Some(metadata.uid()), Some(metadata.gid()))
    {
        tracing::debug!(%error, "configuration owner not preserved");
    }
}

#[cfg(not(unix))]
fn preserve_owner(_temporary: &Path, _metadata: &fs::Metadata) {}

/// Directory flushes performed, so a test can see the rename was made durable.
#[cfg(test)]
static DIRECTORY_SYNCS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(unix)]
fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()?;
    #[cfg(test)]
    DIRECTORY_SYNCS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> io::Result<()> {
    // Windows cannot open a directory handle for flushing through std; the
    // rename is already durable once `MoveFileEx` returns.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::revision;

    const HOLD_LOCK: &str = "HOFUND_TEST_HOLD_CONFIG_LOCK";

    /// Re-exec helper: run in a child process, it holds the sidecar lock for
    /// the config named in `HOLD_LOCK` until `<config>.release` appears.
    #[test]
    #[ignore = "child process helper for one_shot_config_write_uses_sidecar_lock"]
    fn sidecar_lock_holder_child() {
        let Some(config) = std::env::var_os(HOLD_LOCK).map(PathBuf::from) else {
            return;
        };
        let _lock = SidecarLock::acquire(&primary_lock_path(&config), None).expect("child lock");
        fs::write(config.with_extension("held"), b"").expect("ready marker");
        let release = config.with_extension("release");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !release.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn wait_for(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline, "{}", path.display());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn one_shot_config_write_uses_sidecar_lock() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "original\n").expect("seed");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "config_write::tests::sidecar_lock_holder_child",
                "--include-ignored",
                "--test-threads=1",
            ])
            .env(HOLD_LOCK, &path)
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("lock holder");
        wait_for(&path.with_extension("held"));

        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                locked_write(&path, None, &revision(b"original\n"), b"one-shot\n")
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!writer.is_finished(), "the write must wait for the lock");
        assert_eq!(fs::read(&path).expect("read"), b"original\n");
        // The holder edits the file before it releases the lock.
        fs::write(&path, "hand edit\n").expect("hand edit");
        fs::write(path.with_extension("release"), b"").expect("release");
        let outcome = writer.join().expect("writer");
        assert!(
            matches!(&outcome, Err(LockedWriteError::Stale { disk_revision }) if *disk_revision == revision(b"hand edit\n")),
            "{outcome:?}"
        );
        assert_eq!(fs::read(&path).expect("read"), b"hand edit\n");
        assert!(child.wait().expect("child exit").success());

        let written = locked_write(&path, None, &revision(b"hand edit\n"), b"one-shot\n")
            .expect("write after release");
        assert_eq!(written, revision(b"one-shot\n"));
        assert_eq!(fs::read(&path).expect("read"), b"one-shot\n");
    }

    #[test]
    fn concurrent_one_shot_edits_serialize_on_sidecar_lock() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "").expect("seed");
        let writers: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut stale = 0_u32;
                    loop {
                        let current = fs::read(&path).expect("read");
                        let mut next = current.clone();
                        next.extend_from_slice(format!("edit {index}\n").as_bytes());
                        match locked_write(&path, None, &revision(&current), &next) {
                            Ok(_) => return stale,
                            Err(LockedWriteError::Stale { .. }) => stale += 1,
                            Err(error) => panic!("{error:?}"),
                        }
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        let text = fs::read_to_string(&path).expect("read");
        let mut lines: Vec<_> = text.lines().collect();
        lines.sort_unstable();
        let expected: Vec<String> = (0..8).map(|index| format!("edit {index}")).collect();
        assert_eq!(lines, expected, "an edit was lost: {text}");
    }

    #[test]
    fn atomic_write_replaces_bytes_exactly_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "old\n").expect("seed");
        let bytes = b"listen = \"127.0.0.1:0\"\r\n# no trailing newline";
        atomic_write(&path, bytes).expect("write");
        assert_eq!(fs::read(&path).expect("read"), bytes);
        let names: Vec<_> = fs::read_dir(dir.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["hofund.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_copies_mode_and_flushes_the_directory() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "old\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        let before = DIRECTORY_SYNCS.load(Ordering::SeqCst);
        atomic_write(&path, b"new\n").expect("write");
        assert!(DIRECTORY_SYNCS.load(Ordering::SeqCst) > before);
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn sidecar_lock_falls_back_when_the_primary_cannot_be_created() {
        let dir = tempfile::tempdir().expect("dir");
        let primary = dir.path().join("missing").join("hofund.toml.lock");
        let fallback = dir.path().join("config-test.lock");
        let lock = SidecarLock::acquire(&primary, Some(&fallback)).expect("fallback");
        assert_eq!(lock.path(), fallback);
        SidecarLock::acquire(&primary, None).expect_err("no fallback");
    }

    #[test]
    fn primary_lock_sits_beside_the_resolved_file() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("hofund.toml");
        fs::write(&path, "").expect("seed");
        assert_eq!(
            primary_lock_path(&path),
            resolve(dir.path()).join("hofund.toml.lock")
        );
    }

    #[test]
    fn sidecar_lock_serializes_holders() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("hofund.toml.lock");
        let held = SidecarLock::acquire(&path, None).expect("lock");
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let _lock = SidecarLock::acquire(&path, None).expect("second lock");
                started.elapsed()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(held);
        let waited = waiter.join().expect("waiter");
        assert!(
            waited >= std::time::Duration::from_millis(100),
            "{waited:?}"
        );
    }
}
