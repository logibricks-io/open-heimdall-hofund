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

static KEYCHAIN: Mutex<KeychainCache> = Mutex::new(KeychainCache {
    token: String::new(),
    read_at: None,
});

struct KeychainCache {
    token: String,
    read_at: Option<Instant>,
}

/// Whether tests skip macOS keychain reads.
pub(crate) fn skip_keychain() -> bool {
    #[cfg(test)]
    {
        if SKIP_KEYCHAIN.with(|slot| *slot.borrow()) {
            return true;
        }
    }
    std::env::var("HOFUND_CURSOR_SKIP_KEYCHAIN")
        .ok()
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
thread_local! {
    static SKIP_KEYCHAIN: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
}

#[cfg(test)]
pub(crate) fn test_skip_keychain(skip: bool) {
    SKIP_KEYCHAIN.with(|slot| *slot.borrow_mut() = skip);
}

/// Reads cursor-agent's token from the macOS keychain when allowed.
pub(crate) async fn keychain_token(host: &HostContext, fresh: bool) -> Result<String, PluginError> {
    if !cfg!(target_os = "macos") || skip_keychain() {
        return Ok(String::new());
    }
    {
        let cache = KEYCHAIN.lock().unwrap_or_else(|poison| poison.into_inner());
        if !fresh
            && let Some(at) = cache.read_at
            && at.elapsed() < KEYCHAIN_TTL
        {
            return Ok(cache.token.clone());
        }
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
    let mut cache = KEYCHAIN.lock().unwrap_or_else(|poison| poison.into_inner());
    cache.read_at = Some(Instant::now());
    cache.token = token;
    Ok(cache.token.clone())
}

/// Live access token: keychain on macOS, else auth.json.
pub(crate) async fn live_access(
    host: &HostContext,
    agent_home: &Path,
    fresh: bool,
) -> Result<Option<String>, PluginError> {
    let keychain = keychain_token(host, fresh).await?;
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
    let mut env = vec![(
        OsString::from("CURSOR_CONFIG_DIR"),
        dir.as_os_str().to_owned(),
    )];
    if let Some(configured) = auth::configured_dir() {
        env[0].1 = configured.into_os_string();
    }
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
