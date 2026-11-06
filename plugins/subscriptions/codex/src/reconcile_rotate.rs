//! OAuth refresh steps 0–5 under account lease and composite home guard.

use std::path::{Path, PathBuf};

use hofund_plugin_api::{AccountId, HostContext, PluginError, PluginErrorKind, SecretString};
use hofund_plugin_sdk::Call;
use hofund_plugin_sdk::oauth::{SecretObject, parse_token_set, same_secret, secret_text, token_error};

use crate::agent_sign_in::{access_token_from_auth, account_id_from_auth};
use crate::codex_lock::{HomeGuard, HomeGuardError};
use crate::reconcile::{
    RefreshMarkers, assert_unique_owner, clear_markers, fingerprint_location, fingerprint_tuple,
    fingerprint_tuple_absent, home_owners, parse_markers, with_inflight, with_pending, BASELINE_ABSENT,
};
use crate::store::{
    AuthPatch, FakeKeychain, KeychainError, StoreError, StoreReader, canonical_codex_home,
    merge_auth_tokens, read_store_setting,
};
use crate::tokens::{
    Access, Need, access_of, account_of, fresh, stored, unix_seconds, LAPSED, REFRESH_NEEDED,
};
use crate::{CLIENT_ID, REFRESH_SCOPE};

const LEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const REAUTH: &str = "ChatGPT sign-in refresh must be restarted; sign in again";
const INTERRUPTED: &str = "ChatGPT sign-in refresh was interrupted; sign in again";
const CONFLICT: &str = "Codex credential store conflicts with the refresh in progress";

pub(crate) async fn reconcile_rotate(
    host: HostContext,
    token_url: String,
    account: AccountId,
    stale: SecretString,
    need: Need,
    skip_keychain: bool,
    fake: Option<&FakeKeychain>,
) -> Result<Access, PluginError> {
    let blob = loop {
        let lease = host.accounts().lease(&account, LEASE_WAIT).await?;
        let blob = stored(lease.secret())?;
        if !same_secret(&access_of(&blob)?, &stale) {
            return Ok(Access {
                token: access_of(&blob)?,
                account: account_of(&blob),
            });
        }
        let inflight = matches!(parse_markers(&blob), Some(RefreshMarkers::InFlight { .. }));
        drop(lease);
        if !inflight {
            break blob;
        }
        hofund_plugin_sdk::sleep(std::time::Duration::from_millis(50)).await;
    };

    if matches!(parse_markers(&blob), Some(RefreshMarkers::Invalid)) {
        return Err(PluginError::auth(REAUTH));
    }

    let home = blob.text("codex_home").map(PathBuf::from);
    let guard = match home.as_deref() {
        Some(path) => Some(acquire_guard(path, &host, &account).await?),
        None => None,
    };

    if matches!(parse_markers(&blob), Some(RefreshMarkers::CandidatePending { .. })) {
        return repair_pending(&host, &account, home.as_deref(), skip_keychain, fake).await;
    }

    if matches!(parse_markers(&blob), Some(RefreshMarkers::InFlight { .. })) {
        return Err(PluginError::auth(INTERRUPTED));
    }

    if let Some(path) = home.as_deref() {
        if let Some(access) = try_adopt_or_noop(
            &host,
            &account,
            &blob,
            path,
            &stale,
            need.clone(),
            skip_keychain,
            fake,
        )
        .await?
        {
            let _ = guard;
            return Ok(access);
        }
    }

    oauth_protocol(
        &host,
        &account,
        token_url,
        home.as_deref(),
        guard,
        stale,
        need,
        skip_keychain,
        fake,
    )
    .await
}

pub(crate) async fn persist_refresh(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    blob: &SecretObject,
    stale: &SecretString,
    fake: Option<&FakeKeychain>,
) {
    let Ok(_guard) = crate::codex_lock::try_acquire_home(home).await else {
        return;
    };
    let _ = write_store_when_stale_matches(home, host, skip_keychain, blob, stale, fake).await;
}

async fn read_account(host: &HostContext, account: &AccountId) -> Result<SecretObject, PluginError> {
    let secret = host
        .accounts()
        .secret(account)
        .await?
        .ok_or_else(|| PluginError::auth("no ChatGPT sign-in is stored for this account"))?;
    stored(&secret)
}

async fn commit_account(
    host: &HostContext,
    account: &AccountId,
    blob: SecretObject,
) -> Result<(), PluginError> {
    let lease = host.accounts().lease(account, LEASE_WAIT).await?;
    lease.commit(None, blob.to_secret()).await
}

async fn acquire_guard(
    home: &Path,
    host: &HostContext,
    account: &AccountId,
) -> Result<HomeGuard, PluginError> {
    let canonical = canonical_codex_home(home);
    let guard = crate::codex_lock::try_acquire_home(home)
        .await
        .map_err(|error| match error {
            HomeGuardError::Busy => PluginError::auth("Codex home directory is busy"),
            HomeGuardError::Io => PluginError::io("Codex home directory lock failed"),
        })?;
    let owners = home_owners(host, &canonical).await?;
    assert_unique_owner(&owners, account)?;
    Ok(guard)
}

enum LiveStore {
    Absent,
    Present {
        bytes: Vec<u8>,
        access: SecretString,
        refresh: Option<SecretString>,
        id_token: Option<SecretString>,
        store_account: Option<String>,
        fp: String,
        loc_fp: String,
    },
    Blocked,
}

async fn read_live_store(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    fake: Option<&FakeKeychain>,
) -> Result<LiveStore, PluginError> {
    let reader = StoreReader::new(Some(host), skip_keychain, fake);
    match reader.read_login(home, true).await {
        Ok((bytes, loc)) => {
            let setting = read_store_setting(home).map_err(|error| match error {
                StoreError::ConfigParse(message) => PluginError::config(message),
                _ => PluginError::io("cannot read Codex store settings"),
            })?;
            let loc_fp = fingerprint_location(loc.keychain, setting.mode);
            let (access, refresh, id_token, store_account) = tuple_fields_from_auth(&bytes)?;
            let fp = fingerprint_tuple(
                Some(&access),
                refresh.as_ref(),
                id_token.as_ref(),
                store_account.as_deref(),
            );
            Ok(LiveStore::Present {
                bytes,
                access,
                refresh,
                id_token,
                store_account,
                fp,
                loc_fp,
            })
        }
        Err(StoreError::SignedOut) => Ok(LiveStore::Absent),
        Err(StoreError::InvalidJson) => Ok(LiveStore::Blocked),
        Err(StoreError::Unreadable {
            cause: Some(KeychainError::Refused),
            ..
        })
        | Err(StoreError::Unreadable {
            cause: Some(KeychainError::Unreadable),
            ..
        }) => Ok(LiveStore::Blocked),
        Err(StoreError::Unreadable { .. }) => Ok(LiveStore::Blocked),
        Err(StoreError::ConfigParse(message)) => Err(PluginError::config(message)),
        Err(StoreError::FileWrite | StoreError::KeychainWrite) => {
            Err(PluginError::io("cannot read Codex sign-in"))
        }
    }
}

fn tuple_fields_from_auth(
    bytes: &[u8],
) -> Result<
    (
        SecretString,
        Option<SecretString>,
        Option<SecretString>,
        Option<String>,
    ),
    PluginError,
> {
    let access = access_token_from_auth(bytes)
        .ok_or_else(|| PluginError::auth("Codex sign-in on disk has no access token"))?;
    let value = hofund_plugin_sdk::json::from_slice::<hofund_plugin_sdk::json::Value>(bytes)
        .map_err(|_| PluginError::decode("Codex sign-in on disk is not JSON"))?;
    let tokens = value.get("tokens");
    let refresh = tokens
        .and_then(|t| t.get("refresh_token"))
        .and_then(|v| v.as_str())
        .map(|s| SecretString::from(s.to_owned()));
    let id_token = tokens
        .and_then(|t| t.get("id_token"))
        .and_then(|v| v.as_str())
        .map(|s| SecretString::from(s.to_owned()));
    let store_account = tokens
        .and_then(|t| t.get("account_id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| account_id_from_auth(bytes));
    Ok((access, refresh, id_token, store_account))
}

fn account_tuple_fp(blob: &SecretObject) -> String {
    fingerprint_tuple(
        blob.secret("access_token").as_ref(),
        blob.secret("refresh_token").as_ref(),
        blob.secret("id_token").as_ref(),
        blob.text("account_id"),
    )
}

fn need_requires_refresh(need: &Need, access: &SecretString, now: std::time::SystemTime) -> bool {
    match need {
        Need::Always => true,
        Need::Replace(stale) => same_secret(access, stale),
        Need::IfExpiring => !fresh(access, now),
    }
}

async fn try_adopt_or_noop(
    host: &HostContext,
    account: &AccountId,
    blob: &SecretObject,
    home: &Path,
    stale: &SecretString,
    need: Need,
    skip_keychain: bool,
    fake: Option<&FakeKeychain>,
) -> Result<Option<Access>, PluginError> {
    let live = read_live_store(home, host, skip_keychain, fake).await?;
    let now = host.clock().now();
    match live {
        LiveStore::Blocked => Err(PluginError::auth(
            "Codex keeps its sign-in where it cannot be refreshed from here",
        )),
        LiveStore::Absent => Ok(None),
        LiveStore::Present { ref access, .. } => {
            if !same_secret(access, stale) {
                return finish_adopt(host, account, blob, live).await.map(Some);
            }
            if !need_requires_refresh(&need, access, now) {
                let cleared = clear_markers(blob.clone());
                commit_account(host, account, cleared.clone()).await?;
                return Ok(Some(Access {
                    token: access.clone(),
                    account: account_of(&cleared),
                }));
            }
            Ok(None)
        }
    }
}

async fn finish_adopt(
    host: &HostContext,
    account: &AccountId,
    blob: &SecretObject,
    live: LiveStore,
) -> Result<Access, PluginError> {
    let LiveStore::Present {
        access,
        refresh,
        id_token,
        store_account,
        ..
    } = live
    else {
        return Err(PluginError::internal("adopt called without store tuple"));
    };
    let mut next = clear_markers(blob.clone())
        .with_secret("access_token", &access)
        .with_number("last_refresh", unix_seconds(host.clock().now()));
    if let Some(refresh) = refresh {
        next = next.with_secret("refresh_token", &refresh);
    }
    if let Some(id_token) = id_token {
        next = next.with_secret("id_token", &id_token);
    }
    if let Some(account_id) = store_account {
        next = next.with_text("account_id", &account_id);
    }
    commit_account(host, account, next.clone()).await?;
    if !fresh(&access, host.clock().now()) {
        return Err(PluginError::auth(REFRESH_NEEDED).without_cooldown());
    }
    Ok(Access {
        token: access,
        account: account_of(&next),
    })
}

async fn repair_pending(
    host: &HostContext,
    account: &AccountId,
    home: Option<&Path>,
    skip_keychain: bool,
    fake: Option<&FakeKeychain>,
) -> Result<Access, PluginError> {
    let blob = read_account(host, account).await?;
    let RefreshMarkers::CandidatePending {
        baseline_tuple_fp,
        candidate_tuple_fp,
        ..
    } = parse_markers(&blob).ok_or_else(|| PluginError::auth(CONFLICT))?
    else {
        return Err(PluginError::auth(CONFLICT));
    };

    let Some(home) = home else {
        return Err(PluginError::auth(CONFLICT));
    };

    let live = read_live_store(home, host, skip_keychain, fake).await?;
    let live_fp = match &live {
        LiveStore::Absent => fingerprint_tuple_absent().to_owned(),
        LiveStore::Present { fp, .. } => fp.clone(),
        LiveStore::Blocked => return Err(PluginError::auth(CONFLICT)),
    };

    let candidate_account_fp = account_tuple_fp(&blob);
    if live_fp == candidate_tuple_fp || live_fp == candidate_account_fp {
        return finish_repair_clear(host, account, blob).await;
    }
    if live_fp == baseline_tuple_fp
        || (baseline_tuple_fp == BASELINE_ABSENT && live_fp == BASELINE_ABSENT)
    {
        write_store_from_blob(home, host, skip_keychain, &blob, fake).await?;
        return finish_repair_clear(host, account, blob).await;
    }

    Err(PluginError::auth(CONFLICT))
}

async fn finish_repair_clear(
    host: &HostContext,
    account: &AccountId,
    blob: SecretObject,
) -> Result<Access, PluginError> {
    let access = access_of(&blob)?;
    let cleared = clear_markers(blob);
    commit_account(host, account, cleared.clone()).await?;
    if !fresh(&access, host.clock().now()) {
        return Err(PluginError::auth(REFRESH_NEEDED).without_cooldown());
    }
    Ok(Access {
        token: access,
        account: account_of(&cleared),
    })
}

async fn oauth_protocol(
    host: &HostContext,
    account: &AccountId,
    token_url: String,
    home: Option<&Path>,
    guard: Option<HomeGuard>,
    stale: SecretString,
    _need: Need,
    skip_keychain: bool,
    fake: Option<&FakeKeychain>,
) -> Result<Access, PluginError> {
    let _guard = guard;
    let mut blob = read_account(host, account).await?;
    if !same_secret(&access_of(&blob)?, &stale) {
        return Ok(Access {
            token: access_of(&blob)?,
            account: account_of(&blob),
        });
    }

    let canonical = home
        .map(canonical_codex_home)
        .unwrap_or_else(|| blob.text("codex_home").unwrap_or("").to_owned());

    let (baseline_fp, store_loc_fp) = if let Some(path) = home {
        match read_live_store(path, host, skip_keychain, fake).await? {
            LiveStore::Blocked => {
                return Err(PluginError::auth(
                    "Codex keeps its sign-in where it cannot be refreshed from here",
                ));
            }
            LiveStore::Absent => {
                if account_tuple_fp(&blob) != fingerprint_tuple_absent()
                    && parse_markers(&blob).is_none()
                {
                    // account had tokens; store empty is fine for first write
                }
                (
                    fingerprint_tuple_absent().to_owned(),
                    fingerprint_location(
                        false,
                        read_store_setting(path)
                            .map_err(|error| match error {
                                StoreError::ConfigParse(message) => PluginError::config(message),
                                _ => PluginError::io("cannot read Codex store settings"),
                            })?
                            .mode,
                    ),
                )
            }
            LiveStore::Present { fp, loc_fp, .. } => (fp, loc_fp),
        }
    } else {
        (account_tuple_fp(&blob), String::new())
    };

    let attempt_id =
        hofund_plugin_sdk::uuid::v4().map_err(|_| PluginError::io("cannot start Codex refresh"))?;
    blob = with_inflight(
        blob,
        &attempt_id,
        &canonical,
        &store_loc_fp,
        &baseline_fp,
    );
    commit_account(host, account, blob.clone()).await?;

    blob = read_account(host, account).await?;
    let refresh_token = blob
        .secret("refresh_token")
        .ok_or_else(|| PluginError::auth("the stored ChatGPT sign-in has no refresh token"))?;
    let body = SecretObject::new()
        .with_text("client_id", CLIENT_ID)
        .with_text("grant_type", "refresh_token")
        .with_secret("refresh_token", &refresh_token)
        .with_text("scope", REFRESH_SCOPE)
        .to_body();
    let reply = Call::post(token_url).json(body).fetch(host.http()).await?;
    if !(200..300).contains(&reply.status()) {
        let error = token_error(reply.status(), reply.body());
        if error.kind() == PluginErrorKind::Auth {
            commit_account(host, account, clear_markers(blob).with_flag(LAPSED, true)).await?;
        } else {
            // Unknown / timeout: keep InFlight marker (already committed).
        }
        return Err(error);
    }

    let now = host.clock().now();
    let set = parse_token_set(reply.body(), now)
        .map_err(|error| PluginError::decode(error.message()))?
        .or_refresh(blob.secret("refresh_token").as_ref());
    let account_label = blob.text("account_id").map(str::to_owned);
    let candidate_fp = fingerprint_tuple(
        Some(set.access()),
        set.refresh(),
        set.id_token(),
        account_label.as_deref(),
    );
    blob = read_account(host, account).await?;
    blob = with_pending(
        blob,
        &candidate_fp,
        set.access(),
        set.refresh(),
        set.id_token(),
        account_label.as_deref(),
    );
    commit_account(host, account, blob.clone()).await?;

    if let Some(path) = home {
        let live = read_live_store(path, host, skip_keychain, fake).await?;
        let live_fp = match live {
            LiveStore::Absent => BASELINE_ABSENT.to_owned(),
            LiveStore::Present { fp, .. } => fp,
            LiveStore::Blocked => return Err(PluginError::auth(CONFLICT)),
        };
        if live_fp != baseline_fp {
            return Err(PluginError::auth(CONFLICT));
        }
        write_store_from_blob(path, host, skip_keychain, &blob, fake).await?;
    }

    blob = read_account(host, account).await?;
    let mut cleared = clear_markers(blob)
        .with_secret("access_token", set.access())
        .with_number("last_refresh", unix_seconds(now));
    if let Some(refresh) = set.refresh() {
        cleared = cleared.with_secret("refresh_token", refresh);
    }
    if let Some(id_token) = set.id_token() {
        cleared = cleared.with_secret("id_token", id_token);
    }
    let account_id = account_of(&cleared);
    commit_account(host, account, cleared).await?;
    Ok(Access {
        token: set.access().clone(),
        account: account_id,
    })
}

async fn write_store_from_blob(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    blob: &SecretObject,
    fake: Option<&FakeKeychain>,
) -> Result<(), PluginError> {
    let patch = auth_patch_from_blob(blob);
    let reader = StoreReader::new(Some(host), skip_keychain, fake);
    let merged = match reader.read_login(home, true).await {
        Ok((base, _)) => merge_auth_tokens(&base, &patch)?,
        Err(StoreError::SignedOut) => {
            merge_auth_tokens(br#"{"auth_mode":"chatgpt","tokens":{}}"#, &patch)?
        }
        Err(error) => return Err(map_store(error)),
    };
    reader.save_login(home, &merged).await.map_err(map_store)
}

async fn write_store_when_stale_matches(
    home: &Path,
    host: &HostContext,
    skip_keychain: bool,
    blob: &SecretObject,
    stale: &SecretString,
    fake: Option<&FakeKeychain>,
) -> Result<(), PluginError> {
    let reader = StoreReader::new(Some(host), skip_keychain, fake);
    let patch = auth_patch_from_blob(blob);
    match reader.read_login(home, true).await {
        Ok((base, _)) => {
            let Some(current) = access_token_from_auth(&base) else {
                return Ok(());
            };
            if !same_secret(&current, stale) {
                return Ok(());
            }
            let merged = merge_auth_tokens(&base, &patch)?;
            reader.save_login(home, &merged).await.map_err(map_store)?;
        }
        Err(StoreError::SignedOut) => {
            let merged =
                merge_auth_tokens(br#"{"auth_mode":"chatgpt","tokens":{}}"#, &patch)?;
            reader.save_login(home, &merged).await.map_err(map_store)?;
        }
        Err(_) => {}
    }
    Ok(())
}

fn auth_patch_from_blob(blob: &SecretObject) -> AuthPatch {
    AuthPatch {
        access_token: blob
            .secret("access_token")
            .map(|t| secret_text(&t).to_owned()),
        refresh_token: blob
            .secret("refresh_token")
            .map(|t| secret_text(&t).to_owned()),
        id_token: blob
            .secret("id_token")
            .map(|t| secret_text(&t).to_owned()),
        chatgpt_mode: true,
    }
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
