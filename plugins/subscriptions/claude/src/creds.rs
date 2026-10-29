//! Claude Code's sign-in files, and the secret hofund keeps for an account.

use std::path::Path;

use hofund_plugin_api::{AccountId, PluginError};
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::{SecretObject, account_id_from};

/// The OAuth tokens Claude Code writes on Linux and Windows.
pub(crate) const CREDENTIALS_FILE: &str = ".credentials.json";

/// Claude Code's settings, which name the signed-in account.
pub(crate) const PROFILE_FILE: &str = ".claude.json";

/// The largest sign-in file read.
const FILE_CAP: u64 = 1024 * 1024;

/// Secret fields: the credentials file text, and the account profile.
pub(crate) const CREDENTIALS: &str = "credentials";
pub(crate) const PROFILE: &str = "profile";
pub(crate) const VERSION: &str = "v";
pub(crate) const LAPSED: &str = "lapsed";

/// Why a lapsed account needs a new sign-in.
pub(crate) const LAPSED_REASON: &str = "Claude Code says this account is signed out; sign in again";

/// A readable credentials file.
pub(crate) struct Credentials {
    /// The file as Claude Code wrote it.
    pub(crate) text: String,
    /// Whether it holds a refresh token.
    pub(crate) refreshable: bool,
    /// The subscription type, such as `pro` or `max`.
    pub(crate) plan: Option<String>,
}

/// Parses a credentials file. `None` when it has no access token.
pub(crate) fn parse_credentials(text: &str) -> Option<Credentials> {
    let Ok(Value::Object(root)) = hofund_plugin_sdk::json::from_str::<Value>(text) else {
        return None;
    };
    let oauth = root.get("claudeAiOauth")?.as_object()?;
    non_empty(oauth, "accessToken")?;
    Some(Credentials {
        text: text.to_owned(),
        refreshable: non_empty(oauth, "refreshToken").is_some(),
        plan: non_empty(oauth, "subscriptionType").map(str::to_owned),
    })
}

/// The credentials file in `dir`, when it is there and readable.
pub(crate) fn read_credentials(dir: &Path) -> Option<Credentials> {
    parse_credentials(&read_capped(&dir.join(CREDENTIALS_FILE))?)
}

/// Which account signed in, as Claude Code names it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Profile {
    /// The `oauthAccount` object, with what `auth status` added.
    pub(crate) fields: Map<String, Value>,
}

impl Profile {
    /// The `oauthAccount` object in `dir/.claude.json`, or an empty profile.
    pub(crate) fn read(dir: &Path) -> Self {
        let fields = read_capped(&dir.join(PROFILE_FILE))
            .and_then(|text| hofund_plugin_sdk::json::from_str::<Value>(&text).ok())
            .and_then(|root| root.get("oauthAccount").and_then(Value::as_object).cloned())
            .unwrap_or_default();
        Self { fields }
    }

    /// Parses the stored profile text.
    pub(crate) fn parse(text: &str) -> Self {
        match hofund_plugin_sdk::json::from_str::<Value>(text) {
            Ok(Value::Object(fields)) => Self { fields },
            _ => Self::default(),
        }
    }

    /// The account email.
    pub(crate) fn email(&self) -> Option<&str> {
        non_empty(&self.fields, "emailAddress")
    }

    /// The account UUID.
    pub(crate) fn uuid(&self) -> Option<&str> {
        non_empty(&self.fields, "accountUuid")
    }

    /// Fills what is missing from `claude auth status --json` output.
    pub(crate) fn add_status(&mut self, status: &Value) {
        for (from, to) in [
            ("email", "emailAddress"),
            ("orgName", "organizationName"),
            ("orgId", "organizationUuid"),
        ] {
            if non_empty(&self.fields, to).is_some() {
                continue;
            }
            if let Some(value) = status
                .get(from)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                self.fields
                    .insert(to.to_owned(), Value::String(value.to_owned()));
            }
        }
    }

    /// The account this profile names. The UUID is stable; the email is the
    /// fallback when Claude Code did not say it.
    pub(crate) fn account_id(&self) -> Result<AccountId, PluginError> {
        if let Some(uuid) = self.uuid() {
            return Ok(account_id_from(&["claude", uuid]));
        }
        let email = self.email().ok_or_else(|| {
            PluginError::auth("Claude Code signed in but did not say which account")
        })?;
        Ok(account_id_from(&["claude-email", &email.to_lowercase()]))
    }

    /// The profile as stored text.
    pub(crate) fn to_text(&self) -> String {
        Value::Object(self.fields.clone()).to_string()
    }

    /// `.claude.json` content naming this account.
    pub(crate) fn settings_text(&self) -> String {
        let mut root = Map::new();
        root.insert(
            "oauthAccount".to_owned(),
            Value::Object(self.fields.clone()),
        );
        Value::Object(root).to_string()
    }
}

/// `claude auth status --json` output, when it is a JSON object.
pub(crate) fn parse_status(text: &str) -> Option<Value> {
    match hofund_plugin_sdk::json::from_str::<Value>(text.trim()) {
        Ok(value @ Value::Object(_)) => Some(value),
        _ => None,
    }
}

/// Whether the status says the account is signed in. `None` when it does
/// not say.
pub(crate) fn logged_in(status: &Value) -> Option<bool> {
    status.get("loggedIn").and_then(Value::as_bool)
}

/// Whether `status` says plainly that the account is signed out.
pub(crate) fn logged_in_says_no(status: Option<&Value>) -> bool {
    status.and_then(logged_in) == Some(false)
}

/// The secret kept for an account.
pub(crate) fn blob(credentials: &Credentials, profile: &Profile) -> SecretObject {
    SecretObject::new()
        .with_number(VERSION, 1)
        .with_text(CREDENTIALS, &credentials.text)
        .with_text(PROFILE, &profile.to_text())
}

fn non_empty<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > FILE_CAP {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Creates `dir` and its parents, readable by this user only.
pub(crate) fn private_dir(dir: &Path) -> Result<(), PluginError> {
    std::fs::create_dir_all(dir)
        .map_err(|_| PluginError::io("cannot create the Claude Code directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| PluginError::io("cannot restrict the Claude Code directory"))?;
    }
    Ok(())
}

/// Writes `text` to `path`, readable by this user only.
pub(crate) fn write_private(path: &Path, text: &str) -> Result<(), PluginError> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let failed = |_| PluginError::io("cannot write the Claude Code sign-in file");
    let mut file = options.open(path).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)
}
