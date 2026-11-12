//! Process termination: signal-driven drain, exit status, and logging that
//! survives a closed terminal.
//!
//! On Unix, `SIGHUP`, `SIGINT` and `SIGTERM` drain the daemon and then raise
//! the same signal again with its default disposition, so a parent shell or
//! supervisor sees the process die of the signal it sent. A `SIGHUP` that was
//! already ignored when the process started, such as under `nohup`, stays
//! ignored. On Windows, Ctrl-C and Ctrl-Break drain and exit 130; closing
//! the console window, logging off and shutting down drain for at most four
//! seconds and exit 143, because Windows ends the process soon after those
//! events.

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use hofund_gateway::Running;

/// Why a daemon process stopped serving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Termination {
    /// A control `POST /v1/hofund/shutdown` asked the daemon to stop.
    Requested,
    /// A Unix termination signal arrived.
    #[cfg(unix)]
    Signal(i32),
    /// A Windows console control event arrived.
    #[cfg(windows)]
    Console(ConsoleEvent),
    /// A fatal error stopped the daemon, with its message.
    Fatal(String),
}

/// A Windows console control event.
///
/// The type exists on every platform so the exit mapping can be tested
/// anywhere; only Windows receives these events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleEvent {
    /// Ctrl-C in the console.
    CtrlC,
    /// Ctrl-Break in the console.
    CtrlBreak,
    /// The console window is closing.
    Close,
    /// The user is logging off.
    Logoff,
    /// The system is shutting down.
    Shutdown,
}

impl ConsoleEvent {
    /// The process exit code after this event: 130 for an interrupt, 143
    /// for the events Windows follows by ending the process.
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::CtrlC | Self::CtrlBreak => 130,
            Self::Close | Self::Logoff | Self::Shutdown => 143,
        }
    }

    /// The longest drain this event allows. `None` waits for the drain.
    #[must_use]
    pub fn drain_bound(self) -> Option<Duration> {
        match self {
            Self::CtrlC | Self::CtrlBreak => None,
            Self::Close | Self::Logoff | Self::Shutdown => Some(CONSOLE_DRAIN),
        }
    }
}

/// The drain budget after a console close, logoff or shutdown event.
const CONSOLE_DRAIN: Duration = hofund_gateway::CONTROL_DRAIN;

/// Serves until a termination signal or a control shutdown, drains, and
/// reports why it stopped.
///
/// The drain matches [`crate::serve_until_shutdown`]: control listeners stop
/// first and the model listener finishes its in-flight requests. Pass the
/// result to [`finish`] once the runtime is done.
pub async fn serve_until_signal(running: Running) -> Termination {
    serve_until(running, termination_signal()).await
}

/// Serves until `signal` resolves or a control shutdown, then drains.
async fn serve_until(
    mut running: Running,
    signal: impl Future<Output = (Termination, Option<Duration>)>,
) -> Termination {
    let received = tokio::select! {
        received = signal => Some(received),
        () = running.shutdown_requested() => None,
    };
    let (reason, bound) = match received {
        Some(received) => received,
        None => (
            running
                .fatal()
                .map_or(Termination::Requested, Termination::Fatal),
            None,
        ),
    };
    notice(&format!("draining: {}", describe(&reason)));
    match bound {
        Some(bound) => {
            if tokio::time::timeout(bound, running.shutdown())
                .await
                .is_err()
            {
                notice("drain did not finish in time");
            }
        }
        None => running.shutdown().await,
    }
    notice("stopped");
    reason
}

/// Turns a [`Termination`] into the process exit status.
///
/// After a Unix signal this raises the signal again with its default
/// disposition and does not return when that succeeds; exit code `128 + n`
/// is the fallback. Call it after the runtime has stopped.
#[must_use]
pub fn finish(termination: Termination) -> ExitCode {
    match termination {
        Termination::Requested => ExitCode::SUCCESS,
        Termination::Fatal(_) => ExitCode::FAILURE,
        #[cfg(unix)]
        Termination::Signal(signal) => {
            // Restores the default action and raises; a terminating signal
            // does not come back here.
            let _ = signal_hook::low_level::emulate_default_handler(signal);
            ExitCode::from(signal_exit_code(signal))
        }
        #[cfg(windows)]
        Termination::Console(event) => ExitCode::from(event.exit_code()),
    }
}

/// Ends the process after a signal or console event the way [`finish`]
/// maps it, and returns the control shutdown and fatal outcomes, the latter
/// as its message.
pub(crate) fn conclude(termination: Termination) -> Result<(), String> {
    match termination {
        Termination::Requested => Ok(()),
        Termination::Fatal(message) => Err(message),
        #[cfg(unix)]
        Termination::Signal(signal) => {
            let _ = finish(Termination::Signal(signal));
            std::process::exit(i32::from(signal_exit_code(signal)))
        }
        #[cfg(windows)]
        Termination::Console(event) => std::process::exit(i32::from(event.exit_code())),
    }
}

/// The shell convention for a process ended by `signal`.
#[cfg(unix)]
fn signal_exit_code(signal: i32) -> u8 {
    u8::try_from(128 + signal).unwrap_or(u8::MAX)
}

fn describe(reason: &Termination) -> String {
    match reason {
        Termination::Requested => "shutdown requested".to_owned(),
        Termination::Fatal(message) => format!("fatal error: {message}"),
        #[cfg(unix)]
        Termination::Signal(signal) => format!("signal {signal}"),
        #[cfg(windows)]
        Termination::Console(event) => format!("console event {event:?}"),
    }
}

/// Writes one line to standard error, ignoring a closed or failed stream.
///
/// A daemon whose terminal went away must still drain, so these lines never
/// panic the way `eprintln!` does.
pub(crate) fn notice(line: &str) {
    let mut stderr = TolerantWriter(io::stderr());
    let _ = writeln!(stderr, "{line}");
}

/// An [`io::Write`] adapter that reports success when the inner writer fails.
///
/// Log output wraps its sink in this so a hung-up terminal or closed pipe
/// cannot stop a drain.
#[derive(Debug)]
pub struct TolerantWriter<W>(pub W);

impl<W: Write> Write for TolerantWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(self.0.write(buf).unwrap_or(buf.len()))
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.0.flush();
        Ok(())
    }
}

/// Resolves with the first termination signal and its drain bound.
async fn termination_signal() -> (Termination, Option<Duration>) {
    platform::next().await
}

#[cfg(unix)]
mod platform {
    use std::time::Duration;

    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use tokio::signal::unix::{Signal, SignalKind, signal};

    use super::{Termination, notice};

    pub(super) async fn next() -> (Termination, Option<Duration>) {
        // The ignored check must run before the handler replaces SIG_IGN.
        let hangup = !super::hangup_ignored_at_start();
        match Handlers::register(hangup) {
            Ok(mut handlers) => (Termination::Signal(handlers.recv().await), None),
            Err(error) => {
                notice(&format!("cannot register signal handlers: {error}"));
                std::future::pending().await
            }
        }
    }

    struct Handlers {
        interrupt: Signal,
        terminate: Signal,
        hangup: Option<Signal>,
    }

    impl Handlers {
        fn register(hangup: bool) -> std::io::Result<Self> {
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
                hangup: if hangup {
                    Some(signal(SignalKind::hangup())?)
                } else {
                    None
                },
            })
        }

        async fn recv(&mut self) -> i32 {
            let hangup = async {
                match &mut self.hangup {
                    Some(hangup) => hangup.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = self.interrupt.recv() => SIGINT,
                _ = self.terminate.recv() => SIGTERM,
                _ = hangup => SIGHUP,
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use tokio::signal::windows;

    use super::{ConsoleEvent, Termination, notice};

    pub(super) async fn next() -> (Termination, Option<Duration>) {
        match event().await {
            Ok(event) => (Termination::Console(event), event.drain_bound()),
            Err(error) => {
                notice(&format!("cannot register console handlers: {error}"));
                std::future::pending().await
            }
        }
    }

    async fn event() -> std::io::Result<ConsoleEvent> {
        let mut ctrl_c = windows::ctrl_c()?;
        let mut ctrl_break = windows::ctrl_break()?;
        let mut close = windows::ctrl_close()?;
        let mut logoff = windows::ctrl_logoff()?;
        let mut shutdown = windows::ctrl_shutdown()?;
        Ok(tokio::select! {
            _ = ctrl_c.recv() => ConsoleEvent::CtrlC,
            _ = ctrl_break.recv() => ConsoleEvent::CtrlBreak,
            _ = close.recv() => ConsoleEvent::Close,
            _ = logoff.recv() => ConsoleEvent::Logoff,
            _ = shutdown.recv() => ConsoleEvent::Shutdown,
        })
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::time::Duration;

    use super::Termination;

    pub(super) async fn next() -> (Termination, Option<Duration>) {
        std::future::pending().await
    }
}

/// Whether `SIGHUP` was ignored when the process started.
///
/// Linux reads the ignored mask from `/proc/self/status`. Other systems
/// report `false` and register the handler.
#[cfg(unix)]
fn hangup_ignored_at_start() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| ignored_mask(&status))
            .is_some_and(|mask| ignores(mask, signal_hook::consts::SIGHUP))
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The `SigIgn` mask from a Linux `/proc/<pid>/status` file.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn ignored_mask(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigIgn:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
}

/// Whether `mask` has the bit for `signal`; bit 0 is signal 1.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn ignores(mask: u64, signal: i32) -> bool {
    match u32::try_from(signal - 1) {
        Ok(bit) if bit < 64 => mask & (1 << bit) != 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_interrupts_exit_130_and_wait_for_the_drain() {
        for event in [ConsoleEvent::CtrlC, ConsoleEvent::CtrlBreak] {
            assert_eq!(event.exit_code(), 130, "{event:?}");
            assert_eq!(event.drain_bound(), None, "{event:?}");
        }
    }

    #[test]
    fn console_endings_exit_143_after_a_bounded_drain() {
        for event in [
            ConsoleEvent::Close,
            ConsoleEvent::Logoff,
            ConsoleEvent::Shutdown,
        ] {
            assert_eq!(event.exit_code(), 143, "{event:?}");
            assert_eq!(
                event.drain_bound(),
                Some(Duration::from_secs(4)),
                "{event:?}"
            );
        }
    }

    struct Broken {
        flushed: bool,
    }

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushed = true;
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    fn tolerant_writer_swallows_write_and_flush_errors() {
        let mut writer = TolerantWriter(Broken { flushed: false });
        assert_eq!(writer.write(b"abc").expect("write"), 3);
        writeln!(writer, "line").expect("writeln");
        writer.flush().expect("flush");
        assert!(writer.0.flushed, "flush reaches the inner writer");
    }

    #[test]
    fn tolerant_writer_passes_output_through() {
        let mut writer = TolerantWriter(Vec::new());
        writeln!(writer, "hello").expect("writeln");
        assert_eq!(writer.write(b"ab").expect("write"), 2);
        assert_eq!(writer.0, b"hello\nab");
    }

    #[test]
    fn sig_ign_mask_marks_ignored_signals() {
        let status =
            "Name:\thofund\nSigBlk:\t0000000000000000\nSigIgn:\t0000000000001001\nSigCgt:\t0\n";
        let mask = ignored_mask(status).expect("mask");
        assert_eq!(mask, 0x1001);
        assert!(ignores(mask, 1), "SIGHUP is bit 0");
        assert!(ignores(mask, 13), "SIGPIPE is bit 12");
        assert!(!ignores(mask, 2));
        assert!(!ignores(mask, 15));
        assert!(!ignores(mask, 0));
        assert!(!ignores(mask, 65));
        assert!(!ignores(u64::MAX, 0));
        assert!(ignores(u64::MAX, 64));
        assert_eq!(ignored_mask("SigIgn:\tzz\n"), None);
        assert_eq!(ignored_mask("Name:\tx\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn signal_exit_codes_follow_the_shell_convention() {
        assert_eq!(signal_exit_code(1), 129);
        assert_eq!(signal_exit_code(15), 143);
        assert_eq!(signal_exit_code(200), 255);
    }

    #[test]
    fn finish_maps_requested_and_fatal() {
        assert_eq!(finish(Termination::Requested), ExitCode::SUCCESS);
        assert_eq!(finish(Termination::Fatal("x".into())), ExitCode::FAILURE);
    }
}
