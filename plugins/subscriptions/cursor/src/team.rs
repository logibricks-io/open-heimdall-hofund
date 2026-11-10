//! §5.4 team id from `cli-config.json`.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hofund_plugin_sdk::json::Value;

use crate::auth;
use crate::tokens;

/// How long a parsed `cli-config.json` is reused in-process.
const TEAM_TTL: Duration = Duration::from_secs(10);

struct Cached {
    loaded_at: Instant,
    auth_info: Option<Value>,
}

static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

/// Decimal team id to send on Run and dashboard calls, when §5.4 matches.
///
/// The header and JSON `teamId` are sent only when `authInfo.authId` is
/// non-empty, equals the access-token JWT `sub`, and `activeTeamId` is a
/// safe integer greater than zero. The cache is process-global and keyed by
/// config path, not by token hash.
pub(crate) fn id_for_token(token: &str) -> Option<String> {
    let info = auth_info()?;
    let auth_id = info
        .get("authId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_owned();
    let sub = tokens::jwt_sub(token)?;
    if auth_id != sub {
        return None;
    }
    let team = positive_int(info.get("activeTeamId")?)?;
    Some(team.to_string())
}

fn auth_info() -> Option<Value> {
    load_for(&auth::default_agent_home().join("cli-config.json"))
}

fn load_for(path: &Path) -> Option<Value> {
    let mut guard = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(cached) = cache.get(path)
        && cached.loaded_at.elapsed() < TEAM_TTL
    {
        return cached.auth_info.clone();
    }
    let previous = cache.get(path).and_then(|cached| cached.auth_info.clone());
    let next = match fs::read_to_string(path) {
        Ok(text) => match hofund_plugin_sdk::json::from_str::<Value>(&text) {
            Ok(value) => Some(value),
            Err(_) => previous,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => previous,
    };
    cache.insert(
        path.to_path_buf(),
        Cached {
            loaded_at: Instant::now(),
            auth_info: next.clone(),
        },
    );
    next
}

fn positive_int(value: &Value) -> Option<u64> {
    let parsed = if let Some(number) = value.as_u64() {
        number
    } else if let Some(number) = value.as_i64() {
        u64::try_from(number).ok()?
    } else {
        value.as_str()?.trim().parse().ok()?
    };
    (parsed > 0).then_some(parsed)
}

#[cfg(test)]
pub(crate) fn clear_cache() {
    *CACHE.lock().unwrap_or_else(|error| error.into_inner()) = None;
}

#[cfg(test)]
pub(crate) fn stale_cache() {
    let mut guard = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    let Some(cache) = guard.as_mut() else {
        return;
    };
    for cached in cache.values_mut() {
        cached.loaded_at = Instant::now()
            .checked_sub(TEAM_TTL + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    const JWT_U1: &str =
        "eyJhbGciOiJub25lIn0.eyJzdWIiOiJ1MSIsImVtYWlsIjoibWVAZXhhbXBsZS5jb20ifQ.sig";

    fn write_config(dir: &Path, body: &str) {
        fs::write(dir.join("cli-config.json"), body).expect("write cli-config.json");
    }

    fn with_dir(body: Option<&str>, f: impl FnOnce()) {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(body) = body {
            write_config(dir.path(), body);
        }
        auth::set_test_config_dir(Some(dir.path().to_path_buf()));
        clear_cache();
        f();
        clear_cache();
        auth::set_test_config_dir(None);
    }

    #[test]
    fn matching_auth_id_and_positive_team_are_sent() {
        with_dir(Some(r#"{"authId":"u1","activeTeamId":42}"#), || {
            assert_eq!(id_for_token(JWT_U1).as_deref(), Some("42"))
        });
    }

    #[test]
    fn mismatch_or_empty_or_zero_or_missing_are_skipped() {
        with_dir(Some(r#"{"authId":"other","activeTeamId":42}"#), || {
            assert_eq!(id_for_token(JWT_U1), None);
        });
        with_dir(Some(r#"{"authId":"","activeTeamId":42}"#), || {
            assert_eq!(id_for_token(JWT_U1), None);
        });
        with_dir(Some(r#"{"authId":"u1","activeTeamId":0}"#), || {
            assert_eq!(id_for_token(JWT_U1), None);
        });
        with_dir(Some(r#"{"authId":"u1"}"#), || {
            assert_eq!(id_for_token(JWT_U1), None);
        });
        with_dir(None, || assert_eq!(id_for_token(JWT_U1), None));
        with_dir(Some(r#"{"authId":"u1","activeTeamId":42}"#), || {
            assert_eq!(id_for_token("not-a-jwt"), None);
        });
    }

    #[test]
    fn parse_failure_keeps_previous_and_enoent_clears() {
        with_dir(Some(r#"{"authId":"u1","activeTeamId":42}"#), || {
            assert_eq!(id_for_token(JWT_U1).as_deref(), Some("42"));
            let dir = auth::default_agent_home();
            fs::write(dir.join("cli-config.json"), "{not-json").expect("corrupt");
            stale_cache();
            assert_eq!(id_for_token(JWT_U1).as_deref(), Some("42"));
            fs::remove_file(dir.join("cli-config.json")).expect("unlink");
            stale_cache();
            assert_eq!(id_for_token(JWT_U1), None);
        });
    }
}
