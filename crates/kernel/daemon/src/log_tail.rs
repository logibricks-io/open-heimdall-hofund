use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use hofund_control_proto::{ControlError, ControlErrorKind, LogTailQuery, LogTailResponse};
use hofund_gateway::control::ControlFailure;

use crate::control_local::ConfigIdentity;

pub(crate) fn read(
    config_path: &Path,
    query: LogTailQuery,
) -> Result<LogTailResponse, ControlFailure> {
    let path = log_path(config_path)?;
    read_path(&path, query)
}

fn log_path(config_path: &Path) -> Result<PathBuf, ControlFailure> {
    let identity = ConfigIdentity::resolve(config_path)
        .map_err(|error| ControlError::new(ControlErrorKind::Internal, error, "logs"))?;
    Ok(identity
        .resolved_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".hofund")
        .join(format!("daemon-{}.log", identity.id)))
}

fn read_path(path: &Path, query: LogTailQuery) -> Result<LogTailResponse, ControlFailure> {
    let mut response = LogTailResponse::default();
    response.path = path.display().to_string();
    let mut file = match open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(response),
        Err(error) => return Err(read_failure(path, error)),
    };
    let metadata = file.metadata().map_err(|error| read_failure(path, error))?;
    if !metadata.is_file() {
        return Err(read_failure(path, not_regular_file()));
    }
    let file_len = metadata.len();
    let offset = query.after.min(file_len);
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| read_failure(path, error))?;
    let mut bytes = Vec::with_capacity(query.max_bytes);
    file.take(query.max_bytes as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| read_failure(path, error))?;
    let next_offset = offset.saturating_add(bytes.len() as u64);
    let text = String::from_utf8_lossy(&bytes);
    response.offset = offset;
    response.next_offset = next_offset;
    response.lines = text.lines().map(str::to_owned).collect();
    Ok(response)
}

#[cfg(unix)]
fn open(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open(path: &Path) -> std::io::Result<File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(not_regular_file());
    }
    File::open(path)
}

fn not_regular_file() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "daemon log is not a regular file",
    )
}

fn read_failure(path: &Path, error: std::io::Error) -> ControlFailure {
    ControlError::new(
        ControlErrorKind::Internal,
        format!("cannot read daemon log {}: {error}", path.display()),
        "logs",
    )
    .into()
}

#[cfg(test)]
mod tests {
    use hofund_control_proto::LogTailQuery;

    fn query(after: u64, max_bytes: usize) -> LogTailQuery {
        let mut query = LogTailQuery::default();
        query.after = after;
        query.max_bytes = max_bytes;
        query
    }

    fn read(path: &std::path::Path, query: LogTailQuery) -> hofund_control_proto::LogTailResponse {
        super::read_path(path, query).unwrap_or_else(|failure| panic!("{}", failure.error.message))
    }

    #[test]
    fn missing_file_is_empty_and_offsets_resume_and_truncate() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("daemon.log");
        let missing = read(&path, LogTailQuery::default());
        assert_eq!(missing.offset, 0);
        assert_eq!(missing.next_offset, 0);
        assert!(missing.lines.is_empty());

        std::fs::write(&path, b"one\ntwo\n").expect("log");
        let first = read(&path, query(0, 4));
        assert_eq!(first.lines, ["one"]);
        assert_eq!(first.next_offset, 4);
        let second = read(&path, query(first.next_offset, 4));
        assert_eq!(second.offset, 4);
        assert_eq!(second.lines, ["two"]);
        assert_eq!(second.next_offset, 8);

        std::fs::write(&path, vec![b'x'; 70_000]).expect("large log");
        let truncated = read(&path, LogTailQuery::default());
        assert_eq!(truncated.next_offset, 65_536);
        let rest = read(&path, query(truncated.next_offset, 65_536));
        assert_eq!(rest.offset, 65_536);
        assert_eq!(rest.next_offset, 70_000);
    }

    #[test]
    fn a_directory_is_a_read_failure_not_an_empty_log() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(super::read_path(dir.path(), LogTailQuery::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_a_read_failure_without_exposing_its_target() {
        let dir = tempfile::tempdir().expect("dir");
        let target = dir.path().join("target.log");
        let contents = "distinctive-target-contents-never-return";
        std::fs::write(&target, contents).expect("target");
        let path = dir.path().join("daemon.log");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");

        let failure = super::read_path(&path, LogTailQuery::default()).expect_err("symlink");
        assert!(!failure.error.message.contains(contents));
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_a_read_failure() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("daemon.log");
        nix::unistd::mkfifo(
            &path,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("fifo");
        assert!(super::read_path(&path, LogTailQuery::default()).is_err());
    }

    #[test]
    fn a_non_not_found_open_error_is_a_read_failure() {
        let dir = tempfile::tempdir().expect("dir");
        // On Unix, File::open of a directory succeeds, so a parent-is-file path
        // produces ENOTDIR. On Windows that same path is often NotFound (empty
        // log), while opening a directory is PermissionDenied, not NotFound.
        let path = if cfg!(windows) {
            dir.path().to_path_buf()
        } else {
            let file = dir.path().join("not-a-directory");
            std::fs::write(&file, b"x").expect("file");
            file.join("daemon.log")
        };
        assert!(
            super::read_path(&path, LogTailQuery::default()).is_err(),
            "open of {} must not be treated as a missing log",
            path.display()
        );
    }
}
