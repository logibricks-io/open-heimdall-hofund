//! The process log subscriber.
//!
//! `hofundd` installs one `tracing` subscriber at startup. Its `log` bridge
//! turns `log` records from dependencies into `tracing` events, so they reach
//! the same sink. The sink is standard error behind
//! [`hofund_daemon::TolerantWriter`], so a closed stream never stops serving.

use std::io;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;

/// The environment variable that sets the most detailed level written.
pub(crate) const LOG_ENV: &str = "HOFUND_LOG";

/// The level written when [`LOG_ENV`] is unset or empty.
const DEFAULT_LEVEL: LevelFilter = LevelFilter::INFO;

/// Reads the level from `value`: one of `off`, `error`, `warn`, `info`,
/// `debug`, or `trace`, in any case. Unset or empty selects `info`. Anything
/// else is an error that names the accepted values.
pub(crate) fn level(value: Option<&str>) -> Result<LevelFilter, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_LEVEL);
    };
    value.parse::<LevelFilter>().map_err(|_| {
        format!("{LOG_ENV}={value} is not a level; use off, error, warn, info, debug, or trace")
    })
}

/// Standard error that never fails a write.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Stderr;

impl<'writer> MakeWriter<'writer> for Stderr {
    type Writer = hofund_daemon::TolerantWriter<io::Stderr>;

    fn make_writer(&'writer self) -> Self::Writer {
        hofund_daemon::TolerantWriter(io::stderr())
    }
}

/// Installs the process subscriber and its `log` bridge at `level`.
///
/// # Errors
///
/// Returns the reason when a subscriber or `log` logger is already installed.
pub(crate) fn install<W>(level: LevelFilter, writer: W) -> Result<(), String>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_writer(writer)
        .try_init()
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("buffer").clone()).expect("utf-8 log")
        }
    }

    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("buffer").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for Buffer {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn levels_parse_with_info_as_the_default() {
        assert_eq!(level(None), Ok(LevelFilter::INFO));
        assert_eq!(level(Some("")), Ok(LevelFilter::INFO));
        assert_eq!(level(Some("  ")), Ok(LevelFilter::INFO));
        assert_eq!(level(Some("warn")), Ok(LevelFilter::WARN));
        assert_eq!(level(Some(" DEBUG ")), Ok(LevelFilter::DEBUG));
        assert_eq!(level(Some("off")), Ok(LevelFilter::OFF));
        let error = level(Some("loud")).expect_err("not a level");
        assert!(error.contains("HOFUND_LOG=loud"), "{error}");
        assert!(
            error.contains("off, error, warn, info, debug, or trace"),
            "{error}"
        );
    }

    /// The only test in this binary that installs the global subscriber.
    #[test]
    fn dependency_log_records_become_tracing_events() {
        let buffer = Buffer::default();
        install(LevelFilter::INFO, buffer.clone()).expect("first install");
        log::warn!(target: "dependency", "watcher fell behind");
        log::debug!(target: "dependency", "below the level");
        tracing::info!(target: "hofundd", "native event");
        let text = buffer.text();
        assert!(text.contains("WARN"), "{text}");
        assert!(text.contains("dependency"), "{text}");
        assert!(text.contains("watcher fell behind"), "{text}");
        assert!(text.contains("native event"), "{text}");
        assert!(!text.contains("below the level"), "{text}");
        assert!(install(LevelFilter::INFO, buffer).is_err());
    }
}
