//! Resolving the `claude` program on Windows or inside WSL.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use hofund_plugin_api::{BinaryName, PluginError, SpawnRequest};

use crate::cli::CONFIG_DIR_VAR;
use crate::wslrun::{self, Tool};

static SKIP_NATIVE_FIND: AtomicBool = AtomicBool::new(false);

/// Test hook: skip native `PATH` lookup so WSL resolution can be exercised.
#[cfg(test)]
pub(crate) fn set_skip_native_find(skip: bool) {
    SKIP_NATIVE_FIND.store(skip, Ordering::Relaxed);
}

/// How hofund runs Claude Code for sign-in and short CLI calls.
#[derive(Clone, Debug)]
pub(crate) enum ClaudeCommand {
    /// A program on this machine.
    Native(PathBuf),
    /// Claude Code inside a running WSL distro.
    Wsl(Tool),
}

impl ClaudeCommand {
    /// Prefers configured `command`, then `PATH`, then WSL on Windows.
    pub(crate) fn resolve(
        command: Option<&PathBuf>,
        name: BinaryName,
    ) -> Result<Self, PluginError> {
        if let Some(path) = command {
            return Ok(Self::Native(path.clone()));
        }
        let skip_native = cfg!(test) && SKIP_NATIVE_FIND.load(Ordering::Relaxed);
        if !skip_native && let Some(path) = hofund_plugin_sdk::process::find_program(name) {
            return Ok(Self::Native(path));
        }
        if let Some(tool) = wslrun::find(name.as_str()) {
            return Ok(Self::Wsl(tool));
        }
        Err(PluginError::config(
            "Claude Code is not installed: `claude` is not on PATH; install it, set `command`, or run it in a WSL distro",
        ))
    }

    /// Builds a spawn for `args` with `CLAUDE_CONFIG_DIR` set to `config_dir`.
    pub(crate) fn spawn_request(
        &self,
        args: &[&str],
        config_dir: &Path,
        extra: &[(OsString, OsString)],
    ) -> Result<SpawnRequest, PluginError> {
        self.spawn_request_in(args, config_dir, config_dir, extra)
    }

    /// Like [`spawn_request`], but runs with a different working directory.
    pub(crate) fn spawn_request_in(
        &self,
        args: &[&str],
        config_dir: &Path,
        cwd: &Path,
        extra: &[(OsString, OsString)],
    ) -> Result<SpawnRequest, PluginError> {
        let mut env = vec![(
            OsString::from(CONFIG_DIR_VAR),
            config_dir.as_os_str().to_owned(),
        )];
        env.extend_from_slice(extra);
        match self {
            Self::Native(program) => SpawnRequest::new(
                program,
                args.iter().map(|arg| OsString::from(*arg)).collect(),
                env,
                cwd,
            ),
            Self::Wsl(tool) => {
                env = wslrun::cross_env(tool, &env, &["CLAUDE_CONFIG_DIR/p"]);
                let wsl_args = wslrun::argv(tool, args);
                SpawnRequest::new(wslrun::wsl_exe(), wsl_args, env, cwd)
            }
        }
    }

    /// Path recorded for native spawns; the WSL binary path inside the distro otherwise.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn record_path(&self) -> PathBuf {
        match self {
            Self::Native(path) => path.clone(),
            Self::Wsl(tool) => PathBuf::from(&tool.path),
        }
    }

    /// WSL distro name when this command runs there.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn wsl_distro(&self) -> Option<&str> {
        match self {
            Self::Native(_) => None,
            Self::Wsl(tool) => Some(&tool.distro),
        }
    }
}
