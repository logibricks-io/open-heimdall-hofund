//! Writing Codex CLI's sign-in from a stored account.

use std::path::Path;

use hofund_plugin_api::{AccountId, HostContext, PluginError, SecretString};
use hofund_plugin_sdk::oauth::{SecretObject, same_secret, secret_text};

use crate::codex_lock;
use crate::store::{AuthPatch, StoreError, StoreReader, is_valid_auth_json, merge_auth_tokens};

/// Writes Codex's credential store from the stored account.
pub(crate) async fn write(
    account: &AccountId,
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
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
    let canonical = crate::store::canonical_codex_home(home);
    let _guard = crate::codex_lock::try_acquire_home(home)
        .await
        .map_err(|error| match error {
            crate::codex_lock::HomeGuardError::Busy => {
                PluginError::auth("Codex home directory is busy")
            }
            crate::codex_lock::HomeGuardError::Io => {
                PluginError::io("Codex home directory lock failed")
            }
        })?;
    let owners = crate::reconcile::home_owners(host, &canonical).await?;
    crate::reconcile::assert_unique_owner(&owners, account)?;
    let access = blob
        .secret("access_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT sign-in has no access token"))?;
    let refresh = blob.secret("refresh_token");
    let id_token = blob.secret("id_token");
    let reader = StoreReader::new(Some(host), skip_keychain, None);
    let base = match reader.read_login(home, true).await {
        Ok((bytes, _)) => bytes,
        Err(StoreError::SignedOut) => Vec::new(),
        Err(StoreError::InvalidJson) => {
            return Err(PluginError::decode(
                "Codex sign-in on disk is not JSON; nothing was changed",
            ));
        }
        Err(StoreError::Unreadable { .. }) => {
            return Err(PluginError::auth(
                "Codex keeps its sign-in where it cannot be written from here",
            ));
        }
        Err(StoreError::ConfigParse(message)) => return Err(PluginError::config(message)),
        Err(StoreError::FileWrite | StoreError::KeychainWrite) => {
            return Err(PluginError::io("cannot read Codex sign-in before writing"));
        }
    };
    if !base.is_empty() && !is_valid_auth_json(&base) {
        return Err(PluginError::decode(
            "Codex sign-in on disk is not JSON; nothing was changed",
        ));
    }
    let patch = AuthPatch {
        access_token: Some(secret_text(&access).to_owned()),
        refresh_token: refresh.map(|token| secret_text(&token).to_owned()),
        id_token: id_token.map(|token| secret_text(&token).to_owned()),
        chatgpt_mode: true,
    };
    let merged = if base.is_empty() {
        merge_auth_tokens(br#"{"auth_mode":"chatgpt","tokens":{}}"#, &patch)?
    } else {
        merge_auth_tokens(&base, &patch)?
    };
    reader.save_login(home, &merged).await.map_err(map_store)?;
    let updated = blob.with_text("codex_home", &home.to_string_lossy());
    if let Ok(rows) = host.accounts().list().await
        && let Some(row) = rows.iter().find(|row| row.id() == account)
    {
        let account_row = host.accounts().account(account.clone(), row.label())?;
        host.accounts()
            .upsert(account_row, updated.to_secret())
            .await?;
    }
    Ok(())
}

fn map_store(error: StoreError) -> PluginError {
    match error {
        StoreError::SignedOut => PluginError::auth("Codex is signed out in that directory"),
        StoreError::Unreadable { .. } => {
            PluginError::auth("Codex keeps its sign-in where it cannot be written from here")
        }
        StoreError::ConfigParse(message) => PluginError::config(message),
        StoreError::FileWrite | StoreError::KeychainWrite => {
            PluginError::io("cannot write Codex sign-in")
        }
        StoreError::InvalidJson => PluginError::decode("Codex sign-in is not JSON"),
    }
}

/// Outcome of reading Codex's on-disk store before spending a refresh token.
pub(crate) enum RefreshReconcile {
    #[expect(dead_code, reason = "Task 4 protocol uses Proceed after step 0")]
    Proceed,
    Adopt {
        access: SecretString,
        account: Option<String>,
    },
}

/// Reads the Codex store under an already-held home guard (no second lock).
pub(crate) async fn adopt_from_store(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    stale: &SecretString,
    fake: Option<&crate::store::FakeKeychain>,
) -> Option<RefreshReconcile> {
    let reader = StoreReader::new(Some(host), skip_keychain, fake);
    match reader.read_login(home, true).await {
        Ok((bytes, _)) => {
            let access = access_token_from_auth(&bytes)?;
            if !same_secret(&access, stale) {
                return Some(RefreshReconcile::Adopt {
                    access,
                    account: account_id_from_auth(&bytes),
                });
            }
            None
        }
        Err(StoreError::SignedOut) | Err(StoreError::Unreadable { .. }) | Err(_) => None,
    }
}

/// Locks when possible, then compares the Codex store access token to `stale`.
#[expect(dead_code, reason = "legacy entrypoint; Task 4 consolidates on reconcile_rotate")]
pub(crate) async fn reconcile_before_refresh(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    stale: &SecretString,
    fake: Option<&crate::store::FakeKeychain>,
) -> RefreshReconcile {
    let _lock = codex_lock::acquire(home).await;
    adopt_from_store(home, host, skip_keychain, stale, fake)
        .await
        .unwrap_or(RefreshReconcile::Proceed)
}

/// Writes rotated tokens back to Codex's store for `home` when the on-disk access token still matches `stale`.
pub(crate) async fn persist_refresh(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    blob: &SecretObject,
    stale: &SecretString,
    fake: Option<&crate::store::FakeKeychain>,
) {
    crate::reconcile_rotate::persist_refresh(home, host, skip_keychain, blob, stale, fake).await;
}

pub(crate) fn account_id_from_auth(bytes: &[u8]) -> Option<String> {
    let value =
        hofund_plugin_sdk::json::from_slice::<hofund_plugin_sdk::json::Value>(bytes).ok()?;
    value
        .get("tokens")
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(hofund_plugin_sdk::json::Value::as_str)
        .map(str::to_owned)
}

pub(crate) fn access_token_from_auth(bytes: &[u8]) -> Option<SecretString> {
    let value =
        hofund_plugin_sdk::json::from_slice::<hofund_plugin_sdk::json::Value>(bytes).ok()?;
    let token = value.get("tokens")?.get("access_token")?.as_str()?;
    Some(SecretString::from(token.to_owned()))
}

#[cfg(test)]
mod tests {
    use hofund_kit_subscription::{TestHost, block_on};
    use hofund_plugin_api::{PluginId, PluginKind, SecretString};
    use hofund_plugin_sdk::oauth::SecretObject;

    use super::*;
    use crate::store::{FakeKeychain, StoreReader, write_atomic_private};

    fn host(dir: &tempfile::TempDir) -> TestHost {
        TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("codex"),
            dir.path(),
        )
    }

    #[test]
    fn r47b_rr3_persist_refresh_seeds_auth_when_home_was_empty() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let home = dir.path().join("new-codex-home");
        assert!(!home.exists());
        std::fs::create_dir_all(home.parent().expect("parent")).expect("parent");
        let stale = SecretString::from("stale-access".to_owned());
        let blob = SecretObject::new()
            .with_secret(
                "access_token",
                &SecretString::from("fresh-access".to_owned()),
            )
            .with_secret("refresh_token", &SecretString::from("new-r".to_owned()));
        block_on(persist_refresh(
            &home,
            host.context(),
            true,
            &blob,
            &stale,
            None,
        ));
        assert!(home.is_dir());
        let text = std::fs::read_to_string(home.join("auth.json")).expect("auth");
        assert!(text.contains("fresh-access"));
    }

    #[test]
    fn r47b_rr2_persist_refresh_writes_keychain_when_stale_matches() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .expect("config");
        let stale = SecretString::from("stale-access".to_owned());
        let fake = FakeKeychain::default();
        let account = crate::store::keychain_account(&home);
        let on_disk = br#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale-access","refresh_token":"old-r"}}"#;
        fake.set(&account, on_disk);
        let blob = SecretObject::new()
            .with_secret(
                "access_token",
                &SecretString::from("fresh-access".to_owned()),
            )
            .with_secret("refresh_token", &SecretString::from("new-r".to_owned()));
        block_on(persist_refresh(
            &home,
            host.context(),
            false,
            &blob,
            &stale,
            Some(&fake),
        ));
        let reader = StoreReader::new(Some(host.context()), false, Some(&fake));
        let (read, _) = block_on(reader.read_login(&home, true)).expect("read back");
        let text = String::from_utf8(read).expect("utf8");
        assert!(text.contains("fresh-access"));
        assert!(text.contains("new-r"));
    }

    #[test]
    fn r47b_rr2_persist_refresh_keeps_keychain_when_access_replaced() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"keyring\"\n",
        )
        .expect("config");
        let stale = SecretString::from("stale-access".to_owned());
        let fake = FakeKeychain::default();
        let account = crate::store::keychain_account(&home);
        let on_disk = br#"{"auth_mode":"chatgpt","tokens":{"access_token":"someone-else","refresh_token":"keep-r"}}"#;
        fake.set(&account, on_disk);
        let blob = SecretObject::new()
            .with_secret(
                "access_token",
                &SecretString::from("fresh-access".to_owned()),
            )
            .with_secret("refresh_token", &SecretString::from("new-r".to_owned()));
        block_on(persist_refresh(
            &home,
            host.context(),
            false,
            &blob,
            &stale,
            Some(&fake),
        ));
        let reader = StoreReader::new(Some(host.context()), false, Some(&fake));
        let (read, _) = block_on(reader.read_login(&home, true)).expect("read back");
        assert_eq!(read, on_disk);
    }

    #[test]
    fn r47b_rr2_persist_refresh_writes_auth_json_when_stale_matches() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .expect("config");
        write_atomic_private(
            &home.join("auth.json"),
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale-access","refresh_token":"old-r"}}"#,
        )
        .expect("seed");
        let stale = SecretString::from("stale-access".to_owned());
        let blob = SecretObject::new()
            .with_secret(
                "access_token",
                &SecretString::from("fresh-access".to_owned()),
            )
            .with_secret("refresh_token", &SecretString::from("new-r".to_owned()));
        block_on(persist_refresh(
            &home,
            host.context(),
            true,
            &blob,
            &stale,
            None,
        ));
        let text = std::fs::read_to_string(home.join("auth.json")).expect("auth");
        assert!(text.contains("fresh-access"));
    }

    #[test]
    fn r47b_rr2_persist_refresh_keeps_auth_json_when_access_replaced() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .expect("config");
        let seed = br#"{"auth_mode":"chatgpt","tokens":{"access_token":"someone-else","refresh_token":"keep-r"}}"#;
        write_atomic_private(&home.join("auth.json"), seed).expect("seed");
        let stale = SecretString::from("stale-access".to_owned());
        let blob = SecretObject::new().with_secret(
            "access_token",
            &SecretString::from("fresh-access".to_owned()),
        );
        block_on(persist_refresh(
            &home,
            host.context(),
            true,
            &blob,
            &stale,
            None,
        ));
        let read = std::fs::read(home.join("auth.json")).expect("auth");
        assert_eq!(read, seed);
    }
}
