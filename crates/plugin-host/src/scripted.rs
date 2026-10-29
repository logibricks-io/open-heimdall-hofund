//! Scripted piped children for [`crate::RecordingProcess`].
//!
//! Nothing is executed. A test queues a [`PipedScript`] and the next piped
//! spawn plays it: it prints its lines, then either keeps running until it is
//! dropped, exits at once, or exits after the first write to its input.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use hofund_plugin_api::kernel::PipedChildBackend;
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
    ending: Ending,
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
            ending: Ending::Hang,
        }
    }

    /// Prints `line` as soon as it starts.
    #[must_use]
    pub fn line(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
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
}

/// What every scripted child of one backend reports into.
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) stdin: Mutex<Vec<Vec<u8>>>,
    pub(crate) dropped: AtomicUsize,
}

pub(crate) struct ScriptedChild {
    id: u32,
    spawn: PipedSpawn,
    tx: Mutex<Option<mpsc::UnboundedSender<Line>>>,
    stdout: Mutex<Option<BodyStream>>,
    on_stdin: Mutex<Option<OnStdin>>,
    shared: Arc<Shared>,
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
        let (tx, on_stdin) = match script.ending {
            Ending::Hang => (Some(tx), None),
            Ending::Exit(exit) => {
                send(&tx, exit(&spawn));
                (None, None)
            }
            Ending::OnStdin(reply) => (Some(tx), Some(reply)),
        };
        Self {
            id,
            spawn,
            tx: Mutex::new(tx),
            stdout: Mutex::new(Some(stdout)),
            on_stdin: Mutex::new(on_stdin),
            shared,
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
        }
        Ok(())
    }

    async fn close_stdin(&self) -> Result<(), PluginError> {
        Ok(())
    }

    fn take_stdout(&self) -> Option<BodyStream> {
        self.stdout.lock().expect("child lock").take()
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
