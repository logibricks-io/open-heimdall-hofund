//! Plaintext file account store.
//!
//! `hofund-secrets` resolves environment specs and the gateway token file. It
//! does not talk to an OS keychain, so this PR cannot store secret material
//! there. The file holds the account rows, including the secret, as JSON.
//! The mode is `0600`. The type name is [`PlaintextFileAccountStore`] so that
//! trade-off stays visible. Encrypted storage (metadata in the file, secret
//! refs in the keychain) is a prerequisite of RFC 0001 pull request H.
//! Plugins do not open the file.
//!
//! The cache is an [`Arc`] of the parsed rows. A write drops it. A read
//! reloads when the file's mtime, length, or inode changes, so a replace
//! from another process is visible. Writes take an exclusive advisory lock
//! on a sibling lock file. Reads take a shared lock.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use hofund_plugin_api::kernel::{self, AccountBackend};
use hofund_plugin_api::{Account, AccountId, PluginError, PluginId, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A failure while reading or writing the account file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The file could not be read, locked, or replaced.
    #[error("{0}")]
    Io(String),
    /// The file is not the store format.
    #[error("{0}")]
    Format(String),
}

impl From<StoreError> for PluginError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Format(message) => PluginError::config(message),
            StoreError::Io(message) => PluginError::io(message),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Row {
    plugin: String,
    account: String,
    label: String,
    #[serde(
        serialize_with = "serialize_secret",
        deserialize_with = "deserialize_secret"
    )]
    secret: SecretString,
}

fn serialize_secret<S>(secret: &SecretString, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(kernel::expose_secret(secret))
}

fn deserialize_secret<'de, D>(deserializer: D) -> Result<SecretString, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(SecretString::from(String::deserialize(deserializer)?))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

struct Snapshot {
    stamp: Stamp,
    rows: Vec<Row>,
}

/// Accounts in a mode `0600` JSON file.
///
/// The secret is a [`SecretString`] in memory. [`Debug`] prints the path and
/// not the rows. The bytes on disk are still the secret: this store is
/// plaintext until the keychain backend in pull request H.
pub struct PlaintextFileAccountStore {
    path: PathBuf,
    cache: Arc<Mutex<Option<Arc<Snapshot>>>>,
}

impl std::fmt::Debug for PlaintextFileAccountStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlaintextFileAccountStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Clone for PlaintextFileAccountStore {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            cache: Arc::clone(&self.cache),
        }
    }
}

impl PlaintextFileAccountStore {
    /// Opens or creates the store at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent directory or the file cannot be created.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        if !path.exists() {
            write_rows(&path, &[])?;
        }
        set_mode(&path)?;
        Ok(Self {
            path,
            cache: Arc::new(Mutex::new(None)),
        })
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_else(|| "accounts".into());
        name.push(".lock");
        self.path.with_file_name(name)
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
        let rows = read_rows(&self.path)?;
        let snapshot = Arc::new(Snapshot { stamp, rows });
        let mut guard = self.cache.lock().map_err(|_| poisoned())?;
        *guard = Some(Arc::clone(&snapshot));
        Ok(snapshot)
    }

    fn store(&self, rows: Vec<Row>) -> Result<(), StoreError> {
        write_rows(&self.path, &rows)?;
        let mut guard = self.cache.lock().map_err(|_| poisoned())?;
        *guard = None;
        Ok(())
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

fn read_rows(path: &Path) -> Result<Vec<Row>, StoreError> {
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

fn write_rows(path: &Path, rows: &[Row]) -> Result<(), StoreError> {
    let bytes =
        serde_json::to_vec_pretty(rows).map_err(|error| StoreError::Format(error.to_string()))?;
    let tmp = path.with_extension("tmp");
    {
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    fs::rename(&tmp, path).map_err(|error| StoreError::Io(error.to_string()))?;
    sync_parent(path)?;
    set_mode(path)?;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), StoreError> {
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

fn set_mode(path: &Path) -> Result<(), StoreError> {
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

fn account_from(row: &Row) -> Result<Account, PluginError> {
    let plugin = PluginId::try_from(row.plugin.as_str())
        .map_err(|error| PluginError::config(error.to_string()))?;
    let id = AccountId::try_from(row.account.as_str())
        .map_err(|error| PluginError::config(error.to_string()))?;
    Account::from_stored(plugin, id, row.label.clone())
}

#[async_trait]
impl AccountBackend for PlaintextFileAccountStore {
    async fn list(&self, plugin: &PluginId) -> Result<Vec<Account>, PluginError> {
        let store = self.clone();
        let plugin = plugin.clone();
        run_blocking(move || {
            let snapshot = store.with_lock(true, || store.load())?;
            let mut accounts = Vec::new();
            for row in &snapshot.rows {
                if row.plugin == plugin.as_str() {
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
                let mut rows = snapshot.rows.clone();
                rows.retain(|row| {
                    !(row.plugin == account.plugin().as_str()
                        && row.account == account.id().as_str())
                });
                rows.push(Row {
                    plugin: account.plugin().as_str().to_owned(),
                    account: account.id().as_str().to_owned(),
                    label: account.label().to_owned(),
                    secret,
                });
                store.store(rows)
            })?;
            Ok(())
        })
        .await
    }

    async fn remove(&self, plugin: &PluginId, id: &AccountId) -> Result<(), PluginError> {
        let store = self.clone();
        let plugin = plugin.clone();
        let id = id.clone();
        run_blocking(move || {
            store.with_lock(false, || {
                let snapshot = store.load()?;
                let mut rows = snapshot.rows.clone();
                rows.retain(|row| !(row.plugin == plugin.as_str() && row.account == id.as_str()));
                store.store(rows)
            })?;
            Ok(())
        })
        .await
    }

    async fn secret(
        &self,
        plugin: &PluginId,
        id: &AccountId,
    ) -> Result<Option<SecretString>, PluginError> {
        let store = self.clone();
        let plugin = plugin.clone();
        let id = id.clone();
        run_blocking(move || {
            let snapshot = store.with_lock(true, || store.load())?;
            Ok(snapshot.rows.iter().find_map(|row| {
                (row.plugin == plugin.as_str() && row.account == id.as_str())
                    .then(|| row.secret.clone())
            }))
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
