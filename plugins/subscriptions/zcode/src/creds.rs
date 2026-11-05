//! Z Code credential decryption and parsing from the agent store.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{PluginError, SecretString};
use hofund_plugin_sdk::digest::sha256;
use hofund_plugin_sdk::enc_v1;
use hofund_plugin_sdk::json::{self, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from, unverified_claims};

use crate::{BIGMODEL_ANTHROPIC_BASE, ZAI_ANTHROPIC_BASE};

const FILE_CAP: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct PlanAuth {
    pub api_key: Option<SecretString>,
    pub jwt: Option<SecretString>,
    pub team_token: Option<SecretString>,
    pub org: String,
    pub project: String,
    pub base: String,
    pub site: Site,
    pub uid: String,
    pub label: String,
    pub follow_agent: bool,
    pub agent_credentials: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    Zai,
    Bigmodel,
}

impl PlanAuth {
    pub(crate) fn team(&self) -> bool {
        !self.org.is_empty() && !self.project.is_empty()
    }
}

pub(crate) const LAPSED: &str = "lapsed";

/// Why a team seat with a refused business token needs sign-in again.
pub(crate) const LAPSED_REASON: &str =
    "Z Code refused the stored team business sign-in; sign in again";

pub(crate) fn credentials_path(home: &Path) -> PathBuf {
    home.join("v2").join("credentials.json")
}

pub(crate) fn setting_path(home: &Path) -> PathBuf {
    home.join("v2").join("setting.json")
}

pub(crate) fn config_path(home: &Path) -> PathBuf {
    home.join("v2").join("config.json")
}

pub(crate) fn secret_seed(home: &Path) -> Vec<u8> {
    if let Ok(seed) = std::env::var("ZCODE_CREDENTIAL_SECRET")
        && !seed.is_empty()
    {
        return sha256(seed.as_bytes()).to_vec();
    }
    let platform = if cfg!(target_os = "windows") {
        "win32"
    } else {
        std::env::consts::OS
    };
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let user = user.rsplit('\\').next().unwrap_or(user.as_str()).to_owned();
    let home_text = home
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n != ".zcode"))
        .and_then(|p| p.to_str())
        .or_else(|| home.parent().and_then(|p| p.to_str()))
        .unwrap_or("");
    let seed = format!("zcode-credential-fallback:{platform}:{home_text}:{user}");
    sha256(seed.as_bytes()).to_vec()
}

pub(crate) fn decrypt(secret: &[u8], value: &str) -> Option<String> {
    enc_v1::decrypt(secret, value)
}

#[cfg(test)]
pub(crate) fn encrypt_for_test(secret: &[u8], plain: &str) -> String {
    enc_v1::encrypt_for_test(secret, plain)
}

pub(crate) fn read_store(path: &Path) -> Result<json::Map<String, Value>, PluginError> {
    let meta = std::fs::metadata(path)
        .map_err(|_| PluginError::auth("Z Code credentials are not readable"))?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return Err(PluginError::auth("Z Code credentials are not readable"));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| PluginError::auth("Z Code credentials are not readable"))?;
    let Value::Object(map) = json::from_str(&text)
        .map_err(|_| PluginError::auth("Z Code credentials are not readable"))?
    else {
        return Err(PluginError::auth("Z Code credentials are not readable"));
    };
    Ok(map)
}

pub(crate) fn parse_agent_home(home: &Path) -> Result<PlanAuth, PluginError> {
    let cred_path = credentials_path(home);
    let store = read_store(&cred_path)?;
    let secret = secret_seed(home);
    parse_store(&store, &secret, home, &cred_path, true)
}

fn parse_store(
    store: &json::Map<String, Value>,
    secret: &[u8],
    home: &Path,
    cred_path: &Path,
    follow_agent: bool,
) -> Result<PlanAuth, PluginError> {
    let mut auth = PlanAuth {
        api_key: None,
        jwt: None,
        team_token: None,
        org: String::new(),
        project: String::new(),
        base: ZAI_ANTHROPIC_BASE.to_owned(),
        site: Site::Zai,
        uid: String::new(),
        label: String::new(),
        follow_agent,
        agent_credentials: Some(cred_path.to_path_buf()),
    };
    for (name, value) in store {
        let Some(cipher) = value.as_str() else {
            continue;
        };
        if !name.contains(":coding-plan:") || !name.ends_with(":api-key") {
            continue;
        }
        let Some(key) = decrypt(secret, cipher) else {
            continue;
        };
        if !key.contains('.') {
            continue;
        }
        let base = if name.contains(":bigmodel-") {
            BIGMODEL_ANTHROPIC_BASE
        } else {
            ZAI_ANTHROPIC_BASE
        };
        if auth.api_key.is_none() || base == ZAI_ANTHROPIC_BASE {
            auth.api_key = Some(SecretString::from(key));
            auth.base = base.to_owned();
            auth.site = if base.contains("bigmodel.cn") {
                Site::Bigmodel
            } else {
                Site::Zai
            };
        }
    }
    if auth.api_key.is_none()
        && let Some(from_config) = config_plan_key(home, secret)?
    {
        auth.api_key = Some(from_config.key);
        auth.base = from_config.base;
        auth.site = from_config.site;
    }
    if let Some(jwt_cipher) = store.get("zcodejwttoken").and_then(Value::as_str)
        && let Some(jwt) = decrypt(secret, jwt_cipher)
    {
        let jwt = jwt.trim().trim_start_matches("Bearer ").trim();
        if !jwt.is_empty() {
            auth.jwt = Some(SecretString::from(jwt.to_owned()));
        }
    }
    if let Some(team) = team_from_setting(store, secret, home)? {
        auth.team_token = team.token;
        auth.org = team.org;
        auth.project = team.project;
        auth.base = team.base;
        auth.site = team.site;
        auth.api_key = None;
    }
    if auth.api_key.is_none() && !auth.team() && auth.jwt.is_some() {
        return Err(PluginError::unsupported(
            "Z Code Start Plan sign-in is not supported; use a GLM Coding Plan or team seat",
        ));
    }
    if auth.api_key.is_none() && !auth.team() {
        return Err(PluginError::auth(
            "Z Code has no readable GLM Coding Plan or team seat in that directory",
        ));
    }
    let mut info_email = String::new();
    let mut info_name = String::new();
    let mut info_id = String::new();
    for key in ["oauth:zai:user_info", "oauth:bigmodel:user_info"] {
        let Some(cipher) = store.get(key).and_then(Value::as_str) else {
            continue;
        };
        let Some(json) = decrypt(secret, cipher) else {
            continue;
        };
        let Ok(Value::Object(obj)) = json::from_str(&json) else {
            continue;
        };
        if info_email.is_empty() {
            info_email = obj
                .get("email")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
        }
        if info_name.is_empty() {
            info_name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
        }
        if info_id.is_empty() {
            info_id = obj
                .get("user_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
        }
    }
    auth.uid = info_id.trim().to_owned();
    auth.label = who_label(&info_email, &info_name, &info_id);
    Ok(auth)
}

struct ConfigKey {
    key: SecretString,
    base: String,
    site: Site,
}

fn config_plan_key(home: &Path, secret: &[u8]) -> Result<Option<ConfigKey>, PluginError> {
    let path = config_path(home);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let Ok(Value::Object(root)) = json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    let Some(providers) = root.get("provider").and_then(Value::as_object) else {
        return Ok(None);
    };
    for (ids, base, site) in [
        (
            [
                "builtin:zai-coding-plan",
                "account:zai-individual-coding-plan",
            ],
            ZAI_ANTHROPIC_BASE,
            Site::Zai,
        ),
        (
            [
                "builtin:bigmodel-coding-plan",
                "account:bigmodel-individual-coding-plan",
            ],
            BIGMODEL_ANTHROPIC_BASE,
            Site::Bigmodel,
        ),
    ] {
        for id in ids {
            let Some(provider) = providers.get(id).and_then(Value::as_object) else {
                continue;
            };
            let mut key = provider
                .get("options")
                .and_then(|v| v.get("apiKey"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            if key.starts_with("enc:") {
                key = decrypt(secret, &key).unwrap_or_default();
                key = key.trim().to_owned();
            }
            if key.contains('.') && !key.contains([' ', '{', '$']) {
                return Ok(Some(ConfigKey {
                    key: SecretString::from(key),
                    base: base.to_owned(),
                    site,
                }));
            }
        }
    }
    Ok(None)
}

struct TeamPick {
    token: Option<SecretString>,
    org: String,
    project: String,
    base: String,
    site: Site,
}

fn team_from_setting(
    store: &json::Map<String, Value>,
    secret: &[u8],
    home: &Path,
) -> Result<Option<TeamPick>, PluginError> {
    let path = setting_path(home);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let Ok(Value::Object(set)) = json::from_str::<Value>(&text) else {
        return Ok(None);
    };
    let domain = set
        .get("providerFamilyDomain")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut families = ["zai", "bigmodel"];
    if domain == "zai" || domain == "bigmodel" {
        families = [domain, ""];
    }
    let selections = set
        .get("providerFamilyConnectionSelections")
        .and_then(Value::as_object);
    let legacy = set
        .get("modelProviderFamilySelectedKeys")
        .and_then(Value::as_object);
    for fam in families {
        if fam.is_empty() {
            continue;
        }
        let (org, project) = selection_org_project(fam, selections, legacy);
        if org.is_empty() || project.is_empty() {
            continue;
        }
        let token_key = format!("oauth:{fam}:access_token");
        let Some(cipher) = store.get(&token_key).and_then(Value::as_str) else {
            continue;
        };
        let Some(tok) = decrypt(secret, cipher) else {
            continue;
        };
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        if fam == "bigmodel"
            && let Some(jv) = store.get("zcodejwttoken").and_then(Value::as_str)
            && decrypt(secret, jv).is_some_and(|jwt| jwt.trim() == tok)
        {
            continue;
        }
        let (base, site) = if fam == "bigmodel" {
            (BIGMODEL_ANTHROPIC_BASE.to_owned(), Site::Bigmodel)
        } else {
            (ZAI_ANTHROPIC_BASE.to_owned(), Site::Zai)
        };
        return Ok(Some(TeamPick {
            token: Some(SecretString::from(tok.to_owned())),
            org,
            project,
            base,
            site,
        }));
    }
    Ok(None)
}

fn selection_org_project(
    fam: &str,
    selections: Option<&json::Map<String, Value>>,
    legacy: Option<&json::Map<String, Value>>,
) -> (String, String) {
    if let Some(map) = selections
        && let Some(entry) = map.get(fam).and_then(Value::as_object)
    {
        if entry.get("kind").and_then(Value::as_str) == Some("team-coding-plan") {
            let org = entry
                .get("organizationId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            let project = entry
                .get("projectId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            if !org.is_empty() && !project.is_empty() {
                return (org, project);
            }
        }
        return (String::new(), String::new());
    }
    if let Some(map) = legacy {
        let prefix = format!("team-plan:builtin:{fam}-coding-plan:");
        if let Some(rest) = map.get(fam).and_then(Value::as_str)
            && let Some(rest) = rest.strip_prefix(&prefix)
        {
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() == 3 {
                return (percent_decode(parts[1]), percent_decode(parts[2]));
            }
        }
    }
    (String::new(), String::new())
}

fn percent_decode(text: &str) -> String {
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            )
        {
            out.push(char::from(byte));
            index += 3;
            continue;
        }
        out.push(char::from(bytes[index]));
        index += 1;
    }
    out
}

fn who_label(email: &str, name: &str, id: &str) -> String {
    let email = email.strip_suffix("@phone.local").unwrap_or(email).trim();
    if !email.is_empty() {
        return email.to_owned();
    }
    if !name.trim().is_empty() {
        return name.trim().to_owned();
    }
    if !id.trim().is_empty() {
        return id.trim().to_owned();
    }
    "Z Code".to_owned()
}

pub(crate) fn account_id_for(auth: &PlanAuth) -> hofund_plugin_api::AccountId {
    if !auth.uid.is_empty() {
        return account_id_from(&["zcode", auth.uid.as_str()]);
    }
    account_id_from(&["zcode", auth.label.as_str()])
}

pub(crate) fn secret_blob(auth: &PlanAuth, device: &str) -> SecretObject {
    let mut blob = SecretObject::new().with_number("v", 1);
    blob = blob.with_text(
        "site",
        match auth.site {
            Site::Zai => "zai",
            Site::Bigmodel => "bigmodel",
        },
    );
    blob = blob.with_text("base", &auth.base);
    blob = blob.with_text("device", device);
    blob = blob.with_text("label", &auth.label);
    if !auth.uid.is_empty() {
        blob = blob.with_text("uid", &auth.uid);
    }
    if auth.follow_agent {
        blob = blob.with_flag("follow_agent", true);
        if let Some(path) = &auth.agent_credentials {
            blob = blob.with_text("agent_credentials", &path.to_string_lossy());
        }
    }
    if let Some(key) = &auth.api_key {
        blob = blob.with_secret("api_key", key);
    }
    if let Some(jwt) = &auth.jwt {
        blob = blob.with_secret("jwt", jwt);
    }
    if auth.team() {
        if let Some(token) = &auth.team_token {
            blob = blob.with_secret("team_token", token);
        }
        blob = blob.with_text("org", &auth.org);
        blob = blob.with_text("project", &auth.project);
    }
    if let Some(exp) = auth.jwt.as_ref().and_then(jwt_exp_ms) {
        blob = blob.with_number("expires_ms", exp);
    }
    blob
}

pub(crate) fn parse_secret(secret: &SecretString) -> Result<PlanAuth, PluginError> {
    let blob = SecretObject::parse(secret)?;
    if blob.flag(LAPSED) {
        return Err(PluginError::auth(LAPSED_REASON));
    }
    let base = blob
        .text("base")
        .filter(|value| !value.is_empty())
        .unwrap_or(ZAI_ANTHROPIC_BASE)
        .to_owned();
    let site = match blob.text("site").unwrap_or("zai") {
        "bigmodel" => Site::Bigmodel,
        _ => Site::Zai,
    };
    Ok(PlanAuth {
        api_key: blob.secret("api_key"),
        jwt: blob.secret("jwt"),
        team_token: blob.secret("team_token"),
        org: blob.text("org").unwrap_or("").to_owned(),
        project: blob.text("project").unwrap_or("").to_owned(),
        base,
        site,
        uid: blob.text("uid").unwrap_or("").to_owned(),
        label: blob
            .text("label")
            .filter(|value| !value.is_empty())
            .unwrap_or("Z Code")
            .to_owned(),
        follow_agent: blob.flag("follow_agent"),
        agent_credentials: blob
            .text("agent_credentials")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
    })
}

pub(crate) fn jwt_exp_ms(jwt: &SecretString) -> Option<u64> {
    let map = unverified_claims(jwt).ok()?;
    let exp = map.get("exp").and_then(Value::as_f64)?;
    if exp <= 0.0 {
        return None;
    }
    Some((exp * 1000.0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hofund_plugin_sdk::json::json;

    #[test]
    fn a2d_20_decrypt_matches_reference_layout() {
        let secret = secret_seed(Path::new("/home/tester"));
        let enc = encrypt_for_test(&secret, "own.secret.key");
        assert_eq!(decrypt(&secret, &enc).as_deref(), Some("own.secret.key"));
    }

    #[test]
    fn a2d_16_personal_selection_ignores_stale_legacy_team_key() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join(".zcode");
        std::fs::create_dir_all(home.join("v2")).expect("dir");
        let secret = secret_seed(&home);
        let key_name =
            "account-provider:coding-plan:account:zai-individual-coding-plan:account:abc:api-key";
        let enc_key = encrypt_for_test(&secret, "personal.plan.key");
        let store = json!({
            key_name: enc_key,
            "oauth:zai:access_token": encrypt_for_test(&secret, "stale-team-biz-token"),
            "oauth:zai:user_info": encrypt_for_test(
                &secret,
                r#"{"user_id":"u-personal","email":"user@zcode.test","name":"User"}"#
            ),
        });
        std::fs::write(home.join("v2").join("credentials.json"), store.to_string()).expect("write");
        let setting = json!({
            "providerFamilyDomain": "zai",
            "providerFamilyConnectionSelections": {
                "zai": {
                    "kind": "individual-coding-plan",
                    "organizationId": "",
                    "projectId": ""
                }
            },
            "modelProviderFamilySelectedKeys": {
                "zai": "team-plan:builtin:zai-coding-plan:legacy-org:legacy-proj"
            }
        });
        std::fs::write(home.join("v2").join("setting.json"), setting.to_string()).expect("write");
        let auth = parse_agent_home(&home).expect("parse personal");
        assert!(!auth.team());
        assert_eq!(
            auth.api_key
                .as_ref()
                .map(hofund_plugin_sdk::oauth::secret_text),
            Some("personal.plan.key")
        );
    }
}
