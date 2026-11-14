//! One-shot `hofundd` spawn: clear the cargo jobserver and serialize spawns.
//!
//! `cargo test --workspace` runs many integration test binaries in parallel on
//! a small runner. Each may start a large `hofundd one-shot` child that must
//! answer HTTP within five seconds. A cross-process advisory lock keeps those
//! spawns from competing for the same startup window. Clearing
//! `CARGO_MAKEFLAGS` and `MAKEFLAGS` keeps a nested build helper from waiting
//! on the parent's jobserver (see offline `hofundd()` prebuild).

use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::ClientError;

/// Variables that make a child participate in cargo's make jobserver.
pub(crate) const CARGO_JOB_SERVER_ENV: &[&str] = &["CARGO_MAKEFLAGS", "MAKEFLAGS"];

/// Clears jobserver variables on a command about to spawn one-shot `hofundd`.
pub(crate) trait CommandClearJobEnv {
    /// Removes [`CARGO_JOB_SERVER_ENV`] entries from the child's environment.
    fn clear_cargo_jobserver_env(&mut self) -> &mut Self;
}

impl CommandClearJobEnv for std::process::Command {
    fn clear_cargo_jobserver_env(&mut self) -> &mut Self {
        for key in CARGO_JOB_SERVER_ENV {
            self.env_remove(key);
        }
        self
    }
}

impl CommandClearJobEnv for tokio::process::Command {
    fn clear_cargo_jobserver_env(&mut self) -> &mut Self {
        for key in CARGO_JOB_SERVER_ENV {
            self.env_remove(key);
        }
        self
    }
}

/// An advisory lock held from one-shot spawn through handshake completion.
pub(crate) struct OneShotSpawnGuard {
    _lock: std::fs::File,
}

impl OneShotSpawnGuard {
    /// Waits for any other process to finish its one-shot spawn handshake.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the lock file cannot be opened or locked.
    pub(crate) fn acquire() -> io::Result<Self> {
        let path = spawn_lock_path();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        file.lock()?;
        Ok(Self { _lock: file })
    }
}

fn spawn_lock_path() -> &'static PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| std::env::temp_dir().join("hofund-oneshot-spawn.lock"))
}

/// Takes the cross-process one-shot spawn slot for an async body.
pub(crate) async fn with_one_shot_spawn_guard<T, F, Fut>(f: F) -> Result<T, ClientError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>>,
{
    let _guard = tokio::task::spawn_blocking(OneShotSpawnGuard::acquire)
        .await
        .map_err(|join| ClientError::Transport(format!("one-shot spawn lock task: {join}")))?
        .map_err(|io| ClientError::Transport(format!("one-shot spawn lock: {io}")))?;
    f().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn clear_cargo_jobserver_env_removes_explicit_makeflags() {
        let mut command = Command::new("env");
        command.env("CARGO_MAKEFLAGS", "-j8 --jobserver-fds=3,4");
        command.env("MAKEFLAGS", "-j8 --jobserver-fds=3,4");
        command.clear_cargo_jobserver_env();
        let output = command.output().expect("env");
        assert!(output.status.success(), "{output:?}");
        let lines = String::from_utf8_lossy(&output.stdout);
        assert!(
            !lines.contains("CARGO_MAKEFLAGS="),
            "child still has CARGO_MAKEFLAGS: {lines}"
        );
        assert!(
            !lines.contains("MAKEFLAGS="),
            "child still has MAKEFLAGS: {lines}"
        );
    }
}
