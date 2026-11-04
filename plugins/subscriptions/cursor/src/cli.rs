//! Running cursor-agent and reading macOS keychain tokens.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hofund_plugin_api::{BodyStream, HostContext, PluginError, SpawnRequest};

use crate::auth;

/// The CLI cursor-agent installs.
pub(crate) const COMMAND: &str = "cursor-agent";

/// How long `cursor-agent status` may run.
pub(crate) const STATUS_WAIT: Duration = Duration::from_secs(30);

/// How long `cursor-agent models` may run.
pub(crate) const MODELS_WAIT: Duration = Duration::from_secs(30);

/// How long a keychain read is reused.
const KEYCHAIN_TTL: Duration = Duration::from_secs(10);

/// Cached macOS keychain token reads for one subscription instance.
#[derive(Default)]
pub(crate) struct KeychainCache {
    token: String,
    read_at: Option<Instant>,
}

impl KeychainCache {
    fn cached_token(&self) -> Option<String> {
        let at = self.read_at?;
        if at.elapsed() < KEYCHAIN_TTL {
            Some(self.token.clone())
        } else {
            None
        }
    }

    fn store(&mut self, token: String) {
        self.read_at = Some(Instant::now());
        self.token = token;
    }
}

/// Whether macOS keychain reads are skipped (tests or `HOFUND_CURSOR_SKIP_KEYCHAIN`).
pub(crate) fn skip_keychain() -> bool {
    std::env::var("HOFUND_CURSOR_SKIP_KEYCHAIN")
        .ok()
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// Reads cursor-agent's token from the macOS keychain when allowed.
pub(crate) async fn keychain_token(
    host: &HostContext,
    cache: &Mutex<KeychainCache>,
    fresh: bool,
) -> Result<String, PluginError> {
    if !cfg!(target_os = "macos") || skip_keychain() {
        return Ok(String::new());
    }
    if !fresh
        && let Ok(guard) = cache.lock()
        && let Some(token) = guard.cached_token()
    {
        return Ok(token);
    }
    let request = SpawnRequest::new(
        PathBuf::from("security"),
        [
            "find-generic-password",
            "-s",
            "cursor-access-token",
            "-a",
            "cursor-user",
            "-w",
        ]
        .into_iter()
        .map(OsString::from)
        .collect(),
        vec![],
        std::env::temp_dir(),
    )?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let mut stdout = child.stdout()?;
    let token = collect(&mut stdout, 4096)
        .await
        .unwrap_or_default()
        .trim()
        .to_owned();
    drop(child);
    if let Ok(mut guard) = cache.lock() {
        guard.store(token.clone());
    }
    Ok(token)
}

/// Live access token: keychain on macOS, else auth.json.
pub(crate) async fn live_access(
    host: &HostContext,
    agent_home: &Path,
    fresh: bool,
    cache: &Mutex<KeychainCache>,
) -> Result<Option<String>, PluginError> {
    let keychain = keychain_token(host, cache, fresh).await?;
    if !keychain.is_empty() {
        return Ok(Some(keychain));
    }
    Ok(auth::read_agent_access(agent_home))
}

/// Spawn request for `cursor-agent` with `args` in `dir`.
pub(crate) fn request(
    program: &Path,
    args: &[&str],
    dir: &Path,
) -> Result<SpawnRequest, PluginError> {
    let env = vec![(
        OsString::from("CURSOR_CONFIG_DIR"),
        dir.as_os_str().to_owned(),
    )];
    SpawnRequest::new(program, args.iter().map(OsString::from).collect(), env, dir)
}

/// Collects stdout until the child exits or the cap is hit.
pub(crate) async fn collect(stdout: &mut BodyStream, cap: usize) -> Option<String> {
    let mut out = Vec::new();
    while let Some(result) = stdout.next_chunk().await {
        let Ok(chunk) = result else {
            break;
        };
        if out.len() + chunk.len() > cap {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    (!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned())
}

/// Resolves the cursor-agent program: config override or PATH.
pub(crate) fn program(configured: Option<&PathBuf>) -> Result<PathBuf, PluginError> {
    if let Some(path) = configured {
        return Ok(path.clone());
    }
    hofund_plugin_sdk::process::find_program(hofund_plugin_api::BinaryName::new(COMMAND))
        .ok_or_else(|| {
            PluginError::config(
                "cursor-agent is not installed: it is not on PATH; install it or set `command`",
            )
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_ansi_colour_codes() {
        assert_eq!(
            strip_ansi("\u{1b}[4mhttps://example.test/x\u{1b}[24m"),
            "https://example.test/x"
        );
    }

    #[allow(clippy::while_let_on_iterator)]
    fn strip_ansi(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                while let Some(next) = chars.next() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }
}
