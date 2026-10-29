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
