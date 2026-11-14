//! macOS Keychain storage for Claude Code credentials (`security(1)` via the host).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicBool;

use hofund_plugin_api::{BodyStream, HostContext, PluginError, SpawnRequest};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::timeout;
use unicode_normalization::UnicodeNormalization;

use crate::cli;
use crate::creds::{self, CREDENTIALS_FILE, Credentials};

/// Default keychain service when `CLAUDE_CONFIG_DIR` is not set.
pub(crate) const HOME_SERVICE: &str = "Claude Code-credentials";

/// `security(1)` exit status when the item is absent.
const KEYCHAIN_ABSENT: i32 = 44;

/// How long a keychain subprocess may run.
const KEYCHAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Maximum password bytes read from `security -w`.
const KEYCHAIN_CAP: usize = 1024 * 1024;

/// macOS `security(1)` path on every OS so tests can script the spawn.
const SECURITY: &str = "/usr/bin/security";

#[cfg(test)]
static FORCE_KEYCHAIN: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static SKIP_KEYCHAIN: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
static DISCOVERY_BLOCKED: AtomicBool = AtomicBool::new(false);

/// Whether production code may spawn `security` on this host.
pub(crate) fn keychain_enabled() -> bool {
    #[cfg(test)]
    {
        if SKIP_KEYCHAIN.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        if FORCE_KEYCHAIN.load(std::sync::atomic::Ordering::Relaxed) {
            return true;
        }
    }
    cfg!(target_os = "macos")
}

#[cfg(test)]
pub(crate) fn set_test_force_keychain(enabled: bool) {
    FORCE_KEYCHAIN.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
#[expect(
    dead_code,
    reason = "file-only keychain tests in this slice use force flags"
)]
pub(crate) fn set_test_skip_keychain(enabled: bool) {
    SKIP_KEYCHAIN.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn set_test_discovery_blocked(enabled: bool) {
    DISCOVERY_BLOCKED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// Whether discovery reads must skip keychain probes.
pub(crate) fn discovery_keychain_blocked() -> bool {
    #[cfg(test)]
    if DISCOVERY_BLOCKED.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    matches!(
        std::env::var("HOFUND_NO_ACCOUNTS").ok().as_deref(),
        Some("1")
    )
}

/// Where sign-in material may live after a read or for a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CredentialLocation {
    File(PathBuf),
    Keychain { service: String, account: String },
}

/// Whether to use agent-home or per-config-dir keychain naming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadContext {
    /// `~/.claude` or `CLAUDE_CONFIG_DIR`: follows Claude Code's own home rules.
    AgentHome,
    /// A directory Claude Code was told to use (`CLAUDE_CONFIG_DIR` equals `dir`).
    ConfigDir,
}

/// Keychain item name for a config directory (NFC-normalized path hash).
pub(crate) fn dir_service(dir: &Path) -> String {
    let normalized: String = dir.to_string_lossy().nfc().collect();
    let hash = sha256(normalized.as_bytes());
    let hex = bytes_to_hex(&hash);
    format!("Claude Code-credentials-{}", &hex[..8])
}

/// Service name Claude Code uses for `dir` in the given read context.
pub(crate) fn service_for(dir: &Path, ctx: ReadContext) -> String {
    match ctx {
        ReadContext::ConfigDir => dir_service(dir),
        ReadContext::AgentHome => {
            if cli::configured_dir().is_some() {
                dir_service(dir)
            } else {
                HOME_SERVICE.to_owned()
            }
        }
    }
}

/// Account name Claude Code stores under the service item.
pub(crate) fn keychain_account() -> String {
    let name = std::env::var("USER")
        .ok()
        .filter(|value| keychain_account_valid(value));
    if let Some(name) = name {
        return name;
    }
    "claude-code-user".to_owned()
}

fn keychain_account_valid(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn security_program() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\usr\bin\security")
    } else {
        PathBuf::from(SECURITY)
    }
}

/// Reads sign-in material for `dir`, preferring Keychain on macOS when enabled.
pub(crate) async fn read_sign_in(
    host: &HostContext,
    dir: &Path,
    ctx: ReadContext,
    honor_discovery_off: bool,
) -> Option<(Credentials, CredentialLocation)> {
    if honor_discovery_off && discovery_keychain_blocked() {
        return read_file_only(dir);
    }
    if !keychain_enabled() {
        return read_file_only(dir);
    }
    let service = service_for(dir, ctx);
    let account = keychain_account();
    let loc = || CredentialLocation::Keychain {
        service: service.clone(),
        account: account.clone(),
    };
    let mine = read_keychain_item(host, &service, &account).await;
    if matches!(mine, Some(KeychainItem::Present)) {
        return None;
    }
    if matches!(ctx, ReadContext::ConfigDir) {
        if let Some(KeychainItem::Active(credentials, _)) = mine {
            return Some((credentials, loc()));
        }
        return read_file_only(dir);
    }
    let hex_under_account = match mine {
        Some(KeychainItem::Active(credentials, true)) => Some(credentials),
        Some(KeychainItem::Active(credentials, false)) => {
            return Some((credentials, loc()));
        }
        _ => None,
    };
    if let Some(file) = read_file_only(dir) {
        return Some(file);
    }
    if let Some(credentials) = hex_under_account {
        let _ = write_sign_in(host, dir, &credentials.text, &loc()).await;
        return Some((credentials, loc()));
    }
    if let Some(KeychainItem::Active(credentials, was_hex)) =
        read_keychain_item(host, &service, "").await
    {
        if was_hex {
            let _ = write_sign_in(host, dir, &credentials.text, &loc()).await;
        }
        return Some((credentials, loc()));
    }
    None
}

enum KeychainItem {
    Active(Credentials, bool),
    /// An item is present but holds no usable access token (Claude Code reads it as signed out).
    Present,
}

fn read_file_only(dir: &Path) -> Option<(Credentials, CredentialLocation)> {
    creds::read_credentials(dir).map(|credentials| {
        (
            credentials,
            CredentialLocation::File(dir.join(CREDENTIALS_FILE)),
        )
    })
}

/// Writes sign-in text where `loc` says, creating parent directories for files.
pub(crate) async fn write_sign_in(
    host: &HostContext,
    dir: &Path,
    text: &str,
    loc: &CredentialLocation,
) -> Result<(), PluginError> {
    match loc {
        CredentialLocation::File(path) => {
            if let Some(parent) = path.parent() {
                creds::private_dir(parent)?;
            }
            creds::write_private(path, text)
        }
        CredentialLocation::Keychain { service, account } => {
            if !keychain_enabled() {
                return creds::write_private(&dir.join(CREDENTIALS_FILE), text);
            }
            let compact = compact_json(text)?;
            run_security(
                host,
                &[
                    "add-generic-password",
                    "-U",
                    "-s",
                    service,
                    "-a",
                    account,
                    "-w",
                    &compact,
                ],
                dir,
            )
            .await
            .map(|_| ())
        }
    }
}

/// Removes sign-in material from `dir` (Keychain item and credentials file).
#[expect(
    dead_code,
    reason = "agent sign-in clear path lands in a follow-up slice"
)]
pub(crate) async fn clear_sign_in(
    host: &HostContext,
    dir: &Path,
    ctx: ReadContext,
) -> Result<(), PluginError> {
    if discovery_keychain_blocked() && ctx == ReadContext::AgentHome {
        let _ = std::fs::remove_file(dir.join(CREDENTIALS_FILE));
        return Ok(());
    }
    if keychain_enabled() {
        let service = service_for(dir, ctx);
        let account = keychain_account();
        let _ = run_security(
            host,
            &["delete-generic-password", "-s", &service, "-a", &account],
            dir,
        )
        .await;
    }
    let _ = std::fs::remove_file(dir.join(CREDENTIALS_FILE));
    Ok(())
}

/// Default write location when nothing is live yet (Keychain on macOS).
pub(crate) fn default_write_location(dir: &Path, ctx: ReadContext) -> CredentialLocation {
    if keychain_enabled() {
        CredentialLocation::Keychain {
            service: service_for(dir, ctx),
            account: keychain_account(),
        }
    } else {
        CredentialLocation::File(dir.join(CREDENTIALS_FILE))
    }
}

async fn read_keychain_item(
    host: &HostContext,
    service: &str,
    account: &str,
) -> Option<KeychainItem> {
    let mut args = vec!["find-generic-password", "-s", service, "-w"];
    if !account.is_empty() {
        args.push("-a");
        args.push(account);
    }
    let output = run_security(host, &args, std::env::temp_dir().as_path())
        .await
        .ok()?;
    let (text, was_hex) = decode_keychain_output(output.trim());
    if let Some(credentials) = creds::parse_credentials(&text) {
        return Some(KeychainItem::Active(credentials, was_hex));
    }
    if let Ok(Value::Object(root)) = hofund_plugin_sdk::json::from_str::<Value>(&text)
        && root.get("claudeAiOauth").is_some()
    {
        return Some(KeychainItem::Present);
    }
    None
}

async fn run_security(
    host: &HostContext,
    args: &[&str],
    cwd: &Path,
) -> Result<String, PluginError> {
    let request = SpawnRequest::new(
        security_program(),
        args.iter().map(|arg| OsString::from(*arg)).collect(),
        Vec::new(),
        cwd,
    )?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let mut stdout = child.stdout()?;
    let read = async {
        let body = collect_stdout(&mut stdout).await.unwrap_or_default();
        let code = child.exit_code().await.ok();
        Ok::<_, PluginError>((body, code))
    };
    let (body, code) = match timeout(KEYCHAIN_WAIT, read).await {
        Some(Ok(pair)) => pair,
        Some(Err(error)) => return Err(error),
        None => {
            return Err(PluginError::timeout(
                "reading Claude Code credentials from the keychain timed out",
            ));
        }
    };
    if code == Some(KEYCHAIN_ABSENT) {
        return Err(PluginError::io(
            "Claude Code credentials are not in the keychain",
        ));
    }
    if code.is_some_and(|c| c != 0) {
        return Err(PluginError::io(
            "security could not access Claude Code credentials",
        ));
    }
    Ok(body)
}

async fn collect_stdout(stdout: &mut BodyStream) -> Option<String> {
    let mut out = Vec::new();
    while let Some(result) = stdout.next_chunk().await {
        let chunk = result.ok()?;
        if out.len() + chunk.len() > KEYCHAIN_CAP {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    (!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned())
}

fn decode_keychain_output(output: &str) -> (String, bool) {
    if output.is_empty() || output.starts_with('{') {
        return (output.to_owned(), false);
    }
    if !output.len().is_multiple_of(2) {
        return (output.to_owned(), false);
    }
    let Ok(bytes) = hex_decode(output) else {
        return (output.to_owned(), false);
    };
    if serde_json::from_slice::<Value>(&bytes).is_err() {
        return (output.to_owned(), false);
    }
    (
        String::from_utf8(bytes).unwrap_or_else(|_| output.to_owned()),
        true,
    )
}

fn compact_json(text: &str) -> Result<String, PluginError> {
    let value: Value = hofund_plugin_sdk::json::from_str(text)
        .map_err(|_| PluginError::decode("Claude Code credentials are not JSON"))?;
    serde_json::to_string(&value)
        .map_err(|_| PluginError::decode("Claude Code credentials could not be compacted"))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(text: &str) -> Result<Vec<u8>, ()> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return Err(());
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = from_hex_digit(bytes[i])?;
        let lo = from_hex_digit(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn from_hex_digit(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;
    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{PluginId, PluginKind};

    static KEYCHAIN_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn keychain_test_lock() -> MutexGuard<'static, ()> {
        KEYCHAIN_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn host(dir: &tempfile::TempDir) -> TestHost {
        TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("claude"),
            dir.path(),
        )
    }

    fn security_script(output: &str) -> hofund_kit_subscription::PipedScript {
        hofund_kit_subscription::PipedScript::new()
            .line(output)
            .exits(|_| vec![])
    }

    #[test]
    fn r5_42_dir_service_matches_nfc_sha256_prefix() {
        let path = Path::new("/tmp/claude-config");
        let service = dir_service(path);
        assert!(service.starts_with("Claude Code-credentials-"));
        assert_eq!(service.len(), "Claude Code-credentials-".len() + 8);
    }

    #[test]
    fn r5_43_decode_keychain_output_accepts_hex_json() {
        let json = r#"{"claudeAiOauth":{"accessToken":"live","refreshToken":"r"}}"#;
        let hex = bytes_to_hex(json.as_bytes());
        let (decoded, was_hex) = decode_keychain_output(&hex);
        assert!(was_hex);
        assert_eq!(decoded, json);
    }

    #[test]
    fn r5_44_read_sign_in_prefers_keychain_over_file() {
        let _guard = keychain_test_lock();
        set_test_force_keychain(true);
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("agent-home");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join(CREDENTIALS_FILE),
            r#"{"claudeAiOauth":{"accessToken":"file","refreshToken":"rt-file"}}"#,
        )
        .expect("file");
        let host = host(&dir);
        host.process().script_piped(security_script(
            r#"{"claudeAiOauth":{"accessToken":"kc","refreshToken":"rt-kc"}}"#,
        ));
        let (credentials, loc) = block_on(read_sign_in(
            host.context(),
            &home,
            ReadContext::AgentHome,
            false,
        ))
        .expect("read");
        assert!(matches!(loc, CredentialLocation::Keychain { .. }));
        assert!(credentials.text.contains("kc"));
        set_test_force_keychain(false);
    }

    #[test]
    fn r5_45_discovery_off_skips_keychain_spawn() {
        let _guard = keychain_test_lock();
        set_test_force_keychain(true);
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("agent-home");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join(CREDENTIALS_FILE),
            r#"{"claudeAiOauth":{"accessToken":"file-only","refreshToken":"rt"}}"#,
        )
        .expect("file");
        let host = host(&dir);
        // No security script — a spawn would fail the test host.
        set_test_discovery_blocked(true);
        let read = block_on(read_sign_in(
            host.context(),
            &home,
            ReadContext::AgentHome,
            true,
        ))
        .expect("file fallback");
        set_test_discovery_blocked(false);
        assert!(matches!(read.1, CredentialLocation::File(_)));
        assert!(read.0.text.contains("file-only"));
        set_test_force_keychain(false);
    }

    #[test]
    fn r5_46_hex_keychain_item_rewritten_on_one_line() {
        let _guard = keychain_test_lock();
        set_test_force_keychain(true);
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("agent-home");
        std::fs::create_dir_all(&home).expect("home");
        let json =
            "{\n  \"claudeAiOauth\": {\"accessToken\": \"hexed\", \"refreshToken\": \"r\"}\n}";
        let hex = bytes_to_hex(json.as_bytes());
        let host = host(&dir);
        host.process().script_piped(security_script(&hex));
        host.process()
            .script_piped(hofund_kit_subscription::PipedScript::new().exits(|_| vec![]));
        let (credentials, _) = block_on(read_sign_in(
            host.context(),
            &home,
            ReadContext::AgentHome,
            false,
        ))
        .expect("hex read");
        assert!(credentials.text.contains("hexed"));
        let writes: Vec<_> = host
            .process()
            .piped_spawns()
            .into_iter()
            .filter(|spawn| {
                spawn
                    .arg_strings()
                    .iter()
                    .any(|arg| arg == "add-generic-password")
            })
            .collect();
        assert_eq!(writes.len(), 1, "hex items must be rewritten on read");
        let args = writes[0].arg_strings().join(" ");
        assert!(
            !args.contains('\n'),
            "rewritten keychain payload must be one line"
        );
        set_test_force_keychain(false);
    }

    #[test]
    fn r5_47_config_dir_uses_directory_service_name() {
        let _guard = keychain_test_lock();
        cli::test_configured_dir(Some(PathBuf::from("/custom/claude")));
        let service = service_for(Path::new("/custom/claude"), ReadContext::AgentHome);
        assert_eq!(service, dir_service(Path::new("/custom/claude")));
        cli::test_configured_dir(None);
    }
}
