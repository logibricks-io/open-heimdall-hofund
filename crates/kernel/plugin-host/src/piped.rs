//! Children with piped standard input and output.
//!
//! A plugin that drives a local agent over stdio (Claude Code) spawns it here.
//! The child sees an allowlist of the host environment plus the explicit
//! pairs in the request. Standard error is discarded. Standard output is read
//! on a thread and handed out as one chunk per line, capped at [`MAX_LINE`].
//! Dropping the child kills it and reaps it on a background thread.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use hofund_plugin_api::kernel::PipedChildBackend;
use hofund_plugin_api::{BodyStream, PluginError, SpawnRequest};
use tokio::sync::mpsc;

use crate::accounts;

/// The longest output line, newline excluded.
pub(crate) const MAX_LINE: usize = 1024 * 1024;

/// Lines buffered between the reader thread and the consumer.
const LINE_BUFFER: usize = 16;

/// Whether the host variable `name` passes to a piped child.
///
/// `PATH`, `HOME`, `USERPROFILE`, `LANG`, `LANGUAGE`, every `LC_*`, and the
/// proxy variables pass, compared without case. Windows also passes
/// `SYSTEMROOT`, without which the socket stack does not load.
pub(crate) fn is_allowed(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH"
            | "HOME"
            | "USERPROFILE"
            | "LANG"
            | "LANGUAGE"
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "NO_PROXY"
            | "ALL_PROXY"
    ) || upper.starts_with("LC_")
        || (cfg!(windows) && upper == "SYSTEMROOT")
}

fn allowed_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| name.to_str().is_some_and(is_allowed))
        .collect()
}

/// The command a piped spawn runs, before stdio is set.
pub(crate) fn command(request: &SpawnRequest) -> Command {
    let mut command = Command::new(request.program());
    command.args(request.args());
    command.current_dir(request.cwd());
    command.env_clear();
    command.envs(allowed_env());
    command.envs(request.env().iter().map(|(key, value)| (key, value)));
    command
}

fn write_exit_store(
    exit_store: &Arc<Mutex<Option<Result<i32, String>>>>,
    code: Result<i32, String>,
) {
    let mut slot = match exit_store.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    *slot = Some(code);
}

/// Spawns `request` with piped stdin and stdout. Blocking.
pub(crate) fn spawn(request: &SpawnRequest) -> Result<StdPipedChild, PluginError> {
    let mut command = command(request);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| PluginError::io(error.to_string()))?;
    let stdin = child.stdin.take();
    let Some(stdout) = child.stdout.take() else {
        reap(child);
        return Err(PluginError::internal("child has no standard output pipe"));
    };
    let id = child.id();
    let child = Arc::new(Mutex::new(Some(child)));
    let (sender, receiver) = mpsc::channel(LINE_BUFFER);
    let exit = Arc::new(Mutex::new(None::<Result<i32, String>>));
    let exit_store = Arc::clone(&exit);
    let child_for_wait = Arc::clone(&child);
    let reader = std::thread::Builder::new()
        .name("hofund-piped-stdout".into())
        .spawn(move || {
            read_lines(stdout, &sender);
            let mut guard = match child_for_wait.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(mut child) = guard.take() else {
                return;
            };
            let code = match child.wait() {
                Ok(status) => status
                    .code()
                    .ok_or_else(|| "child exited without an exit code".to_owned()),
                Err(error) => Err(error.to_string()),
            };
            write_exit_store(&exit_store, code);
        });
    if let Err(error) = reader {
        if let Ok(mut guard) = child.lock()
            && let Some(child) = guard.take()
        {
            reap(child);
        }
        return Err(PluginError::io(error.to_string()));
    }
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    Ok(StdPipedChild {
        id,
        child,
        stdin: Arc::new(Mutex::new(stdin)),
        stdout: Mutex::new(Some(BodyStream::from_stream(stream))),
        exit,
    })
}

/// Reads `reader` line by line into `sender` until end of file, an error, an
/// oversized line, or a dropped receiver.
pub(crate) fn read_lines(reader: impl Read, sender: &mpsc::Sender<Result<Bytes, PluginError>>) {
    let mut reader = BufReader::new(reader);
    let limit = u64::try_from(MAX_LINE)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    loop {
        let mut line = Vec::new();
        let read = (&mut reader).take(limit).read_until(b'\n', &mut line);
        let item = match read {
            Ok(0) => return,
            Ok(_) if line.len() > MAX_LINE && line.last() != Some(&b'\n') => {
                let _ = sender.blocking_send(Err(PluginError::io(
                    "child output line is longer than 1 MiB",
                )));
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

/// Kills `child` and reaps it on a background thread.
fn reap(mut child: Child) {
    let _ = child.kill();
    let spawned = std::thread::Builder::new()
        .name("hofund-reap".into())
        .spawn(move || {
            let _ = child.wait();
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start the reap thread for a piped child");
    }
}

/// A piped child spawned by [`crate::StdProcess`].
pub(crate) struct StdPipedChild {
    id: u32,
    child: Arc<Mutex<Option<Child>>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    stdout: Mutex<Option<BodyStream>>,
    exit: Arc<Mutex<Option<Result<i32, String>>>>,
}

impl Drop for StdPipedChild {
    fn drop(&mut self) {
        let child = match self.child.lock() {
            Ok(mut child) => child.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(child) = child {
            reap(child);
        }
        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        }
    }
}

#[async_trait]
impl PipedChildBackend for StdPipedChild {
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

    async fn exit_code(&self) -> Result<i32, PluginError> {
        let exit = Arc::clone(&self.exit);
        accounts::run_blocking(move || {
            loop {
                match exit.lock() {
                    Ok(slot) => {
                        if let Some(stored) = slot.as_ref() {
                            return stored
                                .as_ref()
                                .map_err(|message| PluginError::io(message.clone()))
                                .copied();
                        }
                    }
                    Err(_) => {
                        return Err(PluginError::internal("exit slot lock is poisoned"));
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use hofund_plugin_api::PluginErrorKind;

    use super::{MAX_LINE, is_allowed, read_lines, write_exit_store};

    #[test]
    fn write_exit_store_records_through_a_poisoned_lock() {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot_for_panic = std::sync::Arc::clone(&slot);
        let poisoned = std::thread::spawn(move || {
            let _guard = slot_for_panic.lock().expect("lock");
            panic!("poison the exit slot lock");
        })
        .join();
        assert!(poisoned.is_err());
        assert!(slot.lock().is_err());
        write_exit_store(&slot, Ok(44));
        let stored = match slot.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        assert_eq!(stored.as_ref().unwrap(), &Ok(44));
    }

    fn lines(input: Vec<u8>) -> Vec<Result<Vec<u8>, PluginErrorKind>> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let reader = std::thread::spawn(move || read_lines(Cursor::new(input), &sender));
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
    fn allowlist_passes_path_home_locale_and_proxies_only() {
        for name in [
            "PATH",
            "Path",
            "HOME",
            "USERPROFILE",
            "LANG",
            "LANGUAGE",
            "LC_ALL",
            "LC_CTYPE",
            "HTTP_PROXY",
            "https_proxy",
            "NO_PROXY",
            "all_proxy",
        ] {
            assert!(is_allowed(name), "{name} should pass");
        }
        for name in [
            "AWS_SECRET_ACCESS_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "SSH_AUTH_SOCK",
            "LD_PRELOAD",
            "PATHEXT_X",
            "XPATH",
            "HOMEDRIVE",
            "LCX",
            "",
        ] {
            assert!(!is_allowed(name), "{name} should not pass");
        }
        assert_eq!(is_allowed("SYSTEMROOT"), cfg!(windows));
    }

    #[test]
    fn lines_keep_the_newline_and_the_last_partial_line() {
        let out = lines(b"one\ntwo\nthree".to_vec());
        assert_eq!(
            out,
            vec![
                Ok(b"one\n".to_vec()),
                Ok(b"two\n".to_vec()),
                Ok(b"three".to_vec())
            ]
        );
        assert!(lines(Vec::new()).is_empty());
    }

    #[test]
    fn a_line_of_exactly_the_cap_passes() {
        let mut input = vec![b'a'; MAX_LINE];
        input.push(b'\n');
        input.extend_from_slice(b"next\n");
        let out = lines(input);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_ref().map(Vec::len), Ok(MAX_LINE + 1));
        assert_eq!(out[1], Ok(b"next\n".to_vec()));
        assert_eq!(lines(vec![b'b'; MAX_LINE]), vec![Ok(vec![b'b'; MAX_LINE])]);
    }

    #[test]
    fn oversized_line_errors() {
        let mut input = vec![b'a'; MAX_LINE + 1];
        input.extend_from_slice(b"\nnext\n");
        assert_eq!(lines(input), vec![Err(PluginErrorKind::Io)]);
        assert_eq!(
            lines(vec![b'a'; MAX_LINE + 1]),
            vec![Err(PluginErrorKind::Io)]
        );
    }

    #[cfg(unix)]
    mod process {
        use std::ffi::OsString;
        use std::path::PathBuf;
        use std::time::Duration;

        use bytes::Bytes;
        use hofund_plugin_api::kernel::{PipedChildBackend, ProcessBackend};
        use hofund_plugin_api::{PluginErrorKind, SpawnRequest};

        use crate::StdProcess;

        async fn spawn(program: &str, args: &[&str]) -> Box<dyn PipedChildBackend> {
            spawn_env(program, args, Vec::new()).await
        }

        async fn spawn_env(
            program: &str,
            args: &[&str],
            env: Vec<(OsString, OsString)>,
        ) -> Box<dyn PipedChildBackend> {
            StdProcess::default()
                .spawn_piped(
                    PathBuf::from(program),
                    args.iter().map(OsString::from).collect(),
                    env,
                    PathBuf::from("/"),
                )
                .await
                .expect("spawn")
        }

        async fn collect(child: &dyn PipedChildBackend) -> Vec<String> {
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

        #[tokio::test]
        async fn exit_code_is_visible_after_stdout_eof() {
            let child = spawn("/bin/sh", &["-c", "exit 44"]).await;
            let mut stdout = child.take_stdout().expect("stdout");
            while stdout.next_chunk().await.is_some() {}
            assert_eq!(child.exit_code().await.expect("exit code"), 44);
            assert_eq!(child.exit_code().await.expect("second read"), 44);
        }

        #[tokio::test]
        async fn piped_child_round_trips_lines() {
            let child = spawn("/bin/cat", &[]).await;
            child
                .write_stdin(Bytes::from_static(b"first\n"))
                .await
                .expect("write");
            child
                .write_stdin(Bytes::from_static(b"second\n"))
                .await
                .expect("write");
            child.close_stdin().await.expect("close");
            assert_eq!(collect(child.as_ref()).await, ["first\n", "second\n"]);
            assert!(child.take_stdout().is_none(), "stdout is taken once");
        }

        #[tokio::test]
        async fn close_stdin_signals_eof() {
            let child = spawn("/bin/cat", &[]).await;
            child.close_stdin().await.expect("close");
            child.close_stdin().await.expect("closing twice is fine");
            assert!(collect(child.as_ref()).await.is_empty());
            let error = child
                .write_stdin(Bytes::from_static(b"late\n"))
                .await
                .expect_err("closed");
            assert_eq!(error.kind(), PluginErrorKind::Io);
        }

        #[tokio::test]
        async fn env_cleared_except_allowlist() {
            let child = spawn_env(
                "/usr/bin/env",
                &[],
                vec![("HOFUND_EXPLICIT".into(), "yes".into())],
            )
            .await;
            let lines = collect(child.as_ref()).await;
            let names: Vec<&str> = lines
                .iter()
                .filter_map(|line| line.split_once('=').map(|(name, _)| name))
                .collect();
            assert!(lines.contains(&"HOFUND_EXPLICIT=yes\n".to_owned()));
            for name in &names {
                assert!(
                    *name == "HOFUND_EXPLICIT" || super::super::is_allowed(name),
                    "{name} leaked into the child"
                );
            }
            if std::env::var_os("PATH").is_some() {
                assert!(names.contains(&"PATH"), "PATH passes");
            }
        }

        #[tokio::test]
        async fn kill_on_drop_reaps() {
            let child = spawn("/bin/sleep", &["30"]).await;
            let id = child.id();
            assert!(alive(id));
            drop(child);
            let mut gone = false;
            for _ in 0..100 {
                if !alive(id) {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(gone, "child {id} was killed and reaped");
        }

        #[tokio::test]
        async fn drop_does_not_wait_for_a_blocked_write() {
            // `sleep` never reads stdin, so a large write fills the pipe and
            // blocks. Cancelling the future leaves the blocking write running
            // with the stdin lock held; dropping the child must still return.
            let child = spawn("/bin/sleep", &["30"]).await;
            let id = child.id();
            let write = tokio::time::timeout(
                Duration::from_millis(300),
                child.write_stdin(Bytes::from(vec![b'x'; 4 * 1024 * 1024])),
            )
            .await;
            assert!(write.is_err(), "the write is blocked on a full pipe");
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                drop(child);
                let _ = done.send(());
            });
            finished
                .recv_timeout(Duration::from_secs(10))
                .expect("drop returns while a write is blocked");
            for _ in 0..100 {
                if !alive(id) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!("child {id} still alive");
        }

        #[tokio::test]
        async fn oversized_child_line_ends_the_stream_with_io() {
            let child = spawn(
                "/bin/sh",
                &[
                    "-c",
                    "head -c 1048577 /dev/zero | tr '\\0' a; echo; echo after",
                ],
            )
            .await;
            let mut stdout = child.take_stdout().expect("stdout");
            let first = tokio::time::timeout(Duration::from_secs(10), stdout.next_chunk())
                .await
                .expect("first item")
                .expect("an item");
            assert_eq!(first.expect_err("oversized").kind(), PluginErrorKind::Io);
            let rest = tokio::time::timeout(Duration::from_secs(10), stdout.next_chunk())
                .await
                .expect("stream ends");
            assert!(rest.is_none());
        }

        #[tokio::test]
        async fn spawn_piped_rejects_a_relative_program() {
            let error = StdProcess::default()
                .spawn_piped(
                    PathBuf::from("cat"),
                    Vec::new(),
                    Vec::new(),
                    PathBuf::from("/"),
                )
                .await
                .err()
                .expect("relative");
            assert_eq!(error.kind(), PluginErrorKind::Config);
            let error = crate::RecordingProcess::default()
                .spawn_piped(
                    PathBuf::from("/bin/cat"),
                    Vec::new(),
                    Vec::new(),
                    PathBuf::from("/"),
                )
                .await
                .err()
                .expect("recording backend has no piped spawn");
            assert_eq!(error.kind(), PluginErrorKind::Unsupported);
            let _ = SpawnRequest::new("/bin/cat", Vec::new(), Vec::new(), "/").expect("absolute");
        }
    }
}
