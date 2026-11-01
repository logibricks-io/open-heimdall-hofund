//! Writing Codex CLI's auth.json from a stored account.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError};
use hofund_plugin_sdk::json::{Map, Value};
use hofund_plugin_sdk::oauth::{SecretObject, secret_text};

/// Writes `home/auth.json` in the shape Codex CLI reads in file mode.
pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
) -> Result<(), PluginError> {
    let Some(secret) = host.accounts().secret(account).await? else {
        return Err(PluginError::auth(
            "no ChatGPT sign-in is stored for this account",
        ));
    };
    let blob = SecretObject::parse(&secret)?;
    if blob.flag(crate::tokens::LAPSED) {
        return Err(PluginError::auth(crate::tokens::LAPSED_REASON));
    }
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT sign-in has no access token"))?;
    let refresh = blob.secret("refresh_token");
    let id_token = blob.secret("id_token");
    let mut tokens = Map::new();
    tokens.insert(
        "access_token".to_owned(),
        Value::String(secret_text(&access).to_owned()),
    );
    if let Some(refresh) = refresh {
        tokens.insert(
            "refresh_token".to_owned(),
            Value::String(secret_text(&refresh).to_owned()),
        );
    }
    if let Some(id_token) = id_token {
        tokens.insert(
            "id_token".to_owned(),
            Value::String(secret_text(&id_token).to_owned()),
        );
    }
    let mut root = Map::new();
    root.insert("tokens".to_owned(), Value::Object(tokens));
    let text = Value::Object(root).to_string();
    std::fs::create_dir_all(home)
        .map_err(|_| PluginError::io("cannot create the Codex home directory"))?;
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let path = home.join("auth.json");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|_| PluginError::io("cannot write Codex auth.json"))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| PluginError::io("cannot restrict Codex auth.json"))?;
        file.write_all(text.as_bytes())
            .map_err(|_| PluginError::io("cannot write Codex auth.json"))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(home.join("auth.json"), text)
            .map_err(|_| PluginError::io("cannot write Codex auth.json"))?;
    }
    Ok(())
}
