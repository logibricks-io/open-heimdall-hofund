//! Encrypted account storage.
//!
//! Account metadata lives in a mode `0600` JSON file. Secret values live in
//! the OS credential store, or in an authenticated encrypted file when that
//! store is unavailable. A legacy plaintext file is migrated under the same
//! exclusive cross-process lock used by ordinary writes.
//!
//! The cache is an [`Arc`] of parsed metadata rows with a per-row secret cache.
//! Every write drops it. A read reloads when the metadata file's mtime, length,
//! or inode changes. Secret reads route through each row's stored reference.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use hofund_plugin_api::kernel::AccountBackend;
use hofund_plugin_api::{
    Account, AccountId, PluginError, PluginId, PluginKey, PluginKind, SecretString,
};
use serde::{Deserialize, Deserializer, Serialize};
use zeroize::Zeroizing;

use crate::account_secrets::{AccountSecretBackend, default_backend};

/// The selected account secret backend for new writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountStorageKind {
    /// The platform-native OS credential store.
    OsKeychain,
    /// XChaCha20-Poly1305 records in a mode `0600` sibling file.
    EncryptedFile,
}

impl AccountStorageKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OsKeychain => "os-keychain",
            Self::EncryptedFile => "encrypted-file",
        }
    }
}

impl std::fmt::Display for AccountStorageKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A failure while reading or writing account metadata or secrets.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// A metadata, key, or lock file operation failed.
    #[error("{0}")]
    Io(String),
    /// A metadata or encrypted record is not in the store format.
    #[error("{0}")]
    Format(String),
    /// A secret backend rejected an operation for one account.
    #[error("account `{plugin}/{account}` {backend} backend: {message}")]
    Backend {
        /// The backend that rejected the operation.
        backend: AccountStorageKind,
        /// The plugin that owns the account.
        plugin: String,
        /// The account id.
        account: String,
        /// A secret-free description of the failure.
        message: String,
    },
}

impl From<StoreError> for PluginError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Format(message) => PluginError::config(message),
            StoreError::Io(message) => PluginError::io(message),
            StoreError::Backend {
                backend,
                plugin,
                account,
                message,
            } => PluginError::io(format!(
                "account `{plugin}/{account}` {backend} backend: {message}"
            )),
        }
    }
}

pub(crate) fn backend_error(
    backend: AccountStorageKind,
    plugin: &str,
    account: &str,
    message: impl Into<String>,
) -> StoreError {
    StoreError::Backend {
        backend,
        plugin: plugin.to_owned(),
        account: account.to_owned(),
        message: message.into(),
    }
}

fn legacy_account_kind() -> String {
    PluginKind::Subscription.as_str().to_owned()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetadataRow {
    #[serde(default = "legacy_account_kind")]
    kind: String,
    plugin: String,
    account: String,
    label: String,
    secret_ref: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRow {
    #[serde(default = "legacy_account_kind")]
    kind: String,
    plugin: String,
    account: String,
    label: String,
    #[serde(deserialize_with = "deserialize_secret")]
    secret: SecretString,
}

fn deserialize_secret<'de, D>(deserializer: D) -> Result<SecretString, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(SecretString::from(String::deserialize(deserializer)?))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DiskRow {
    Metadata(MetadataRow),
    Legacy(LegacyRow),
}

type RowCacheKey = (String, String, String);

struct Snapshot {
    stamp: Stamp,
    rows: Vec<MetadataRow>,
    secrets: Mutex<BTreeMap<RowCacheKey, SecretString>>,
}

impl Snapshot {
    fn secret(
        &self,
        row: &MetadataRow,
        backend: &Arc<dyn AccountSecretBackend>,
    ) -> Result<SecretString, StoreError> {
        validate_ids(&row.kind, &row.plugin, &row.account)?;
        let key = (row.kind.clone(), row.plugin.clone(), row.account.clone());
        if let Some(secret) = self
            .secrets
            .lock()
            .map_err(|_| poisoned())?
            .get(&key)
            .cloned()
        {
            return Ok(secret);
        }
        backend.prepare()?;
        let secret = backend.get(&row.kind, &row.plugin, &row.account, &row.secret_ref)?;
        self.secrets
            .lock()
            .map_err(|_| poisoned())?
            .insert(key, secret.clone());
        Ok(secret)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

/// Accounts whose metadata is in JSON and whose secrets are encrypted at rest.
///
/// [`Debug`] prints only the path. The fallback key file protects against
/// accidental disclosure or backup of the ciphertext alone; it does not protect
/// against an attacker who can read both files as the user.
pub struct AccountStore {
    path: PathBuf,
    backend: Arc<dyn AccountSecretBackend>,
    cache: Arc<Mutex<Option<Arc<Snapshot>>>>,
    #[cfg(test)]
    migration_failure: Option<MigrationFailure>,
}

impl std::fmt::Debug for AccountStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Clone for AccountStore {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            backend: Arc::clone(&self.backend),
            cache: Arc::clone(&self.cache),
            #[cfg(test)]
            migration_failure: self.migration_failure,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum MigrationFailure {
    Backup,
    Metadata,
    Cleanup,
}

impl AccountStore {
    /// Opens or creates the store at `path`, migrating legacy plaintext rows.
    ///
    /// On first write, the OS credential store is probed. If it is unusable,
    /// the reason is logged without secret material and the encrypted-file
    /// backend is selected for new writes. Existing rows always use their
    /// stored references. Opening an empty store does not probe.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a file or the selected secret backend fails.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        Self::open_with_backend(&path, default_backend(&path))
    }

    pub(crate) fn open_with_backend(
        path: impl Into<PathBuf>,
        backend: Arc<dyn AccountSecretBackend>,
    ) -> Result<Self, StoreError> {
        Self::open_inner(path.into(), backend, None)
    }

    #[cfg(test)]
    fn open_with_migration_failure(
        path: impl Into<PathBuf>,
        backend: Arc<dyn AccountSecretBackend>,
        failure: MigrationFailure,
    ) -> Result<Self, StoreError> {
        Self::open_inner(path.into(), backend, Some(failure))
    }

    fn open_inner(
        path: PathBuf,
        backend: Arc<dyn AccountSecretBackend>,
        #[cfg_attr(not(test), allow(unused_variables))] failure: Option<MigrationFailure>,
    ) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        let store = Self {
            path,
            backend,
            cache: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            migration_failure: failure,
        };
        match create_empty_metadata(&store.path) {
            Ok(()) => sync_parent(&store.path)?,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                set_mode(&store.path)?;
                if store.migration_lock_required()? {
                    store.with_lock(false, || store.initialize())?;
                }
            }
            Err(error) => return Err(StoreError::Io(error.to_string())),
        }
        Ok(store)
    }

    /// The backend selected for new account writes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if native backend detection fails unexpectedly.
    pub fn storage_kind(&self) -> Result<AccountStorageKind, StoreError> {
        self.backend.kind()
    }

    fn migration_lock_required(&self) -> Result<bool, StoreError> {
        let original = Zeroizing::new(
            fs::read(&self.path).map_err(|error| StoreError::Io(error.to_string()))?,
        );
        let rows = parse_disk_rows(&original)?;
        Ok(rows.iter().any(|row| matches!(row, DiskRow::Legacy(_)))
            || legacy_backup_path(&self.path).exists()
            || temporary_path(&legacy_backup_path(&self.path)).exists()
            || legacy_temporary_path(&self.path).exists())
    }

    fn initialize(&self) -> Result<(), StoreError> {
        if !self.path.exists() {
            write_metadata(&self.path, &[])?;
        }
        set_mode(&self.path)?;
        let original = Zeroizing::new(
            fs::read(&self.path).map_err(|error| StoreError::Io(error.to_string()))?,
        );
        let rows = parse_disk_rows(&original)?;
        let has_legacy = rows.iter().any(|row| matches!(row, DiskRow::Legacy(_)));
        if has_legacy {
            self.backend.prepare()?;
            self.migrate(rows)?;
        } else {
            self.remove_legacy_copies()?;
        }
        Ok(())
    }

    fn migrate(&self, rows: Vec<DiskRow>) -> Result<(), StoreError> {
        let mut metadata = Vec::with_capacity(rows.len());
        for row in rows {
            match row {
                DiskRow::Metadata(row) => {
                    validate_ids(&row.kind, &row.plugin, &row.account)?;
                    metadata.push(row);
                }
                DiskRow::Legacy(row) => {
                    validate_ids(&row.kind, &row.plugin, &row.account)?;
                    let reference = self.backend.reference()?;
                    self.backend.set(
                        &row.kind,
                        &row.plugin,
                        &row.account,
                        reference,
                        row.secret,
                    )?;
                    metadata.push(MetadataRow {
                        kind: row.kind,
                        plugin: row.plugin,
                        account: row.account,
                        label: row.label,
                        secret_ref: reference.to_owned(),
                    });
                }
            }
        }

        #[cfg(test)]
        if self.migration_failure == Some(MigrationFailure::Backup) {
            return Err(StoreError::Io(
                "injected migration backup failure".to_owned(),
            ));
        }
        preserve_legacy_inode(&self.path)?;

        #[cfg(test)]
        if self.migration_failure == Some(MigrationFailure::Metadata) {
            return Err(StoreError::Io(
                "injected migration metadata failure".to_owned(),
            ));
        }
        write_metadata(&self.path, &metadata)?;

        #[cfg(test)]
        if self.migration_failure == Some(MigrationFailure::Cleanup) {
            return Err(StoreError::Io(
                "injected migration cleanup failure".to_owned(),
            ));
        }
        self.remove_legacy_copies()
    }

    fn remove_legacy_copies(&self) -> Result<(), StoreError> {
        secure_remove(&legacy_backup_path(&self.path))?;
        secure_remove(&temporary_path(&legacy_backup_path(&self.path)))?;
        secure_remove(&legacy_temporary_path(&self.path))?;
        Ok(())
    }

    fn lock_path(&self) -> PathBuf {
        sibling_path(&self.path, ".lock")
    }

    fn with_lock<T>(
        &self,
        shared: bool,
        body: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let lock_path = self.lock_path();
        if let Some(parent) = lock_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&lock_path)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        set_mode(&lock_path)?;
        if shared {
            file.lock_shared()
                .map_err(|error| StoreError::Io(error.to_string()))?;
        } else {
            file.lock()
                .map_err(|error| StoreError::Io(error.to_string()))?;
        }
        let result = body();
        file.unlock()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        result
    }

    fn load(&self) -> Result<Arc<Snapshot>, StoreError> {
        let stamp = stamp_of(&self.path);
        {
            let guard = self.cache.lock().map_err(|_| poisoned())?;
            if let Some(cached) = guard.as_ref()
                && cached.stamp == stamp
            {
                return Ok(Arc::clone(cached));
            }
        }
        let rows = read_metadata(&self.path)?;
        for row in &rows {
            validate_ids(&row.kind, &row.plugin, &row.account)?;
        }
        let snapshot = Arc::new(Snapshot {
            stamp,
            rows,
            secrets: Mutex::new(BTreeMap::new()),
        });
        let mut guard = self.cache.lock().map_err(|_| poisoned())?;
        *guard = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    fn invalidate(&self) -> Result<(), StoreError> {
        let mut guard = self.cache.lock().map_err(|_| poisoned())?;
        *guard = None;
        Ok(())
    }

    fn store(&self, rows: Vec<MetadataRow>) -> Result<(), StoreError> {
        self.invalidate()?;
        write_metadata(&self.path, &rows)
    }
}

fn poisoned() -> StoreError {
    StoreError::Io("account cache lock is poisoned".to_owned())
}

fn stamp_of(path: &Path) -> Stamp {
    let meta = fs::metadata(path).ok();
    Stamp {
        modified: meta.as_ref().and_then(|meta| meta.modified().ok()),
        len: meta.as_ref().map(fs::Metadata::len).unwrap_or(0),
        inode: meta.as_ref().map(file_id).unwrap_or(0),
    }
}

#[cfg(unix)]
fn file_id(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(windows)]
fn file_id(_meta: &fs::Metadata) -> u64 {
    // `MetadataExt::file_index` is still unstable (`windows_by_handle`,
    // rust-lang/rust#63010). The stamp still changes when the mtime or the
    // length changes, which a rewrite of this file does.
    0
}

#[cfg(not(any(unix, windows)))]
fn file_id(_meta: &fs::Metadata) -> u64 {
    0
}

fn parse_disk_rows(bytes: &[u8]) -> Result<Vec<DiskRow>, StoreError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    serde_json::from_slice(bytes).map_err(|error| StoreError::Format(error.to_string()))
}

fn read_metadata(path: &Path) -> Result<Vec<MetadataRow>, StoreError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(StoreError::Io(error.to_string())),
    };
    if text.trim().is_empty() {
        Ok(Vec::new())
    } else {
        serde_json::from_str(&text).map_err(|error| StoreError::Format(error.to_string()))
    }
}

fn create_empty_metadata(path: &Path) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(b"[]\n")?;
    file.sync_all()
}

fn write_metadata(path: &Path, rows: &[MetadataRow]) -> Result<(), StoreError> {
    let mut bytes =
        serde_json::to_vec_pretty(rows).map_err(|error| StoreError::Format(error.to_string()))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

pub(crate) fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| "accounts".into());
    name.push(suffix);
    path.with_file_name(name)
}

fn legacy_backup_path(path: &Path) -> PathBuf {
    sibling_path(path, ".legacy")
}

fn legacy_temporary_path(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

pub(crate) fn temporary_path(path: &Path) -> PathBuf {
    sibling_path(path, ".tmp")
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
    }
    let temporary = temporary_path(path);
    {
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        set_mode(&temporary)?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    fs::rename(&temporary, path).map_err(|error| StoreError::Io(error.to_string()))?;
    sync_parent(path)?;
    set_mode(path)?;
    Ok(())
}

fn preserve_legacy_inode(path: &Path) -> Result<(), StoreError> {
    let backup = legacy_backup_path(path);
    match fs::remove_file(&backup) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(StoreError::Io(error.to_string())),
    }
    if fs::hard_link(path, &backup).is_err() {
        fs::copy(path, &backup).map_err(|error| StoreError::Io(error.to_string()))?;
        set_mode(&backup)?;
    }
    sync_parent(path)
}

fn secure_remove(path: &Path) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(StoreError::Io(error.to_string())),
    };
    let len = file
        .metadata()
        .map_err(|error| StoreError::Io(error.to_string()))?
        .len();
    let zeros = [0_u8; 8192];
    let mut remaining = len;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(zeros.len() as u64)).unwrap_or(zeros.len());
        file.write_all(&zeros[..count])
            .map_err(|error| StoreError::Io(error.to_string()))?;
        remaining -= count as u64;
    }
    file.sync_all()
        .map_err(|error| StoreError::Io(error.to_string()))?;
    drop(file);
    fs::remove_file(path).map_err(|error| StoreError::Io(error.to_string()))?;
    sync_parent(path)
}

pub(crate) fn sync_parent(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        else {
            return Ok(());
        };
        let directory = File::open(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        directory
            .sync_all()
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub(crate) fn set_mode(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| StoreError::Io(error.to_string()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn validate_ids(
    kind: &str,
    plugin: &str,
    account: &str,
) -> Result<(PluginKey, AccountId), StoreError> {
    let kind = PluginKind::try_from(kind).map_err(|error| StoreError::Format(error.to_string()))?;
    let plugin =
        PluginId::try_from(plugin).map_err(|error| StoreError::Format(error.to_string()))?;
    let account =
        AccountId::try_from(account).map_err(|error| StoreError::Format(error.to_string()))?;
    Ok((PluginKey::new(kind, plugin), account))
}

fn account_from(row: &MetadataRow) -> Result<Account, PluginError> {
    let (plugin, id) = validate_ids(&row.kind, &row.plugin, &row.account)?;
    Account::from_stored_scoped(plugin, id, row.label.clone())
}

#[async_trait]
impl AccountBackend for AccountStore {
    async fn list(&self, plugin: &PluginKey) -> Result<Vec<Account>, PluginError> {
        let store = self.clone();
        let plugin = plugin.clone();
        run_blocking(move || {
            let snapshot = store.with_lock(true, || store.load())?;
            let mut accounts = Vec::new();
            for row in &snapshot.rows {
                let (key, _) = validate_ids(&row.kind, &row.plugin, &row.account)?;
                if key == plugin {
                    accounts.push(account_from(row)?);
                }
            }
            Ok(accounts)
        })
        .await
    }

    async fn upsert(&self, account: Account, secret: SecretString) -> Result<(), PluginError> {
        let store = self.clone();
        run_blocking(move || {
            store.with_lock(false, || {
                let snapshot = store.load()?;
                let key = account.key().clone();
                let kind = key.kind().as_str().to_owned();
                let plugin = key.id().as_str().to_owned();
                let id = account.id().as_str().to_owned();
                store.backend.prepare()?;
                let secret_ref = store.backend.reference()?.to_owned();
                store.invalidate()?;
                store
                    .backend
                    .set(&kind, &plugin, &id, &secret_ref, secret.clone())?;
                let mut rows = snapshot.rows.clone();
                rows.retain(|row| !(row.kind == kind && row.plugin == plugin && row.account == id));
                rows.push(MetadataRow {
                    kind,
                    plugin,
                    account: id,
                    label: account.label().to_owned(),
                    secret_ref,
                });
                store.store(rows)
            })?;
            Ok(())
        })
        .await
    }

    async fn remove(&self, plugin: &PluginKey, id: &AccountId) -> Result<(), PluginError> {
        let store = self.clone();
        let plugin = plugin.clone();
        let id = id.clone();
        run_blocking(move || {
            store.with_lock(false, || {
                let snapshot = store.load()?;
                let kind = plugin.kind().as_str();
                let plugin_id = plugin.id().as_str();
                let Some(row) = snapshot.rows.iter().find(|row| {
                    row.kind == kind && row.plugin == plugin_id && row.account == id.as_str()
                }) else {
                    return Ok(());
                };
                let reference = row.secret_ref.clone();
                store.backend.prepare()?;
                let mut rows = snapshot.rows.clone();
                rows.retain(|row| {
                    !(row.kind == kind && row.plugin == plugin_id && row.account == id.as_str())
                });
                store.store(rows)?;
                store
                    .backend
                    .delete(kind, plugin_id, id.as_str(), &reference)?;
                store.invalidate()
            })?;
            Ok(())
        })
        .await
    }

    async fn secret(
        &self,
        plugin: &PluginKey,
        id: &AccountId,
    ) -> Result<Option<SecretString>, PluginError> {
        let store = self.clone();
        let plugin = plugin.clone();
        let id = id.clone();
        run_blocking(move || {
            let snapshot = store.with_lock(true, || store.load())?;
            let Some(row) = snapshot.rows.iter().find(|row| {
                row.kind == plugin.kind().as_str()
                    && row.plugin == plugin.id().as_str()
                    && row.account == id.as_str()
            }) else {
                return Ok(None);
            };
            Ok(snapshot.secret(row, &store.backend).map(Some)?)
        })
        .await
    }
}

pub(crate) async fn run_blocking<T>(
    work: impl FnOnce() -> Result<T, PluginError> + Send + 'static,
) -> Result<T, PluginError>
where
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle
            .spawn_blocking(work)
            .await
            .map_err(|error| PluginError::internal(error.to_string()))?,
        Err(_) => work(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use hofund_plugin_api::kernel;

    use super::*;
    use crate::account_secrets::{KEYCHAIN_REFERENCE, MemoryAccountSecrets};

    const MARKER: &str = "account-secret-marker-7f3c91";

    struct TestDir(PathBuf);

    impl TestDir {
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    #[derive(Default)]
    struct FakeSecrets {
        inner: MemoryAccountSecrets,
        reads: AtomicUsize,
        fail_next_set: AtomicBool,
    }

    impl AccountSecretBackend for FakeSecrets {
        fn kind(&self) -> Result<AccountStorageKind, StoreError> {
            self.inner.kind()
        }

        fn reference(&self) -> Result<&'static str, StoreError> {
            self.inner.reference()
        }

        fn get(
            &self,
            kind: &str,
            plugin: &str,
            account: &str,
            reference: &str,
        ) -> Result<SecretString, StoreError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.inner.get(kind, plugin, account, reference)
        }

        fn set(
            &self,
            kind: &str,
            plugin: &str,
            account: &str,
            reference: &str,
            secret: SecretString,
        ) -> Result<(), StoreError> {
            if self.fail_next_set.swap(false, Ordering::Relaxed) {
                return Err(backend_error(
                    self.kind()?,
                    plugin,
                    account,
                    "injected credential write failure",
                ));
            }
            self.inner.set(kind, plugin, account, reference, secret)
        }

        fn delete(
            &self,
            kind: &str,
            plugin: &str,
            account: &str,
            reference: &str,
        ) -> Result<(), StoreError> {
            self.inner.delete(kind, plugin, account, reference)
        }
    }

    fn scratch() -> TestDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "hofund-accounts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("scratch");
        TestDir(path)
    }

    fn plugin() -> PluginId {
        PluginId::from_static("example-agent")
    }

    fn key(kind: PluginKind) -> PluginKey {
        PluginKey::new(kind, plugin())
    }

    fn account(id: &'static str) -> Account {
        scoped_account(PluginKind::Subscription, id, "Work")
    }

    fn scoped_account(kind: PluginKind, id: &'static str, label: &'static str) -> Account {
        Account::from_stored_scoped(key(kind), AccountId::from_static(id), label).expect("account")
    }

    fn write_legacy(path: &Path) {
        let text = format!(
            "[{{\"plugin\":\"example-agent\",\"account\":\"work\",\"label\":\"Work\",\"secret\":\"{MARKER}\"}}]\n"
        );
        fs::write(path, text).expect("legacy");
    }

    async fn assert_secret(store: &AccountStore) {
        let secret = store
            .secret(
                &key(PluginKind::Subscription),
                &AccountId::from_static("work"),
            )
            .await
            .expect("read")
            .expect("present");
        assert_eq!(kernel::expose_secret(&secret), MARKER);
    }

    #[tokio::test]
    async fn migration_is_idempotent_and_leaves_no_plaintext_in_metadata() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        write_legacy(&path);
        let backend = Arc::new(FakeSecrets::default());
        let store = AccountStore::open_with_backend(&path, backend.clone()).expect("migrate");
        assert_secret(&store).await;
        let bytes = fs::read(&path).expect("metadata");
        assert!(!String::from_utf8(bytes).expect("JSON").contains(MARKER));
        assert!(!legacy_backup_path(&path).exists());
        assert!(!temporary_path(&legacy_backup_path(&path)).exists());

        let reopened = AccountStore::open_with_backend(&path, backend).expect("reopen");
        assert_secret(&reopened).await;
        assert!(
            !fs::read_to_string(&path)
                .expect("metadata")
                .contains(MARKER)
        );
    }

    #[tokio::test]
    async fn migration_recovers_from_each_injected_commit_failure() {
        for failure in [
            MigrationFailure::Backup,
            MigrationFailure::Metadata,
            MigrationFailure::Cleanup,
        ] {
            let dir = scratch();
            let path = dir.join("accounts.json");
            write_legacy(&path);
            let backend = Arc::new(FakeSecrets::default());
            AccountStore::open_with_migration_failure(&path, backend.clone(), failure)
                .expect_err("injected failure");
            let reopened = AccountStore::open_with_backend(&path, backend).expect("recover");
            assert_secret(&reopened).await;
            assert!(
                !fs::read_to_string(&path)
                    .expect("metadata")
                    .contains(MARKER)
            );
            assert!(!legacy_backup_path(&path).exists());
        }
    }

    #[tokio::test]
    async fn migration_recovers_from_a_backend_write_failure() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        write_legacy(&path);
        let backend = Arc::new(FakeSecrets::default());
        backend.fail_next_set.store(true, Ordering::Relaxed);
        AccountStore::open_with_backend(&path, backend.clone()).expect_err("backend failure");
        assert!(fs::read_to_string(&path).expect("legacy").contains(MARKER));

        let reopened = AccountStore::open_with_backend(&path, backend).expect("recover");
        assert_secret(&reopened).await;
        assert!(
            !fs::read_to_string(&path)
                .expect("metadata")
                .contains(MARKER)
        );
    }

    #[tokio::test]
    async fn legacy_extension_temp_is_securely_removed_during_migration() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        write_legacy(&path);
        fs::write(legacy_temporary_path(&path), MARKER).expect("legacy temp");
        let backend = Arc::new(FakeSecrets::default());
        AccountStore::open_with_backend(&path, backend).expect("migrate");
        for entry in fs::read_dir(path.parent().expect("parent")).expect("directory") {
            let entry = entry.expect("entry");
            let bytes = fs::read(entry.path()).expect("file");
            assert!(
                !bytes
                    .windows(MARKER.len())
                    .any(|window| window == MARKER.as_bytes()),
                "plaintext remained in {:?}",
                entry.path()
            );
        }
    }

    #[tokio::test]
    async fn migration_zeroes_the_original_legacy_inode() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        write_legacy(&path);
        let witness = dir.join("witness");
        fs::hard_link(&path, &witness).expect("witness link");
        let backend = Arc::new(FakeSecrets::default());
        AccountStore::open_with_backend(&path, backend).expect("migrate");
        let bytes = fs::read(&witness).expect("witness");
        assert!(!bytes.is_empty());
        assert!(bytes.iter().all(|byte| *byte == 0));
    }

    #[tokio::test]
    async fn upsert_invalidates_secret_and_label_caches_for_both_stores() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        let backend = Arc::new(FakeSecrets::default());
        let first = AccountStore::open_with_backend(&path, backend.clone()).expect("first");
        let second = AccountStore::open_with_backend(&path, backend.clone()).expect("second");
        first
            .upsert(account("work"), SecretString::from(MARKER))
            .await
            .expect("upsert");
        assert_secret(&second).await;
        let reads = backend.reads.load(Ordering::Relaxed);
        assert_secret(&second).await;
        assert_eq!(backend.reads.load(Ordering::Relaxed), reads);

        first
            .upsert(
                scoped_account(PluginKind::Subscription, "work", "Updated"),
                SecretString::from("replacement-marker"),
            )
            .await
            .expect("replace");
        for store in [&first, &second] {
            let secret = store
                .secret(
                    &key(PluginKind::Subscription),
                    &AccountId::from_static("work"),
                )
                .await
                .expect("reload")
                .expect("present");
            assert_eq!(kernel::expose_secret(&secret), "replacement-marker");
            let accounts = store
                .list(&key(PluginKind::Subscription))
                .await
                .expect("list");
            assert_eq!(accounts[0].label(), "Updated");
        }
        assert!(backend.reads.load(Ordering::Relaxed) > reads);
    }

    #[tokio::test]
    async fn remove_deletes_the_secret_from_the_memory_backend() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        let backend = Arc::new(FakeSecrets::default());
        let store = AccountStore::open_with_backend(&path, backend.clone()).expect("open");
        store
            .upsert(account("work"), SecretString::from(MARKER))
            .await
            .expect("upsert");
        store
            .remove(
                &key(PluginKind::Subscription),
                &AccountId::from_static("work"),
            )
            .await
            .expect("remove");
        backend
            .get(
                PluginKind::Subscription.as_str(),
                plugin().as_str(),
                "work",
                KEYCHAIN_REFERENCE,
            )
            .expect_err("deleted secret");
        assert!(
            store
                .secret(
                    &key(PluginKind::Subscription),
                    &AccountId::from_static("work"),
                )
                .await
                .expect("secret")
                .is_none()
        );
    }

    #[tokio::test]
    async fn kind_scoped_rows_with_the_same_ids_have_distinct_secrets() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        let backend = Arc::new(FakeSecrets::default());
        let store = AccountStore::open_with_backend(&path, backend).expect("open");
        store
            .upsert(
                scoped_account(PluginKind::Subscription, "work", "Subscription"),
                SecretString::from("subscription-secret"),
            )
            .await
            .expect("subscription");
        store
            .upsert(
                scoped_account(PluginKind::Agent, "work", "Agent"),
                SecretString::from("agent-secret"),
            )
            .await
            .expect("agent");
        for (kind, expected) in [
            (PluginKind::Subscription, "subscription-secret"),
            (PluginKind::Agent, "agent-secret"),
        ] {
            let secret = store
                .secret(&key(kind), &AccountId::from_static("work"))
                .await
                .expect("secret")
                .expect("present");
            assert_eq!(kernel::expose_secret(&secret), expected);
            assert_eq!(store.list(&key(kind)).await.expect("list").len(), 1);
        }
    }

    #[tokio::test]
    async fn metadata_ids_are_validated_before_secret_access() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        fs::write(
            &path,
            format!(
                "[{{\"kind\":\"subscription\",\"plugin\":\"bad/plugin\",\"account\":\"work\",\"label\":\"Work\",\"secret_ref\":\"{KEYCHAIN_REFERENCE}\"}}]"
            ),
        )
        .expect("metadata");
        let backend = Arc::new(FakeSecrets::default());
        let store = AccountStore::open_with_backend(&path, backend.clone()).expect("open");
        store
            .secret(
                &key(PluginKind::Subscription),
                &AccountId::from_static("work"),
            )
            .await
            .expect_err("invalid plugin");
        assert_eq!(backend.reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn exclusive_lock_blocks_an_account_write() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        let store =
            AccountStore::open_with_backend(&path, Arc::new(MemoryAccountSecrets::default()))
                .expect("open");
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(store.lock_path())
            .expect("lock file");
        lock.lock().expect("lock");
        let writer = store.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            let result =
                runtime.block_on(writer.upsert(account("work"), SecretString::from(MARKER)));
            sender.send(result).expect("result");
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(receiver.try_recv().is_err(), "write crossed the lock");
        lock.unlock().expect("unlock");
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("finished")
            .expect("write");
    }

    #[tokio::test]
    async fn public_debug_display_and_backend_errors_do_not_contain_secrets() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        let backend = Arc::new(FakeSecrets::default());
        let store = AccountStore::open_with_backend(&path, backend.clone()).expect("open");
        backend.fail_next_set.store(true, Ordering::Relaxed);
        let error = store
            .upsert(account("work"), SecretString::from(MARKER))
            .await
            .expect_err("write failure");
        let rendered = format!("{store:?} {error} {error:?}");
        assert!(!rendered.contains(MARKER));
        assert!(rendered.contains("example-agent/work"));
        assert!(rendered.contains("os-keychain"));
        let kind = store.storage_kind().expect("kind");
        assert!(!format!("{kind} {kind:?}").contains(MARKER));
    }

    #[tokio::test]
    #[ignore = "touches the real OS credential store"]
    async fn real_backend_smoke_test() {
        let dir = scratch();
        let path = dir.join("accounts.json");
        let store = AccountStore::open(&path).expect("open");
        store
            .upsert(account("work"), SecretString::from(MARKER))
            .await
            .expect("upsert");
        assert_secret(&store).await;
        store
            .remove(
                &key(PluginKind::Subscription),
                &AccountId::from_static("work"),
            )
            .await
            .expect("remove");
    }
}
