//! Team id from `cli-config.json` for the `x-cursor-team-id` header.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(not(test))]
use std::sync::{Mutex, OnceLock};

use hofund_plugin_api::SecretString;
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::unverified_claims;

/// Re-read `cli-config.json` at most this often (`TEAM_TTL` in the reference).
pub(crate) const TEAM_TTL: Duration = Duration::from_millis(10_000);

const FILE_NAME: &str = "cli-config.json";
const FILE_CAP: u64 = 256 * 1024;
/// JavaScript `Number.MAX_SAFE_INTEGER` (`2^53 - 1`).
const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthInfo {
    auth_id: String,
    team_id: Option<u64>,
}

struct Cache {
    last: Option<(u64, Option<AuthInfo>)>,
}

enum ReadOutcome {
    /// A successful read, including valid JSON with no `authInfo`.
    Fresh(Option<AuthInfo>),
    /// The file is missing (`ENOENT`). Clears the cached value.
    Missing,
    /// Parse failure or unexpected I/O. Keeps the previous `authInfo`.
    Invalid,
}

/// Decimal `x-cursor-team-id` when section 5.4 says one exists.
pub(crate) fn header_value(token: &SecretString) -> Option<String> {
    team_id(token).map(|id| id.to_string())
}

/// Team id to send, when cached `authId` is non-empty and equals the JWT `sub`.
pub(crate) fn team_id(token: &SecretString) -> Option<u64> {
    let sub = token_sub(token)?;
    let info = current_auth_info()?;
    if info.auth_id.is_empty() || info.auth_id != sub {
        return None;
    }
    info.team_id
}

fn token_sub(token: &SecretString) -> Option<String> {
    let claims = unverified_claims(token).ok()?;
    claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn current_auth_info() -> Option<AuthInfo> {
    let now = now_ms();
    let ttl_ms = u64::try_from(TEAM_TTL.as_millis()).unwrap_or(u64::MAX);
    with_cache(|cache| {
        if let Some((read_at, info)) = cache.last.as_ref()
            && now.saturating_sub(*read_at) < ttl_ms
        {
            return info.clone();
        }
        let outcome = read_auth_info(&cli_config_file());
        let next = match outcome {
            ReadOutcome::Fresh(info) => info,
            ReadOutcome::Missing => None,
            ReadOutcome::Invalid => cache.last.as_ref().and_then(|(_, info)| info.clone()),
        };
        cache.last = Some((now, next.clone()));
        next
    })
}

fn read_auth_info(path: &Path) -> ReadOutcome {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ReadOutcome::Missing;
        }
        Err(_) => return ReadOutcome::Invalid,
        Ok(meta) if !meta.is_file() || meta.len() > FILE_CAP => return ReadOutcome::Invalid,
        Ok(_) => {}
    }
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_auth_info(&text) {
            Some(info) => ReadOutcome::Fresh(info),
            None => ReadOutcome::Invalid,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ReadOutcome::Missing,
        Err(_) => ReadOutcome::Invalid,
    }
}

/// `Some` on a successful JSON parse; the inner value is `None` when `authInfo` is absent.
/// `None` is a parse failure.
fn parse_auth_info(text: &str) -> Option<Option<AuthInfo>> {
    let root: Value = hofund_plugin_sdk::json::from_str(text).ok()?;
    let Some(auth) = root.get("authInfo") else {
        return Some(None);
    };
    if auth.is_null() || !auth.is_object() {
        return Some(None);
    }
    let auth_id = auth
        .get("authId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    Some(Some(AuthInfo {
        auth_id,
        team_id: team_of(auth.get("activeTeamId")),
    }))
}

fn team_of(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    let id = value.as_u64()?;
    (id > 0 && id <= MAX_SAFE_INTEGER).then_some(id)
}

/// Path to `cli-config.json` (design §5.4). This is not [`crate::auth`]'s
/// `auth_file_dir()`, which uses `APPDATA` on Windows and `~/.config/cursor` on
/// Linux when `XDG_CONFIG_HOME` is unset.
pub(crate) fn cli_config_file() -> PathBuf {
    #[cfg(test)]
    if crate::auth::configured_dir().is_none() {
        return isolated_missing_config();
    }
    cli_config_file_from(crate::auth::configured_dir(), xdg_config_home(), home_dir())
}

/// Resolves the config path from explicit roots. Empty paths are treated as unset.
pub(crate) fn cli_config_file_from(
    cursor_config_dir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: PathBuf,
) -> PathBuf {
    let dir = if let Some(dir) = cursor_config_dir.filter(|path| !path.as_os_str().is_empty()) {
        dir
    } else if let Some(xdg) = xdg_config_home.filter(|path| !path.as_os_str().is_empty()) {
        xdg.join("cursor")
    } else {
        home.join(".cursor")
    };
    dir.join(FILE_NAME)
}

fn xdg_config_home() -> Option<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn now_ms() -> u64 {
    #[cfg(test)]
    if let Some(ms) = test_now_ms() {
        return ms;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|spent| u64::try_from(spent.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn with_cache<R>(func: impl FnOnce(&mut Cache) -> R) -> R {
    #[cfg(test)]
    {
        TEST_CACHE.with(|cell| func(&mut cell.borrow_mut()))
    }
    #[cfg(not(test))]
    {
        let mutex = CACHE.get_or_init(|| Mutex::new(Cache { last: None }));
        let mut guard = mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        func(&mut guard)
    }
}

#[cfg(not(test))]
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_CACHE: std::cell::RefCell<Cache> = const { std::cell::RefCell::new(Cache { last: None }) };
    static TEST_NOW_MS: std::cell::RefCell<Option<u64>> = const { std::cell::RefCell::new(None) };
}

/// A path that does not exist, so tests never read the operator's `cli-config.json`.
#[cfg(test)]
fn isolated_missing_config() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"C:\hofund-test-missing-cursor-config\cli-config.json")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/var/empty/hofund-test-missing-cursor-config/cli-config.json")
    }
}

#[cfg(test)]
fn test_now_ms() -> Option<u64> {
    TEST_NOW_MS.with(|cell| *cell.borrow())
}

/// Pins the team-cache clock in tests. `None` restores the wall clock.
#[cfg(test)]
pub(crate) fn set_test_now_ms(ms: Option<u64>) {
    TEST_NOW_MS.with(|cell| *cell.borrow_mut() = ms);
}

/// Clears the per-thread test cache and clock.
#[cfg(test)]
pub(crate) fn reset_test_state() {
    TEST_CACHE.with(|cell| cell.borrow_mut().last = None);
    set_test_now_ms(None);
}

/// Sets `CURSOR_CONFIG_DIR` for this thread and resets the team cache. Restored on drop.
#[cfg(test)]
pub(crate) fn begin_test(config_dir: Option<PathBuf>) -> TeamTestGuard {
    reset_test_state();
    crate::auth::set_test_config_dir(config_dir);
    TeamTestGuard
}

/// Restores team-test isolation when dropped.
#[cfg(test)]
pub(crate) struct TeamTestGuard;

#[cfg(test)]
impl Drop for TeamTestGuard {
    fn drop(&mut self) {
        crate::auth::set_test_config_dir(None);
        reset_test_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_api::SecretString;

    fn b64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
            let count = chunk.len() + 1;
            for i in 0..count {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            }
        }
        out
    }

    fn jwt(sub: &str) -> SecretString {
        let claims = format!(r#"{{"email":"user@example.test","sub":"{sub}","exp":4000000000}}"#);
        SecretString::from(format!("e30.{}.sig", b64url(claims.as_bytes())))
    }

    fn write_config(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).expect("dir");
        std::fs::write(dir.join(FILE_NAME), body).expect("cli-config");
    }

    fn header_for(dir: &Path, body: &str, sub: &str) -> Option<String> {
        write_config(dir, body);
        let _guard = begin_test(Some(dir.to_path_buf()));
        set_test_now_ms(Some(1_000));
        header_value(&jwt(sub))
    }

    #[test]
    fn cursor_config_dir_wins_over_xdg_and_home() {
        let path = cli_config_file_from(
            Some(PathBuf::from("override")),
            Some(PathBuf::from("xdg")),
            PathBuf::from("home"),
        );
        assert_eq!(path, PathBuf::from("override").join(FILE_NAME));
    }

    #[test]
    fn empty_cursor_config_dir_falls_through_to_xdg() {
        let path = cli_config_file_from(
            Some(PathBuf::new()),
            Some(PathBuf::from("xdg")),
            PathBuf::from("home"),
        );
        assert_eq!(path, PathBuf::from("xdg").join("cursor").join(FILE_NAME));
    }

    #[test]
    fn xdg_config_home_uses_cursor_child() {
        let path = cli_config_file_from(None, Some(PathBuf::from("xdg")), PathBuf::from("home"));
        assert_eq!(path, PathBuf::from("xdg").join("cursor").join(FILE_NAME));
    }

    #[test]
    fn empty_xdg_falls_through_to_home_cursor() {
        let path = cli_config_file_from(None, Some(PathBuf::new()), PathBuf::from("home"));
        assert_eq!(path, PathBuf::from("home").join(".cursor").join(FILE_NAME));
    }

    #[test]
    fn default_path_is_home_dot_cursor_on_every_os() {
        let path = cli_config_file_from(None, None, PathBuf::from("home"));
        assert_eq!(path, PathBuf::from("home").join(".cursor").join(FILE_NAME));
        assert!(
            !path
                .components()
                .any(|part| part.as_os_str() == "AppData" || part.as_os_str() == "config"),
            "team path must not use APPDATA or ~/.config/cursor, got {}",
            path.display()
        );
    }

    #[test]
    fn team_id_is_a_safe_integer_greater_than_zero() {
        assert_eq!(team_of(None), None);
        assert_eq!(team_of(Some(&Value::from(0_u64))), None);
        assert_eq!(team_of(Some(&Value::from(1_u64))), Some(1));
        assert_eq!(
            team_of(Some(&Value::from(MAX_SAFE_INTEGER))),
            Some(MAX_SAFE_INTEGER)
        );
        assert_eq!(team_of(Some(&Value::from(MAX_SAFE_INTEGER + 1))), None);
        assert_eq!(team_of(Some(&Value::from(-1_i64))), None);
        assert_eq!(team_of(Some(&serde_json_number(1.5))), None);
        assert_eq!(team_of(Some(&Value::from("12"))), None);
    }

    fn serde_json_number(value: f64) -> Value {
        hofund_plugin_sdk::json::from_str(&value.to_string()).expect("number")
    }

    #[test]
    fn header_sent_only_when_auth_id_matches_sub() {
        let dir = tempfile::tempdir().expect("dir");
        let body = r#"{"authInfo":{"authId":"auth0|user","activeTeamId":42}}"#;
        assert_eq!(
            header_for(dir.path(), body, "auth0|user").as_deref(),
            Some("42")
        );
        assert_eq!(header_for(dir.path(), body, "auth0|other"), None);
        assert_eq!(
            header_for(
                dir.path(),
                r#"{"authInfo":{"authId":"","activeTeamId":42}}"#,
                "auth0|user"
            ),
            None
        );
    }

    #[test]
    fn header_is_omitted_for_zero_or_non_integer_team_id() {
        let dir = tempfile::tempdir().expect("dir");
        let cases = [
            r#"{"authInfo":{"authId":"auth0|user","activeTeamId":0}}"#,
            r#"{"authInfo":{"authId":"auth0|user","activeTeamId":1.5}}"#,
            r#"{"authInfo":{"authId":"auth0|user","activeTeamId":"12"}}"#,
            r#"{"authInfo":{"authId":"auth0|user","activeTeamId":9007199254740992}}"#,
            r#"{"authInfo":{"authId":"auth0|user"}}"#,
        ];
        for body in cases {
            assert_eq!(header_for(dir.path(), body, "auth0|user"), None, "{body}");
        }
        let ok = r#"{"authInfo":{"authId":"auth0|user","activeTeamId":9007199254740991}}"#;
        assert_eq!(
            header_for(dir.path(), ok, "auth0|user").as_deref(),
            Some("9007199254740991")
        );
    }

    #[test]
    fn cache_is_not_keyed_by_token_and_reuses_one_file_read() {
        let dir = tempfile::tempdir().expect("dir");
        write_config(
            dir.path(),
            r#"{"authInfo":{"authId":"auth0|user","activeTeamId":42}}"#,
        );
        let _guard = begin_test(Some(dir.path().to_path_buf()));
        set_test_now_ms(Some(1_000));
        assert_eq!(header_value(&jwt("auth0|user")).as_deref(), Some("42"));
        assert_eq!(header_value(&jwt("auth0|other")), None);
        write_config(
            dir.path(),
            r#"{"authInfo":{"authId":"auth0|user","activeTeamId":99}}"#,
        );
        set_test_now_ms(Some(2_000));
        assert_eq!(
            header_value(&jwt("auth0|user")).as_deref(),
            Some("42"),
            "TTL has not elapsed"
        );
        set_test_now_ms(Some(11_000));
        assert_eq!(header_value(&jwt("auth0|user")).as_deref(), Some("99"));
    }

    #[test]
    fn parse_failure_keeps_previous_auth_info_and_enoent_clears_it() {
        let dir = tempfile::tempdir().expect("dir");
        write_config(
            dir.path(),
            r#"{"authInfo":{"authId":"auth0|user","activeTeamId":42}}"#,
        );
        let _guard = begin_test(Some(dir.path().to_path_buf()));
        set_test_now_ms(Some(1_000));
        assert_eq!(header_value(&jwt("auth0|user")).as_deref(), Some("42"));

        std::fs::write(dir.path().join(FILE_NAME), "not-json").expect("corrupt");
        set_test_now_ms(Some(12_000));
        assert_eq!(
            header_value(&jwt("auth0|user")).as_deref(),
            Some("42"),
            "parse failure keeps the previous authInfo"
        );

        std::fs::remove_file(dir.path().join(FILE_NAME)).expect("remove");
        set_test_now_ms(Some(23_000));
        assert_eq!(
            header_value(&jwt("auth0|user")),
            None,
            "ENOENT clears the cached authInfo"
        );
    }

    #[test]
    fn tests_do_not_read_the_real_home_config() {
        let _guard = begin_test(None);
        set_test_now_ms(Some(1_000));
        assert_eq!(header_value(&jwt("auth0|user")), None);
        assert_eq!(cli_config_file(), isolated_missing_config());
    }
}
