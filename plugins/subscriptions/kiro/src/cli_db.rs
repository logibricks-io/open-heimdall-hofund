//! kiro-cli SQLite sign-in import and refresh write-back.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use hofund_plugin_api::{PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{same_secret, secret_text};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _};

use crate::creds::Credential;

const BUILDER_ID_START: &str = "https://view.awsapps.com/start";

pub(crate) fn default_path() -> Result<PathBuf, PluginError> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let userprofile = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let platform = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    path_for(
        platform,
        appdata.as_deref(),
        home.as_deref(),
        userprofile.as_deref(),
    )
    .ok_or_else(|| PluginError::io("cannot resolve the user home for kiro-cli"))
}

fn path_for(
    platform: &str,
    appdata: Option<&Path>,
    home: Option<&Path>,
    userprofile: Option<&Path>,
) -> Option<PathBuf> {
    if platform == "windows" {
        let base = appdata.map(Path::to_path_buf).or_else(|| {
            userprofile
                .or(home)
                .map(|home| home.join("AppData/Roaming"))
        })?;
        return Some(base.join("kiro-cli/data.sqlite3"));
    }
    let home = home.or(userprofile)?;
    if platform == "macos" {
        Some(home.join("Library/Application Support/kiro-cli/data.sqlite3"))
    } else {
        Some(home.join(".local/share/kiro-cli/data.sqlite3"))
    }
}

pub(crate) fn read_at(path: &Path) -> Result<Option<Credential>, PluginError> {
    if !path.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| PluginError::io("cannot open the kiro-cli sign-in database"))?;
    for (kind, key) in [
        ("social", "kirocli:social:token"),
        ("odic", "kirocli:odic:token"),
        ("external-idp", "kirocli:external-idp:token"),
    ] {
        let Some(token) = get(&connection, key) else {
            continue;
        };
        let Some(access) = text(&token, "access_token").filter(|token| !token.is_empty()) else {
            continue;
        };
        let refresh = text(&token, "refresh_token").unwrap_or("");
        let expires_at =
            text(&token, "expires_at").and_then(hofund_plugin_sdk::time::parse_rfc3339);
        let region = text(&token, "region")
            .filter(|region| !region.is_empty())
            .unwrap_or("us-east-1")
            .to_owned();
        let profile_arn = text(&token, "profile_arn").unwrap_or("").to_owned();
        let mut credential = Credential {
            access: SecretString::from(access.to_owned()),
            refresh: SecretString::from(refresh.to_owned()),
            expires_at,
            region,
            profile_arn,
            label: "kiro-cli account".to_owned(),
            method: kind.to_owned(),
            provider: String::new(),
            client_id: String::new(),
            client_secret: SecretString::from(String::new()),
            client_id_hash: String::new(),
            token_url: String::new(),
            ide_token_path: String::new(),
            db_key: key.to_owned(),
            cli_db_path: path.to_string_lossy().into_owned(),
        };
        match kind {
            "odic" => {
                credential.method = "idc".to_owned();
                let start_url = text(&token, "start_url").unwrap_or("");
                credential.provider = if start_url.is_empty() || start_url == BUILDER_ID_START {
                    "BuilderId"
                } else {
                    "Enterprise"
                }
                .to_owned();
                if let Some(registration) = get(&connection, "kirocli:odic:device-registration") {
                    credential.client_id =
                        text(&registration, "client_id").unwrap_or("").to_owned();
                    credential.client_secret = SecretString::from(
                        text(&registration, "client_secret")
                            .unwrap_or("")
                            .to_owned(),
                    );
                }
            }
            "external-idp" => {
                credential.client_id = text(&token, "client_id").unwrap_or("").to_owned();
                credential.token_url = text(&token, "token_endpoint")
                    .filter(|url| !url.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        text(&token, "issuer_url")
                            .filter(|issuer| !issuer.is_empty())
                            .map(|issuer| format!("{}/v1/token", issuer.trim_end_matches('/')))
                    })
                    .unwrap_or_default();
            }
            _ => {}
        }
        return Ok(Some(credential));
    }
    Ok(None)
}

fn get(connection: &Connection, key: &str) -> Option<Value> {
    let text = connection
        .query_row("SELECT value FROM auth_kv WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()?;
    json::from_str(&text).ok()
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

pub(crate) fn write_refresh(
    path: &Path,
    key: &str,
    credential: &Credential,
    refresh_used: &SecretString,
) -> Result<(), PluginError> {
    let connection = Connection::open(path)
        .map_err(|_| PluginError::io("cannot open the kiro-cli sign-in database for writing"))?;
    connection
        .busy_timeout(std::time::Duration::from_secs(3))
        .map_err(|_| PluginError::io("cannot configure the kiro-cli sign-in database"))?;
    let Some(current) = get(&connection, key) else {
        return Ok(());
    };
    let current_refresh =
        SecretString::from(text(&current, "refresh_token").unwrap_or("").to_owned());
    if !same_secret(&current_refresh, refresh_used) {
        return Ok(());
    }
    let Value::Object(mut object) = current else {
        return Ok(());
    };
    object.insert(
        "access_token".to_owned(),
        Value::String(secret_text(&credential.access).to_owned()),
    );
    object.insert(
        "refresh_token".to_owned(),
        Value::String(secret_text(&credential.refresh).to_owned()),
    );
    let expires = credential
        .expires_at
        .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
        .map(|duration| hofund_plugin_sdk::time::format_rfc3339(UNIX_EPOCH + duration))
        .unwrap_or_default();
    object.insert("expires_at".to_owned(), Value::String(expires));
    if credential.method == "social" && !credential.profile_arn.is_empty() {
        object.insert(
            "profile_arn".to_owned(),
            Value::String(credential.profile_arn.clone()),
        );
    }
    let encoded = json::to_string(&Value::Object(object))
        .map_err(|_| PluginError::io("cannot encode the kiro-cli sign-in"))?;
    connection
        .execute(
            "UPDATE auth_kv SET value = ?1 WHERE key = ?2",
            (&encoded, key),
        )
        .map_err(|_| PluginError::io("cannot write the kiro-cli sign-in database"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    fn fixture(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("data.sqlite3");
        let connection = Connection::open(&path).expect("fixture database");
        connection
            .execute_batch("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)")
            .expect("fixture schema");
        path
    }

    fn insert(path: &Path, key: &str, value: Value) {
        let connection = Connection::open(path).expect("fixture database");
        connection
            .execute(
                "INSERT INTO auth_kv (key, value) VALUES (?1, ?2)",
                (key, json::to_string(&value).expect("JSON")),
            )
            .expect("fixture row");
    }

    #[test]
    fn r51b_cli_paths_match_each_platform_and_home_fallbacks() {
        let home = Path::new("/fixture/home");
        let profile = Path::new("/fixture/profile");
        let appdata = Path::new("/fixture/roaming");
        assert_eq!(
            path_for("windows", Some(appdata), Some(home), Some(profile)),
            Some(appdata.join("kiro-cli/data.sqlite3"))
        );
        assert_eq!(
            path_for("windows", None, Some(home), Some(profile)),
            Some(profile.join("AppData/Roaming/kiro-cli/data.sqlite3"))
        );
        assert_eq!(
            path_for("macos", None, Some(home), None),
            Some(home.join("Library/Application Support/kiro-cli/data.sqlite3"))
        );
        assert_eq!(
            path_for("linux", None, None, Some(profile)),
            Some(profile.join(".local/share/kiro-cli/data.sqlite3"))
        );
    }

    #[test]
    fn r51b_cli_import_reads_social_oidc_and_external_idp() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let path = fixture(&dir);
        insert(
            &path,
            "kirocli:social:token",
            json::json!({"access_token":"social-a","refresh_token":"social-r","expires_at":"2099-01-01T00:00:00Z","profile_arn":"profile/s"}),
        );
        let social = read_at(&path).expect("read").expect("social");
        assert_eq!(social.method, "social");
        assert_eq!(social.db_key, "kirocli:social:token");
        assert_eq!(social.cli_db_path, path.to_string_lossy());

        let odic_path = dir.path().join("odic.sqlite3");
        Connection::open(&odic_path)
            .expect("odic fixture")
            .execute_batch("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)")
            .expect("odic schema");
        insert(
            &odic_path,
            "kirocli:odic:token",
            json::json!({"access_token":"idc-a","refresh_token":"idc-r","start_url":BUILDER_ID_START}),
        );
        insert(
            &odic_path,
            "kirocli:odic:device-registration",
            json::json!({"client_id":"client","client_secret":"secret"}),
        );
        let idc = read_at(&odic_path).expect("read").expect("idc");
        assert_eq!(idc.method, "idc");
        assert_eq!(idc.provider, "BuilderId");
        assert_eq!(idc.client_id, "client");

        let external_path = dir.path().join("external.sqlite3");
        Connection::open(&external_path)
            .expect("external fixture")
            .execute_batch("CREATE TABLE auth_kv (key TEXT PRIMARY KEY, value TEXT)")
            .expect("external schema");
        insert(
            &external_path,
            "kirocli:external-idp:token",
            json::json!({"access_token":"external-a","refresh_token":"external-r","client_id":"external-client","issuer_url":"https://idp.example.test/"}),
        );
        let external = read_at(&external_path).expect("read").expect("external");
        assert_eq!(external.method, "external-idp");
        assert_eq!(external.token_url, "https://idp.example.test/v1/token");
    }

    #[test]
    fn r51b_cli_write_back_preserves_fields_and_rejects_rotated_source() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let path = fixture(&dir);
        let key = "kirocli:social:token";
        insert(
            &path,
            key,
            json::json!({"access_token":"old-a","refresh_token":"old-r","expires_at":"1970-01-01T00:00:01Z","custom":"kept"}),
        );
        let mut credential = read_at(&path).expect("read").expect("credential");
        credential.access = SecretString::from("new-a".to_owned());
        credential.refresh = SecretString::from("new-r".to_owned());
        credential.profile_arn = "profile/new".to_owned();
        credential.expires_at = Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        write_refresh(
            &path,
            key,
            &credential,
            &SecretString::from("old-r".to_owned()),
        )
        .expect("write");
        let connection = Connection::open(&path).expect("read fixture");
        let updated = get(&connection, key).expect("updated row");
        assert_eq!(text(&updated, "access_token"), Some("new-a"));
        assert_eq!(text(&updated, "refresh_token"), Some("new-r"));
        assert_eq!(text(&updated, "custom"), Some("kept"));
        assert_eq!(text(&updated, "profile_arn"), Some("profile/new"));

        credential.access = SecretString::from("must-not-win".to_owned());
        write_refresh(
            &path,
            key,
            &credential,
            &SecretString::from("stale-r".to_owned()),
        )
        .expect("stale write is a no-op");
        let unchanged = get(&connection, key).expect("unchanged row");
        assert_eq!(text(&unchanged, "access_token"), Some("new-a"));
    }
}
