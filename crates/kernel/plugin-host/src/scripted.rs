//! Scripted piped children for [`crate::RecordingProcess`].
//!
//! Nothing is executed. A test queues a [`PipedScript`] and the next piped
//! spawn plays it: it prints its lines, then either keeps running until it is
//! dropped, exits at once, or exits after the first write to its input.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use bytes::Bytes;
use hofund_plugin_api::kernel::{LongLivedChildBackend, PipedChildBackend};
use hofund_plugin_api::{BodyStream, PluginError};
use tokio::sync::mpsc;

type Line = Result<Bytes, PluginError>;
type OnExit = Box<dyn FnOnce(&PipedSpawn) -> Vec<String> + Send>;
type OnStdin = Box<dyn FnOnce(&PipedSpawn, &[u8]) -> Vec<String> + Send>;

/// One piped spawn the recording backend accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PipedSpawn {
    /// The absolute program path.
    pub program: PathBuf,
    /// The arguments, unparsed.
    pub args: Vec<OsString>,
    /// The extra environment the plugin asked for.
    pub env: Vec<(OsString, OsString)>,
    /// The working directory.
    pub cwd: PathBuf,
}

impl PipedSpawn {
    /// The value the plugin set for `name`, if any.
    #[must_use]
    pub fn env_var(&self, name: &str) -> Option<&OsStr> {
        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_os_str())
    }

    /// The arguments as UTF-8, lossily.
    #[must_use]
    pub fn arg_strings(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
}

enum Ending {
    Hang,
    Exit(OnExit),
    OnStdin(OnStdin),
}

/// What the next piped child does.
pub struct PipedScript {
    lines: Vec<String>,
    stderr: Bytes,
    ending: Ending,
    status: i32,
}

impl Default for PipedScript {
    fn default() -> Self {
        Self::new()
    }
}

impl PipedScript {
    /// A child that prints nothing and runs until it is dropped.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            stderr: Bytes::new(),
            ending: Ending::Hang,
            status: 0,
        }
    }

    /// Prints `line` as soon as it starts.
    #[must_use]
    pub fn line(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
        self
    }

    /// Writes `text` to standard error.
    #[must_use]
    pub fn stderr(mut self, text: impl Into<Bytes>) -> Self {
        self.stderr = text.into();
        self
    }

    /// Exits right after its first lines. `exit` runs first, sees the
    /// spawn, and returns more lines to print.
    #[must_use]
    pub fn exits(mut self, exit: impl FnOnce(&PipedSpawn) -> Vec<String> + Send + 'static) -> Self {
        self.ending = Ending::Exit(Box::new(exit));
        self
    }

    /// Exits after the first write to its input. `reply` sees the spawn and
    /// the bytes, and returns the lines to print before it exits.
    #[must_use]
    pub fn on_stdin(
        mut self,
        reply: impl FnOnce(&PipedSpawn, &[u8]) -> Vec<String> + Send + 'static,
    ) -> Self {
        self.ending = Ending::OnStdin(Box::new(reply));
        self
    }

    /// Sets the exit status reported after the child exits.
    ///
    /// A hanging script never reports a status until it is dropped; `exit_code`
    /// stays pending so a caller timeout can win.
    #[must_use]
    pub fn status(mut self, code: i32) -> Self {
        self.status = code;
        self
    }
}

/// What every scripted child of one backend reports into.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) stdin: Mutex<Vec<Vec<u8>>>,
    pub(crate) dropped: AtomicUsize,
}

enum ExitWait {
    Open(Vec<Waker>),
    Code(i32),
}

struct ExitCell {
    inner: Mutex<ExitWait>,
}

impl ExitCell {
    fn open() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(ExitWait::Open(Vec::new())),
        })
    }

    fn finish(&self, code: i32) {
        let mut inner = self.inner.lock().expect("child lock");
        let previous = std::mem::replace(&mut *inner, ExitWait::Code(code));
        if let ExitWait::Open(wakers) = previous {
            for waker in wakers {
                waker.wake();
            }
        }
    }
}

struct ExitCodeWait {
    cell: Arc<ExitCell>,
}

impl Future for ExitCodeWait {
    type Output = i32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.cell.inner.lock().expect("child lock");
        match &mut *inner {
            ExitWait::Code(code) => Poll::Ready(*code),
            ExitWait::Open(wakers) => {
                if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                    wakers.push(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

pub(crate) struct ScriptedChild {
    id: u32,
    spawn: PipedSpawn,
    tx: Mutex<Option<mpsc::UnboundedSender<Line>>>,
    stdout: Mutex<Option<BodyStream>>,
    stderr: Bytes,
    on_stdin: Mutex<Option<OnStdin>>,
    shared: Arc<Shared>,
    exit: Arc<ExitCell>,
    status: i32,
}

impl ScriptedChild {
    pub(crate) fn start(
        id: u32,
        spawn: PipedSpawn,
        script: PipedScript,
        shared: Arc<Shared>,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Line>();
        let stdout =
            BodyStream::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)));
        send(&tx, script.lines);
        let exit = ExitCell::open();
        let status = script.status;
        let (tx, on_stdin) = match script.ending {
            Ending::Hang => (Some(tx), None),
            Ending::Exit(on_exit) => {
                send(&tx, on_exit(&spawn));
                exit.finish(status);
                (None, None)
            }
            Ending::OnStdin(reply) => (Some(tx), Some(reply)),
        };
        Self {
            id,
            spawn,
            tx: Mutex::new(tx),
            stdout: Mutex::new(Some(stdout)),
            stderr: script.stderr,
            on_stdin: Mutex::new(on_stdin),
            shared,
            exit,
            status,
        }
    }
}

fn send(tx: &mpsc::UnboundedSender<Line>, lines: Vec<String>) {
    for mut line in lines {
        if !line.ends_with('\n') {
            line.push('\n');
        }
        let _ = tx.send(Ok(Bytes::from(line)));
    }
}

#[async_trait]
impl PipedChildBackend for ScriptedChild {
    fn id(&self) -> u32 {
        self.id
    }

    async fn write_stdin(&self, bytes: Bytes) -> Result<(), PluginError> {
        let mut tx = self.tx.lock().expect("child lock");
        let Some(sender) = tx.as_ref() else {
            return Err(PluginError::io("standard input is closed"));
        };
        self.shared
            .stdin
            .lock()
            .expect("stdin lock")
            .push(bytes.to_vec());
        let reply = self.on_stdin.lock().expect("child lock").take();
        if let Some(reply) = reply {
            send(sender, reply(&self.spawn, &bytes));
            *tx = None;
            self.exit.finish(self.status);
        }
        Ok(())
    }

    async fn close_stdin(&self) -> Result<(), PluginError> {
        Ok(())
    }

    fn take_stdout(&self) -> Option<BodyStream> {
        self.stdout.lock().expect("child lock").take()
    }

    async fn exit_code(&self) -> Result<i32, PluginError> {
        Ok(ExitCodeWait {
            cell: Arc::clone(&self.exit),
        }
        .await)
    }

    fn stderr_tail(&self) -> Bytes {
        self.stderr.clone()
    }
}

#[async_trait]
impl LongLivedChildBackend for ScriptedChild {
    fn id(&self) -> u32 {
        self.id
    }

    async fn write_stdin(&self, bytes: Bytes) -> Result<(), PluginError> {
        PipedChildBackend::write_stdin(self, bytes).await
    }

    async fn close_stdin(&self) -> Result<(), PluginError> {
        PipedChildBackend::close_stdin(self).await
    }

    fn take_stdout(&self) -> Option<BodyStream> {
        PipedChildBackend::take_stdout(self)
    }

    async fn wait(&self) -> Result<i32, PluginError> {
        PipedChildBackend::exit_code(self).await
    }

    fn kill(&self) -> Result<(), PluginError> {
        self.exit.finish(self.status);
        Ok(())
    }

    fn stderr_tail(&self) -> Bytes {
        PipedChildBackend::stderr_tail(self)
    }
}

impl Drop for ScriptedChild {
    fn drop(&mut self) {
        self.shared.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// Queued scripts plus what the plugin did with them.
#[derive(Default)]
pub(crate) struct Piped {
    pub(crate) scripts: Mutex<VecDeque<PipedScript>>,
    pub(crate) spawns: Mutex<Vec<PipedSpawn>>,
    pub(crate) shared: Arc<Shared>,
}

#[cfg(test)]
mod tests {
    use super::PipedScript;
    use crate::RecordingProcess;
    use hofund_plugin_api::kernel::ProcessBackend;
    use std::path::PathBuf;

    #[tokio::test]
    async fn scripted_child_reports_a_nonzero_exit_status() {
        let process = RecordingProcess::default();
        process.script_piped(
            PipedScript::new()
                .stderr("diagnostic")
                .exits(|_| Vec::new())
                .status(44),
        );
        let child = process
            .spawn_piped(
                PathBuf::from("/usr/bin/security"),
                Vec::new(),
                Vec::new(),
                PathBuf::from("/tmp"),
            )
            .await
            .expect("spawn");
        assert_eq!(child.exit_code().await.expect("exit"), 44);
        assert_eq!(&child.stderr_tail()[..], b"diagnostic");
        let mut stdout = child.take_stdout().expect("stdout");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stdout.next_chunk())
                .await
                .expect("stdout EOF")
                .is_none()
        );
    }

    #[tokio::test]
    async fn scripted_exit_without_output_closes_stdout() {
        let process = RecordingProcess::default();
        process.script_piped(PipedScript::new().exits(|_| Vec::new()));
        let child = process
            .spawn_piped(
                PathBuf::from("/usr/bin/security"),
                Vec::new(),
                Vec::new(),
                PathBuf::from("/tmp"),
            )
            .await
            .expect("spawn");
        let mut stdout = child.take_stdout().expect("stdout");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stdout.next_chunk())
                .await
                .expect("stdout EOF")
                .is_none()
        );
    }
}
