//! Path containment and no-follow opens for plugin-provided session files.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// A session path failed a containment or file-type check.
#[derive(Debug, Error)]
pub enum OpenError {
    /// The lexical or resolved path leaves the plugin root.
    #[error("session path escapes its plugin root")]
    Escaped,
    /// The final target is not a regular file.
    #[error("session path is not a regular file")]
    NotRegular,
    /// The file system rejected the read.
    #[error("cannot open session file: {0}")]
    Io(#[from] io::Error),
}

/// Whether `path` resolves to a target inside the resolved plugin `root`.
///
/// Relative paths are joined to `root`. Parent-directory components and an
/// absolute path outside the lexical root are rejected before canonicalization.
#[must_use]
pub fn within_root(root: &Path, path: &Path) -> bool {
    resolve_within_root(root, path).is_ok()
}

/// Opens a regular session file without following a final symlink.
///
/// The whole existing path is canonicalized first and must remain within the
/// canonical plugin root. The no-follow open and a second metadata check guard
/// the final component at open time.
///
/// # Errors
///
/// Returns [`OpenError`] when the path escapes, is not regular, or cannot be
/// opened.
pub fn open_session_file(root: &Path, path: &Path) -> Result<File, OpenError> {
    let (resolved_root, target, lexical) = resolve_within_root(root, path)?;
    if !target.is_file() {
        return Err(OpenError::NotRegular);
    }
    debug_assert!(target.starts_with(&resolved_root));
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(&lexical)?;
    if !file.metadata()?.is_file() {
        return Err(OpenError::NotRegular);
    }
    Ok(file)
}

fn resolve_within_root(root: &Path, path: &Path) -> Result<(PathBuf, PathBuf, PathBuf), OpenError> {
    let root = absolute(root)?;
    if path.components().any(|component| {
        matches!(component, Component::ParentDir)
            || (!path.is_absolute()
                && matches!(component, Component::RootDir | Component::Prefix(_)))
    }) {
        return Err(OpenError::Escaped);
    }
    let lexical = if path.is_absolute() {
        if !path.starts_with(&root) {
            return Err(OpenError::Escaped);
        }
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let resolved_root = root.canonicalize()?;
    if !resolved_root.is_dir() {
        return Err(OpenError::NotRegular);
    }
    let target = lexical.canonicalize()?;
    if !target.starts_with(&resolved_root) {
        return Err(OpenError::Escaped);
    }
    Ok((resolved_root, target, lexical))
}

fn absolute(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::time::Duration;

    use super::*;

    #[test]
    fn open_accepts_a_regular_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("session.jsonl");
        std::fs::write(&path, b"hello\n").expect("write");
        let mut text = String::new();
        open_session_file(temp.path(), &path)
            .expect("open regular file")
            .read_to_string(&mut text)
            .expect("read");
        assert_eq!(text, "hello\n");
    }

    #[test]
    fn open_rejects_dotdot() {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("root");
        std::fs::create_dir(&root).expect("root");
        std::fs::write(temp.path().join("outside"), b"outside").expect("outside");
        assert!(matches!(
            open_session_file(&root, Path::new("../outside")),
            Err(OpenError::Escaped)
        ));
        assert!(matches!(
            open_session_file(&root, &root.join("nested").join("..").join("outside")),
            Err(OpenError::Escaped)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_symlink_out() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let outside = temp.path().join("outside");
        std::fs::write(&outside, b"outside").expect("outside");
        symlink(&outside, root.join("linked")).expect("symlink");
        assert!(!within_root(&root, &root.join("linked")));
        assert!(matches!(
            open_session_file(&root, Path::new("linked")),
            Err(OpenError::Escaped)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_accepts_root_junction() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temp dir");
        let real = temp.path().join("real");
        std::fs::create_dir(&real).expect("real");
        std::fs::write(real.join("session.jsonl"), b"fixture\n").expect("session");
        let linked = temp.path().join("linked");
        symlink(&real, &linked).expect("root link");
        let lexical = linked.join("session.jsonl");
        assert!(within_root(&linked, &lexical));
        let mut text = String::new();
        open_session_file(&linked, &lexical)
            .expect("open through root link")
            .read_to_string(&mut text)
            .expect("read");
        assert_eq!(text, "fixture\n");
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_fifo() {
        let temp = tempfile::tempdir().expect("temp dir");
        let fifo = temp.path().join("session.jsonl");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        // Bound the open: if the `is_file` guard is inverted, `open` of a
        // FIFO blocks. Fail in 2s instead of hanging the package suite.
        let root = temp.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(open_session_file(&root, &fifo));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("fifo open should not block; the is_file guard must reject it first");
        assert!(matches!(result, Err(OpenError::NotRegular)), "{result:?}");
    }

    #[test]
    fn open_refuses_final_symlink_even_inside_root() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let temp = tempfile::tempdir().expect("temp dir");
            std::fs::write(temp.path().join("target"), b"target").expect("target");
            symlink(temp.path().join("target"), temp.path().join("link")).expect("link");
            assert!(open_session_file(temp.path(), &temp.path().join("link")).is_err());
        }
    }
}
