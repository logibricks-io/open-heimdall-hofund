//! Long-lived piped children.
//!
//! The child inherits the host environment after a caller-supplied deny-list
//! is applied, then the explicit pairs in the request. Standard output is
//! read as one chunk per line, capped at [`MAX_LINE`]. Standard error is
//! retained up to [`STDERR_CAP`] (the tail). Dropping the handle does not
//! kill the child and does not close standard input.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use hofund_plugin_api::kernel::LongLivedChildBackend;
use hofund_plugin_api::{BodyStream, PluginError, SpawnRequest};
use tokio::sync::mpsc;

use crate::accounts;

/// The longest standard-output line, newline excluded.
pub(crate) const MAX_LINE: usize = 64 * 1024 * 1024;

/// Retained standard-error tail.
pub(crate) const STDERR_CAP: usize = 1024 * 1024;

/// Lines buffered between the reader thread and the consumer.
const LINE_BUFFER: usize = 16;

type ExitSlot = Arc<Mutex<Option<Result<i32, String>>>>;

/// Whether `name` is listed in `deny`.
///
/// Comparison is case-insensitive on Windows and case-sensitive elsewhere.
pub(crate) fn is_denied(name: &OsStr, deny: &[OsString]) -> bool {
    deny.iter().any(|blocked| env_name_eq(name, blocked))
}

fn env_name_eq(left: &OsStr, right: &OsStr) -> bool {
    if left == right {
        return true;
    }
    if cfg!(windows) {
        match (left.to_str(), right.to_str()) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            _ => false,
        }
    } else {
        false
    }
}

/// Host environment after names in `deny` are removed.
pub(crate) fn inherited_env(deny: &[OsString]) -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| !is_denied(name, deny))
        .collect()
}

/// The command a long-lived spawn runs, before stdio is set.
pub(crate) fn command(request: &SpawnRequest, deny: &[OsString]) -> Command {
    let mut command = Command::new(request.program());
    command.args(request.args());
    command.current_dir(request.cwd());
    command.env_clear();
    command.envs(inherited_env(deny));
    command.envs(request.env().iter().cloned());
    command
}

fn write_exit_store(exit_store: &ExitSlot, code: Result<i32, String>) {
    let mut slot = match exit_store.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    *slot = Some(code);
}

fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

/// Appends `chunk` to `store`, keeping only the last [`STDERR_CAP`] bytes.
pub(crate) fn append_stderr_tail(store: &mut Vec<u8>, chunk: &[u8]) {
    store.extend_from_slice(chunk);
    if store.len() > STDERR_CAP {
        let excess = store.len() - STDERR_CAP;
        store.drain(..excess);
    }
}

/// Spawns `request` with piped stdin, stdout, and stderr. Blocking.
pub(crate) fn spawn(
    request: &SpawnRequest,
    deny: &[OsString],
) -> Result<StdLongLivedChild, PluginError> {
    let mut command = command(request, deny);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| PluginError::io(error.to_string()))?;
    let stdin = child.stdin.take();
    let Some(stdout) = child.stdout.take() else {
        reap_only(child, None);
        return Err(PluginError::internal("child has no standard output pipe"));
    };
    let stderr = child.stderr.take();
    let id = child.id();
    let child = Arc::new(Mutex::new(Some(child)));
    let (sender, receiver) = mpsc::channel(LINE_BUFFER);
    let reader = std::thread::Builder::new()
        .name("hofund-long-lived-stdout".into())
        .spawn(move || {
            read_lines_capped(stdout, &sender, MAX_LINE);
        });
    if let Err(error) = reader {
        if let Ok(mut guard) = child.lock()
            && let Some(child) = guard.take()
        {
            reap_kill(child);
        }
        return Err(PluginError::io(error.to_string()));
    }
    let stderr_store = Arc::new(Mutex::new(Vec::new()));
    if let Some(stderr) = stderr {
        let store = Arc::clone(&stderr_store);
        let started = std::thread::Builder::new()
            .name("hofund-long-lived-stderr".into())
            .spawn(move || retain_stderr(stderr, &store));
        if let Err(error) = started {
            if let Ok(mut guard) = child.lock()
                && let Some(child) = guard.take()
            {
                reap_kill(child);
            }
            return Err(PluginError::io(error.to_string()));
        }
    }
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    Ok(StdLongLivedChild {
        id,
        child,
        stdin: Arc::new(Mutex::new(stdin)),
        stdout: Mutex::new(Some(BodyStream::from_stream(stream))),
        stderr: stderr_store,
        exit: Arc::new(Mutex::new(None)),
    })
}

/// Reads `reader` line by line into `sender` until end of file, an error, an
/// oversized line, or a dropped receiver.
pub(crate) fn read_lines_capped(
    reader: impl Read,
    sender: &mpsc::Sender<Result<Bytes, PluginError>>,
    max_line: usize,
) {
    let mut reader = BufReader::new(reader);
    let limit = u64::try_from(max_line)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    loop {
        let mut line = Vec::new();
        let read = (&mut reader).take(limit).read_until(b'\n', &mut line);
        let item = match read {
            Ok(0) => return,
            Ok(_) if line.len() > max_line && line.last() != Some(&b'\n') => {
                let _ = sender.blocking_send(Err(PluginError::io(format!(
                    "child output line is longer than {max_line} bytes"
                ))));
                return;
            }
            Ok(_) => Ok(Bytes::from(line)),
            Err(error) => {
                let _ = sender.blocking_send(Err(PluginError::io(error.to_string())));
                return;
            }
        };
        if sender.blocking_send(item).is_err() {
            return;
        }
    }
}

pub(crate) fn retain_stderr(reader: impl Read, store: &Arc<Mutex<Vec<u8>>>) {
    let mut reader = reader;
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(count) => {
                let mut guard = match store.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                append_stderr_tail(&mut guard, &buffer[..count]);
            }
            Err(_) => return,
        }
    }
}

fn reap_only(mut child: Child, exit: Option<ExitSlot>) {
    let spawned = std::thread::Builder::new()
        .name("hofund-long-lived-reap".into())
        .spawn(move || {
            let code = match child.wait() {
                Ok(status) => Ok(exit_code_of(status)),
                Err(error) => Err(error.to_string()),
            };
            if let Some(exit) = exit {
                write_exit_store(&exit, code);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start the reap thread for a long-lived child");
    }
}

fn reap_kill(mut child: Child) {
    let _ = child.kill();
    reap_only(child, None);
}

/// A long-lived child spawned by [`crate::StdProcess`].
pub(crate) struct StdLongLivedChild {
    id: u32,
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stdout: Mutex<Option<BodyStream>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    exit: ExitSlot,
}

impl Drop for StdLongLivedChild {
    fn drop(&mut self) {
        // Detach stdin so dropping the handle does not close the pipe.
        if let Ok(mut stdin) = self.stdin.lock()
            && let Some(stdin) = stdin.take()
        {
            std::mem::forget(stdin);
        } else if let Err(poisoned) = self.stdin.lock()
            && let Some(stdin) = poisoned.into_inner().take()
        {
            std::mem::forget(stdin);
        }
        let child = match self.child.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(child) = child {
            reap_only(child, Some(Arc::clone(&self.exit)));
        }
    }
}

#[async_trait]
impl LongLivedChildBackend for StdLongLivedChild {
    fn id(&self) -> u32 {
        self.id
    }

    async fn write_stdin(&self, bytes: Bytes) -> Result<(), PluginError> {
        let stdin = Arc::clone(&self.stdin);
        accounts::run_blocking(move || {
            let mut guard = stdin
                .lock()
                .map_err(|_| PluginError::internal("stdin lock is poisoned"))?;
            let Some(pipe) = guard.as_mut() else {
                return Err(PluginError::io("standard input is closed"));
            };
            pipe.write_all(&bytes)
                .and_then(|()| pipe.flush())
                .map_err(|error| PluginError::io(error.to_string()))
        })
        .await
    }

    async fn close_stdin(&self) -> Result<(), PluginError> {
        let stdin = Arc::clone(&self.stdin);
        accounts::run_blocking(move || {
            stdin
                .lock()
                .map_err(|_| PluginError::internal("stdin lock is poisoned"))?
                .take();
            Ok(())
        })
        .await
    }

    fn take_stdout(&self) -> Option<BodyStream> {
        self.stdout.lock().ok()?.take()
    }

    async fn wait(&self) -> Result<i32, PluginError> {
        let exit = Arc::clone(&self.exit);
        let child = Arc::clone(&self.child);
        accounts::run_blocking(move || wait_for_exit(&child, &exit)).await
    }

    fn kill(&self) -> Result<(), PluginError> {
        let mut guard = self
            .child
            .lock()
            .map_err(|_| PluginError::internal("child lock is poisoned"))?;
        if let Some(child) = guard.as_mut() {
            child
                .kill()
                .map_err(|error| PluginError::io(error.to_string()))?;
        }
        Ok(())
    }

    fn stderr_tail(&self) -> Bytes {
        let guard = match self.stderr.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Bytes::copy_from_slice(&guard)
    }
}

fn wait_for_exit(child: &Arc<Mutex<Option<Child>>>, exit: &ExitSlot) -> Result<i32, PluginError> {
    loop {
        if let Some(stored) = peek_exit(exit) {
            return stored.map_err(PluginError::io);
        }
        let mut guard = match child.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(child) = guard.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = exit_code_of(status);
                    let _ = guard.take();
                    write_exit_store(exit, Ok(code));
                    return Ok(code);
                }
                Ok(None) => {}
                Err(error) => {
                    let message = error.to_string();
                    write_exit_store(exit, Err(message.clone()));
                    return Err(PluginError::io(message));
                }
            }
        }
        drop(guard);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn peek_exit(exit: &ExitSlot) -> Option<Result<i32, String>> {
    let guard = match exit.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone()
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::io::Cursor;
    use std::sync::Arc;

    use hofund_plugin_api::PluginErrorKind;
    use hofund_plugin_api::kernel::LongLivedChildBackend;

    use super::{
        MAX_LINE, STDERR_CAP, StdLongLivedChild, append_stderr_tail, is_denied, read_lines_capped,
    };

    fn lines_capped(input: Vec<u8>, max_line: usize) -> Vec<Result<Vec<u8>, PluginErrorKind>> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let reader =
            std::thread::spawn(move || read_lines_capped(Cursor::new(input), &sender, max_line));
        let mut out = Vec::new();
        while let Some(item) = receiver.blocking_recv() {
            out.push(
                item.map(|bytes| bytes.to_vec())
                    .map_err(|error| error.kind()),
            );
        }
        reader.join().expect("reader thread");
        out
    }

    #[test]
    fn stdout_line_cap_is_64_mib() {
        assert_eq!(MAX_LINE, 64 * 1024 * 1024);
        assert_eq!(STDERR_CAP, 1024 * 1024);
    }

    #[test]
    fn a_line_at_a_small_cap_passes_and_an_oversize_line_errors() {
        let passed = lines_capped(b"aaa\n".to_vec(), 3);
        assert_eq!(passed, [Ok(b"aaa\n".to_vec())]);
        let failed = lines_capped(b"aaaa".to_vec(), 3);
        assert_eq!(failed, [Err(PluginErrorKind::Io)]);
    }

    #[test]
    fn stderr_keeps_the_tail_not_the_head() {
        let mut store = Vec::new();
        append_stderr_tail(&mut store, b"HEAD");
        append_stderr_tail(&mut store, &vec![b'x'; STDERR_CAP]);
        assert_eq!(store.len(), STDERR_CAP);
        assert!(!store.starts_with(b"HEAD"));
        assert!(store.ends_with(b"x"));
        let mut store = vec![b'h'; STDERR_CAP];
        append_stderr_tail(&mut store, b"TAIL");
        assert_eq!(store.len(), STDERR_CAP);
        assert_eq!(&store[store.len() - 4..], b"TAIL");
    }

    #[test]
    fn deny_list_matches_exact_names() {
        let deny = vec![OsString::from("SECRET_TOKEN")];
        assert!(is_denied(OsStr::new("SECRET_TOKEN"), &deny));
        assert!(!is_denied(OsStr::new("PATH"), &deny));
        if cfg!(windows) {
            assert!(is_denied(OsStr::new("secret_token"), &deny));
        } else {
            assert!(!is_denied(OsStr::new("secret_token"), &deny));
        }
    }

    #[tokio::test]
    async fn wait_returns_a_stored_exit_code_through_a_poisoned_lock() {
        let slot = Arc::new(std::sync::Mutex::new(None));
        let slot_for_panic = Arc::clone(&slot);
        let poisoned = std::thread::spawn(move || {
            let _guard = slot_for_panic.lock().expect("lock");
            panic!("poison the exit slot lock");
        })
        .join();
        assert!(poisoned.is_err());
        assert!(slot.lock().is_err());
        super::write_exit_store(&slot, Ok(42));
        let child = StdLongLivedChild {
            id: 0,
            child: Arc::new(std::sync::Mutex::new(None)),
            stdin: Arc::new(std::sync::Mutex::new(None)),
            stdout: std::sync::Mutex::new(None),
            stderr: Arc::new(std::sync::Mutex::new(Vec::new())),
            exit: slot,
        };
        assert_eq!(child.wait().await.expect("wait"), 42);
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::Duration;

    use bytes::Bytes;
    use hofund_plugin_api::PluginErrorKind;
    use hofund_plugin_api::SpawnRequest;
    use hofund_plugin_api::kernel::{LongLivedChildBackend, ProcessBackend};

    use crate::StdProcess;

    async fn spawn(program: &str, args: &[&str]) -> Box<dyn LongLivedChildBackend> {
        spawn_env(program, args, Vec::new(), &[]).await
    }

    async fn spawn_env(
        program: &str,
        args: &[&str],
        env: Vec<(OsString, OsString)>,
        deny: &[&str],
    ) -> Box<dyn LongLivedChildBackend> {
        StdProcess::default()
            .spawn_long_lived(
                PathBuf::from(program),
                args.iter().map(OsString::from).collect(),
                env,
                PathBuf::from("/"),
                deny.iter().map(OsString::from).collect(),
            )
            .await
            .expect("spawn")
    }

    async fn collect(child: &dyn LongLivedChildBackend) -> Vec<String> {
        let mut stdout = child.take_stdout().expect("stdout");
        let mut out = Vec::new();
        let deadline = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(item) = stdout.next_chunk().await {
                out.push(String::from_utf8(item.expect("line").to_vec()).expect("utf8"));
            }
        });
        deadline.await.expect("stdout ends");
        out
    }

    fn alive(id: u32) -> bool {
        std::process::Command::new("/bin/kill")
            .args(["-0", &id.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("kill -0")
            .success()
    }

    fn force_kill(id: u32) {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-9", &id.to_string()])
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[tokio::test]
    async fn inherit_host_env_except_the_deny_list() {
        let child = spawn_env(
            "/usr/bin/env",
            &[],
            vec![("HOFUND_EXPLICIT".into(), "yes".into())],
            &["CARGO"],
        )
        .await;
        let lines = collect(child.as_ref()).await;
        assert!(lines.contains(&"HOFUND_EXPLICIT=yes\n".to_owned()));
        assert!(
            !lines.iter().any(|line| line.starts_with("CARGO=")),
            "denied CARGO leaked: {lines:?}"
        );
        if std::env::var_os("HOME").is_some() {
            assert!(
                lines.iter().any(|line| line.starts_with("HOME=")),
                "HOME is inherited"
            );
        }
        // A host variable outside the piped allowlist still passes when not denied.
        if let Some(path) = std::env::var_os("CARGO_MANIFEST_DIR") {
            let expected = format!("CARGO_MANIFEST_DIR={}\n", path.to_string_lossy());
            assert!(
                lines.contains(&expected),
                "non-allowlist host env should pass: missing {expected}"
            );
        }
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn request_env_can_replace_a_denied_name() {
        let child = spawn_env(
            "/usr/bin/env",
            &[],
            vec![("CARGO".into(), "from-request".into())],
            &["CARGO"],
        )
        .await;
        let lines = collect(child.as_ref()).await;
        assert!(
            lines.contains(&"CARGO=from-request\n".to_owned()),
            "{lines:?}"
        );
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn drop_does_not_kill_and_does_not_close_stdin() {
        let child = spawn("/bin/cat", &[]).await;
        let id = child.id();
        assert!(alive(id));
        drop(child);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(alive(id), "drop must not kill or stdin-EOF the child");
        force_kill(id);
        for _ in 0..100 {
            if !alive(id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("cleanup kill did not reap {id}");
    }

    #[tokio::test]
    async fn wait_returns_the_exit_code_without_killing() {
        let child = spawn("/bin/sh", &["-c", "exit 42"]).await;
        assert_eq!(child.wait().await.expect("wait"), 42);
        assert_eq!(child.wait().await.expect("second wait"), 42);
    }

    #[tokio::test]
    async fn kill_stops_the_child_and_wait_reports_status() {
        let child = spawn("/bin/sleep", &["30"]).await;
        let id = child.id();
        assert!(alive(id));
        child.kill().expect("kill");
        let code = child.wait().await.expect("wait after kill");
        assert!(
            !alive(id) || {
                tokio::time::sleep(Duration::from_millis(100)).await;
                !alive(id)
            },
            "child {id} still alive after kill, status {code}"
        );
        // SIGKILL is 9, reported as 128 + 9.
        assert_eq!(code, 137, "status {code}");
    }

    #[tokio::test]
    async fn stdout_round_trips_and_a_two_mib_line_is_accepted() {
        let child = spawn("/bin/cat", &[]).await;
        child
            .write_stdin(Bytes::from_static(b"first\n"))
            .await
            .expect("write");
        child.close_stdin().await.expect("close");
        assert_eq!(collect(child.as_ref()).await, ["first\n"]);
        assert!(child.take_stdout().is_none(), "stdout is taken once");
        let _ = child.wait().await;

        let child = spawn(
            "/bin/sh",
            &["-c", "head -c 2097152 /dev/zero | tr '\\0' a; echo"],
        )
        .await;
        let mut stdout = child.take_stdout().expect("stdout");
        let first = tokio::time::timeout(Duration::from_secs(10), stdout.next_chunk())
            .await
            .expect("first item")
            .expect("an item")
            .expect("a 2 MiB line is under the 64 MiB cap");
        assert_eq!(first.len(), 2097152 + 1);
        assert!(first.starts_with(b"a") && first.ends_with(b"\n"));
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn stderr_tail_is_retained() {
        let child = spawn("/bin/sh", &["-c", "printf 'secret-err' >&2; echo ok"]).await;
        let lines = collect(child.as_ref()).await;
        assert_eq!(lines, ["ok\n"]);
        let _ = child.wait().await;
        // Give the stderr thread a moment after wait.
        for _ in 0..50 {
            if child.stderr_tail() == Bytes::from_static(b"secret-err") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "stderr was {}",
            String::from_utf8_lossy(&child.stderr_tail())
        );
    }

    #[tokio::test]
    async fn stderr_overflow_keeps_the_tail() {
        let child = spawn(
            "/bin/sh",
            &[
                "-c",
                "head -c 1048576 /dev/zero | tr '\\0' a >&2; printf TAIL >&2; echo ok",
            ],
        )
        .await;
        let _ = collect(child.as_ref()).await;
        let _ = child.wait().await;
        let mut tail = Bytes::new();
        for _ in 0..50 {
            tail = child.stderr_tail();
            if tail.len() == 1024 * 1024 && tail.ends_with(b"TAIL") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "stderr len {} tail {:?}",
            tail.len(),
            String::from_utf8_lossy(&tail[tail.len().saturating_sub(8)..])
        );
    }

    #[tokio::test]
    async fn spawn_long_lived_rejects_a_relative_program() {
        let error = StdProcess::default()
            .spawn_long_lived(
                PathBuf::from("cat"),
                Vec::new(),
                Vec::new(),
                PathBuf::from("/"),
                Vec::new(),
            )
            .await
            .err()
            .expect("relative");
        assert_eq!(error.kind(), PluginErrorKind::Config);
        let error = crate::RecordingProcess::default()
            .spawn_long_lived(
                PathBuf::from("/bin/cat"),
                Vec::new(),
                Vec::new(),
                PathBuf::from("/"),
                Vec::new(),
            )
            .await
            .err()
            .expect("recording backend has no long-lived spawn");
        assert_eq!(error.kind(), PluginErrorKind::Unsupported);
        let _ = SpawnRequest::new("/bin/cat", Vec::new(), Vec::new(), "/").expect("absolute");
    }
}
