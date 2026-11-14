//! WorkBuddy desktop auth and stored secrets.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, secret_text};

use crate::site::Site;

#[derive(Clone, Debug)]
pub(crate) struct Credential {
    pub uid: String,
    pub access: SecretString,
    pub refresh: SecretString,
    pub expires_at: i64,
    pub refresh_expires_at: i64,
    pub domain: String,
    pub token_type: String,
    pub label: String,
    pub site: Site,
    pub desktop: bool,
    pub api_key: bool,
    pub enterprise_id: String,
    pub auth_path: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub credential: Credential,
}

impl Stored {
    pub(crate) async fn load(host: &HostContext, account: &AccountId) -> Result<Self, PluginError> {
        let Some(secret) = host.accounts().secret(account).await? else {
            return Err(PluginError::auth(
                "no WorkBuddy sign-in is stored for this account",
            ));
        };
        let mut stored = parse_secret(&secret)?;
        if stored.credential.desktop {
            stored.credential = hydrate_desktop(host, &stored.credential)?;
        }
        Ok(stored)
    }

    pub(crate) fn fresh_at(&self, now: SystemTime) -> bool {
        if self.credential.expires_at <= 0 {
            return !self.credential.desktop;
        }
        let Ok(now_ms) = now.duration_since(UNIX_EPOCH) else {
            return false;
        };
        now_ms.as_millis() as i64 + 60_000 < self.credential.expires_at
    }
}

fn hydrate_desktop(host: &HostContext, stored: &Credential) -> Result<Credential, PluginError> {
    if stored.auth_path.is_empty() {
        return Err(PluginError::auth(
            "the stored WorkBuddy desktop sign-in has no auth file path",
        ));
    }
    let path = Path::new(&stored.auth_path);
    let text = std::fs::read_to_string(path).map_err(|error| {
        PluginError::auth(format!(
            "WorkBuddy desktop auth at {} can't be read ({error})",
            path.display()
        ))
    })?;
    let now_ms = host
        .clock()
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let mut fresh = parse_desktop_info(&text, stored.site, path, now_ms)?;
    if fresh.uid != stored.uid {
        return Err(PluginError::auth(
            "WorkBuddy desktop is signed in to a different account than the one stored here",
        ));
    }
    fresh.label = stored.label.clone();
    fresh.auth_path = stored.auth_path.clone();
    Ok(fresh)
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<Stored, PluginError> {
    let blob = SecretObject::parse(secret)?;
    let uid = blob
        .text("uid")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("the stored WorkBuddy sign-in has no uid"))?;
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored WorkBuddy sign-in has no access token"))?;
    let site = match blob.text("site").unwrap_or("workbuddy") {
        "workbuddy-ai" => Site::International,
        _ => Site::China,
    };
    Ok(Stored {
        credential: Credential {
            uid: uid.to_owned(),
            access,
            refresh: blob
                .secret("refresh_token")
                .unwrap_or_else(|| SecretString::from(String::new())),
            expires_at: blob.number("expires_at").unwrap_or(0) as i64,
            refresh_expires_at: blob.number("refresh_expires_at").unwrap_or(0) as i64,
            domain: blob.text("domain").unwrap_or("").to_owned(),
            token_type: blob.text("token_type").unwrap_or("Bearer").to_owned(),
            label: blob.text("label").unwrap_or(uid).to_owned(),
            site,
            desktop: blob.flag("desktop"),
            api_key: blob.flag("api_key"),
            enterprise_id: blob.text("enterprise_id").unwrap_or("").to_owned(),
            auth_path: blob.text("auth_path").unwrap_or("").to_owned(),
        },
    })
}

pub(crate) fn secret_blob(credential: &Credential) -> SecretObject {
    SecretObject::new()
        .with_number("v", 1)
        .with_text("uid", &credential.uid)
        .with_secret("access_token", &credential.access)
        .with_secret("refresh_token", &credential.refresh)
        .with_number("expires_at", credential.expires_at.max(0) as u64)
        .with_number(
            "refresh_expires_at",
            credential.refresh_expires_at.max(0) as u64,
        )
        .with_text("domain", &credential.domain)
        .with_text("token_type", &credential.token_type)
        .with_text("label", &credential.label)
        .with_text("site", credential.site.plugin_id())
        .with_text("auth_path", &credential.auth_path)
        .with_text("enterprise_id", &credential.enterprise_id)
        .with_flag("desktop", credential.desktop)
        .with_flag("api_key", credential.api_key)
}

pub(crate) fn account_id(credential: &Credential) -> AccountId {
    if credential.enterprise_id.is_empty() {
        account_id_from(&[credential.site.plugin_id(), &credential.uid])
    } else {
        account_id_from(&[
            credential.site.plugin_id(),
            &credential.uid,
            &credential.enterprise_id,
        ])
    }
}

pub(crate) fn parse_desktop_info(
    text: &str,
    site: Site,
    auth_path: &Path,
    now_ms: i64,
) -> Result<Credential, PluginError> {
    let root: Value = json::from_str(text.trim())
        .map_err(|_| PluginError::auth("WorkBuddy auth file is invalid JSON"))?;
    let account = root.get("account").and_then(Value::as_object);
    let auth = root.get("auth").and_then(Value::as_object);
    if auth.is_some_and(|map| {
        ["accessToken", "refreshToken"]
            .into_iter()
            .filter_map(|key| map.get(key))
            .any(encrypted_value)
    }) {
        return Err(PluginError::auth(
            "WorkBuddy desktop keeps its sign-in encrypted (WorkBuddy 5.6 and later), which hofund can't read; use browser sign-in instead",
        ));
    }
    let uid = account
        .and_then(|map| map.get("uid"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PluginError::auth("WorkBuddy auth file has no account uid"))?;
    let access = auth
        .and_then(|map| map.get("accessToken"))
        .and_then(string_value)
        .ok_or_else(|| PluginError::auth("WorkBuddy auth file has no access token"))?;
    let refresh = auth
        .and_then(|map| map.get("refreshToken"))
        .and_then(string_value)
        .unwrap_or_default();
    let mut expires_at = auth
        .and_then(|map| map.get("expiresAt"))
        .and_then(json_i64)
        .unwrap_or(0);
    if expires_at == 0
        && let Some(expires_in) = auth.and_then(|map| map.get("expiresIn")).and_then(json_i64)
    {
        expires_at = now_ms + expires_in.saturating_mul(1_000);
    }
    let mut refresh_expires_at = auth
        .and_then(|map| map.get("refreshExpiresAt"))
        .and_then(json_i64)
        .unwrap_or(0);
    if refresh_expires_at == 0
        && let Some(refresh_in) = auth
            .and_then(|map| map.get("refreshExpiresIn"))
            .and_then(json_i64)
    {
        refresh_expires_at = now_ms + refresh_in.saturating_mul(1_000);
    }
    let domain = auth
        .and_then(|map| map.get("domain"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let label = account
        .and_then(|map| map.get("nickname"))
        .and_then(string_value)
        .or_else(|| {
            account
                .and_then(|map| map.get("phoneNumber"))
                .and_then(string_value)
        })
        .unwrap_or_else(|| uid.to_owned());
    Ok(Credential {
        uid: uid.to_owned(),
        access: SecretString::from(access),
        refresh: SecretString::from(refresh),
        expires_at,
        refresh_expires_at,
        domain,
        token_type: auth
            .and_then(|map| map.get("tokenType"))
            .and_then(Value::as_str)
            .unwrap_or("Bearer")
            .to_owned(),
        label,
        site,
        desktop: true,
        api_key: false,
        enterprise_id: account
            .and_then(|map| map.get("enterpriseId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned(),
        auth_path: auth_path.display().to_string(),
    })
}

fn encrypted_value(value: &Value) -> bool {
    value.get("$wbEncrypted").is_some()
        || value
            .as_str()
            .is_some_and(|text| text.contains("$wbEncrypted"))
}

fn string_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn json_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|n| n as i64))
        .or_else(|| value.as_f64().map(|n| n as i64))
}

pub(crate) fn domain(credential: &Credential, base: &str) -> String {
    if !credential.domain.is_empty() {
        return credential.domain.clone();
    }
    hofund_plugin_sdk::url::Url::parse(base)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?;
            Some(match url.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            })
        })
        .unwrap_or_default()
}

pub(crate) fn signed_headers(credential: &Credential, base: &str) -> Vec<(String, String)> {
    let mut headers = vec![
        (
            "Authorization".to_owned(),
            format!("Bearer {}", secret_text(&credential.access)),
        ),
        ("X-User-Id".to_owned(), credential.uid.clone()),
        ("X-Product".to_owned(), "SaaS".to_owned()),
        ("X-IDE-Type".to_owned(), "WorkBuddy".to_owned()),
        ("User-Agent".to_owned(), credential.site.cli_user_agent()),
        ("X-Requested-With".to_owned(), "XMLHttpRequest".to_owned()),
        ("X-Agent-Intent".to_owned(), "craft".to_owned()),
        ("X-Agent-Purpose".to_owned(), "conversation".to_owned()),
        ("X-IDE-Name".to_owned(), "WorkBuddy".to_owned()),
        ("X-IDE-Version".to_owned(), Site::APP_VERSION.to_owned()),
    ];
    if credential.api_key {
        headers.push((
            "X-API-Key".to_owned(),
            secret_text(&credential.access).to_owned(),
        ));
    } else {
        headers.push(("X-Domain".to_owned(), domain(credential, base)));
    }
    if !credential.api_key && !credential.enterprise_id.is_empty() {
        headers.push((
            "X-Enterprise-Id".to_owned(),
            credential.enterprise_id.clone(),
        ));
        headers.push(("X-Tenant-Id".to_owned(), credential.enterprise_id.clone()));
    }
    headers
}

pub(crate) fn chat_headers() -> Vec<(String, String)> {
    vec![
        ("X-Agent-Type".to_owned(), "main".to_owned()),
        ("X-Agent-Runtime".to_owned(), "local-agent".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
        ("X-Stainless-Retry-Count".to_owned(), "0".to_owned()),
        ("X-Stainless-Timeout".to_owned(), "1200".to_owned()),
        ("X-Stainless-Lang".to_owned(), "js".to_owned()),
        (
            "X-Stainless-Package-Version".to_owned(),
            "6.25.0".to_owned(),
        ),
        ("X-Stainless-OS".to_owned(), sdk_os()),
        ("X-Stainless-Arch".to_owned(), sdk_arch()),
        ("X-Stainless-Runtime".to_owned(), "node".to_owned()),
        (
            "X-Stainless-Runtime-Version".to_owned(),
            "v22.21.1".to_owned(),
        ),
    ]
}

fn sdk_os() -> String {
    match std::env::consts::OS {
        "macos" => "MacOS".to_owned(),
        "windows" => "Windows".to_owned(),
        "linux" => "Linux".to_owned(),
        "freebsd" => "FreeBSD".to_owned(),
        "openbsd" => "OpenBSD".to_owned(),
        "android" => "Android".to_owned(),
        "" => "Unknown".to_owned(),
        other => format!("Other:{other}"),
    }
}

fn sdk_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x64".to_owned(),
        "aarch64" => "arm64".to_owned(),
        "arm" => "arm".to_owned(),
        "x86" => "x32".to_owned(),
        "" => "unknown".to_owned(),
        other => format!("other:{other}"),
    }
}

pub(crate) fn apply_signed_headers(
    mut call: hofund_plugin_sdk::Call,
    credential: &Credential,
    base: &str,
) -> hofund_plugin_sdk::Call {
    for (name, value) in signed_headers(credential, base) {
        call = if name == "Authorization" || name == "X-API-Key" {
            call.sensitive_header(name, value)
        } else {
            call.header(name, value)
        };
    }
    call
}

#[cfg(test)]
mod tests {
    use super::{
        Credential, Site, chat_headers, hydrate_desktop, parse_desktop_info, signed_headers,
    };
    use hofund_kit_subscription::TestHost;
    use hofund_plugin_api::SecretString;
    use std::path::Path;

    fn credential(site: Site) -> Credential {
        Credential {
            uid: "u1".to_owned(),
            access: SecretString::from("fixture-access".to_owned()),
            refresh: SecretString::from(String::new()),
            expires_at: 0,
            refresh_expires_at: 0,
            domain: String::new(),
            token_type: "Bearer".to_owned(),
            label: "fixture-user".to_owned(),
            site,
            desktop: false,
            api_key: false,
            enterprise_id: "enterprise-1".to_owned(),
            auth_path: String::new(),
        }
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn a2c_34_desktop_uid_mismatch_is_auth() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("auth.info");
        std::fs::write(&path, include_str!("fixtures/workbuddy-desktop.info")).expect("write");
        let mut stored = credential(Site::China);
        stored.uid = "other-uid".to_owned();
        stored.desktop = true;
        stored.auth_path = path.display().to_string();
        let host = TestHost::open_for(
            hofund_plugin_api::PluginKind::Subscription,
            hofund_plugin_api::PluginId::from_static("workbuddy"),
            dir.path(),
        );
        assert!(hydrate_desktop(host.context(), &stored).is_err());
    }

    #[test]
    fn a2c_35_desktop_expires_in_sets_expiry() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("auth.info");
        std::fs::write(
            &path,
            r#"{"account":{"uid":"u1","nickname":"n"},"auth":{"accessToken":"tok","expiresIn":3600}}"#,
        )
        .expect("write");
        let cred = parse_desktop_info(
            &std::fs::read_to_string(&path).expect("read"),
            Site::China,
            Path::new(&path),
            1_700_000_000_000,
        )
        .expect("parse");
        assert_eq!(cred.expires_at, 1_700_000_000_000 + 3_600_000);
    }

    #[test]
    fn r55_desktop_rejects_encrypted_tokens_without_exposing_envelopes() {
        for auth in [
            r#"{"accessToken":{"$wbEncrypted":1,"envelope":"secret-envelope"}}"#,
            r#"{"accessToken":"plain","refreshToken":{"$wbEncrypted":1,"envelope":"secret-envelope"}}"#,
        ] {
            let text = format!(r#"{{"account":{{"uid":"u1"}},"auth":{auth}}}"#);
            let error = parse_desktop_info(&text, Site::China, Path::new("auth.info"), 0)
                .expect_err("encrypted desktop token");
            assert!(
                error
                    .message()
                    .contains("desktop keeps its sign-in encrypted")
            );
            assert!(error.message().contains("use browser sign-in instead"));
            assert!(!error.message().contains("secret-envelope"));
            assert!(!format!("{error:?}").contains("secret-envelope"));
        }
    }

    #[test]
    fn r55_signed_headers_match_each_site() {
        for (site, base, agent) in [
            (
                Site::China,
                "https://workbuddy-cn.test",
                "WorkBuddy/5.9.3 WorkBuddy/5.9.3 CLI/2.165.0",
            ),
            (
                Site::International,
                "https://workbuddy-ai.test",
                "WorkBuddy/5.9.3 workbuddy-ai/5.9.3 CLI/2.165.0",
            ),
        ] {
            let headers = signed_headers(&credential(site), base);
            for (name, expected) in [
                ("Authorization", "Bearer fixture-access"),
                ("X-User-Id", "u1"),
                ("X-Product", "SaaS"),
                ("X-IDE-Type", "WorkBuddy"),
                ("User-Agent", agent),
                ("X-Requested-With", "XMLHttpRequest"),
                ("X-Agent-Intent", "craft"),
                ("X-Agent-Purpose", "conversation"),
                ("X-IDE-Name", "WorkBuddy"),
                ("X-IDE-Version", "5.9.3"),
                ("X-Enterprise-Id", "enterprise-1"),
                ("X-Tenant-Id", "enterprise-1"),
            ] {
                assert_eq!(header(&headers, name), Some(expected), "{name}");
            }
            assert_eq!(
                header(&headers, "X-Domain"),
                Some(base.trim_start_matches("https://"))
            );
            assert!(header(&headers, "X-API-Key").is_none());
        }
    }

    #[test]
    fn r55_key_headers_have_no_domain_or_enterprise() {
        let mut credential = credential(Site::International);
        credential.api_key = true;
        credential.domain = "must-not-be-sent.test".to_owned();
        let headers = signed_headers(&credential, "https://workbuddy-ai.test");
        assert_eq!(header(&headers, "X-API-Key"), Some("fixture-access"));
        assert!(header(&headers, "X-Domain").is_none());
        assert!(header(&headers, "X-Enterprise-Id").is_none());
        assert!(header(&headers, "X-Tenant-Id").is_none());
        credential.enterprise_id.clear();
        let headers = signed_headers(&credential, "https://workbuddy-ai.test");
        assert!(header(&headers, "X-Enterprise-Id").is_none());
        assert!(header(&headers, "X-Tenant-Id").is_none());
    }

    #[test]
    fn r55_chat_headers_match_the_desktop_cli() {
        let headers = chat_headers();
        for (name, expected) in [
            ("X-Agent-Type", "main"),
            ("X-Agent-Runtime", "local-agent"),
            ("Accept", "application/json"),
            ("X-Stainless-Retry-Count", "0"),
            ("X-Stainless-Timeout", "1200"),
            ("X-Stainless-Lang", "js"),
            ("X-Stainless-Package-Version", "6.25.0"),
            ("X-Stainless-Runtime", "node"),
            ("X-Stainless-Runtime-Version", "v22.21.1"),
        ] {
            assert_eq!(header(&headers, name), Some(expected), "{name}");
        }
        assert!(header(&headers, "X-Stainless-OS").is_some());
        assert!(header(&headers, "X-Stainless-Arch").is_some());
        assert_eq!(
            super::domain(&credential(Site::China), "http://127.0.0.1:18443"),
            "127.0.0.1:18443"
        );
    }
}
