//! File-backed account store.
//!
//! PR B's store is a file created with mode `0600` plus an in-memory cache.
//! The cache is dropped on every write. A read also reloads when the file
//! mtime changes, so a write from another process is visible. The OS keychain
//! can replace this type later. The daemon does not call it yet.
//!
//! Writes take an advisory lock on a sibling lock file ([`std::fs::File::lock`])
//! so a daemon refresh and a CLI refresh do not tear the file.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use hofund_plugin_api::kernel::{self, AccountBackend};
use hofund_plugin_api::{Account, AccountId, PluginError, PluginId, SecretString};
use serde::{Deserialize, Serialize};

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
        PluginError::config(error.to_string())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Row {
    plugin: String,
    account: String,
    label: String,
    secret: String,
}

#[derive(Clone, Debug)]
struct Snapshot {
    modified: Option<SystemTime>,
    rows: Vec<Row>,
}

/// Accounts on disk, with a cache that drops on write.
#[derive(Debug)]
pub struct FileAccountStore {
    path: PathBuf,
    cache: std::sync::Mutex<Option<Snapshot>>,
}

impl FileAccountStore {
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
            cache: std::sync::Mutex::new(None),
        })
    }

    fn lock_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| "accounts".into());
        name.push(".lock");
        self.path.with_file_name(name)
    }

    fn with_lock<T>(&self, body: impl FnOnce() -> Result<T, StoreError>) -> Result<T, StoreError> {
        let lock_path = self.lock_path();
        if let Some(parent) = lock_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.lock()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        let result = body();
        file.unlock()
            .map_err(|error| StoreError::Io(error.to_string()))?;
        result
    }

    fn load(&self) -> Result<Snapshot, StoreError> {
        let modified = fs::metadata(&self.path)
            .and_then(|meta| meta.modified())
            .ok();
        {
            let guard = self
                .cache
                .lock()
                .map_err(|_| StoreError::Io("account cache lock is poisoned".to_owned()))?;
            if let Some(cached) = guard.as_ref()
                && cached.modified == modified
            {
                return Ok(cached.clone());
            }
        }
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => return Err(StoreError::Io(error.to_string())),
        };
        let rows = if text.trim().is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(&text).map_err(|error| StoreError::Format(error.to_string()))?
        };
        let snapshot = Snapshot { modified, rows };
        let mut guard = self
            .cache
            .lock()
            .map_err(|_| StoreError::Io("account cache lock is poisoned".to_owned()))?;
        *guard = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn store(&self, rows: Vec<Row>) -> Result<(), StoreError> {
        write_rows(&self.path, &rows)?;
        set_mode(&self.path)?;
        let mut guard = self
            .cache
            .lock()
            .map_err(|_| StoreError::Io("account cache lock is poisoned".to_owned()))?;
        *guard = None;
        Ok(())
    }
}

fn write_rows(path: &Path, rows: &[Row]) -> Result<(), StoreError> {
    let bytes =
        serde_json::to_vec_pretty(rows).map_err(|error| StoreError::Format(error.to_string()))?;
    let tmp = path.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        set_mode(&tmp)?;
        file.write_all(&bytes)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        file.write_all(b"\n")
            .map_err(|error| StoreError::Io(error.to_string()))?;
    }
    fs::rename(&tmp, path).map_err(|error| StoreError::Io(error.to_string()))
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
    Account::new(plugin, id, row.label.clone())
}

#[async_trait]
impl AccountBackend for FileAccountStore {
    async fn list(&self, plugin: &PluginId) -> Result<Vec<Account>, PluginError> {
        let snapshot = self.with_lock(|| self.load())?;
        let mut accounts = Vec::new();
        for row in snapshot.rows {
            if row.plugin == plugin.as_str() {
                accounts.push(account_from(&row)?);
            }
        }
        Ok(accounts)
    }

    async fn upsert(&self, account: Account, secret: SecretString) -> Result<(), PluginError> {
        self.with_lock(|| {
            let mut snapshot = self.load()?;
            snapshot.rows.retain(|row| {
                !(row.plugin == account.plugin().as_str() && row.account == account.id().as_str())
            });
            snapshot.rows.push(Row {
                plugin: account.plugin().as_str().to_owned(),
                account: account.id().as_str().to_owned(),
                label: account.label().to_owned(),
                secret: kernel::expose_secret(&secret).to_owned(),
            });
            self.store(snapshot.rows)
        })?;
        Ok(())
    }

    async fn remove(&self, plugin: &PluginId, id: &AccountId) -> Result<(), PluginError> {
        self.with_lock(|| {
            let mut snapshot = self.load()?;
            snapshot
                .rows
                .retain(|row| !(row.plugin == plugin.as_str() && row.account == id.as_str()));
            self.store(snapshot.rows)
        })?;
        Ok(())
    }

    async fn secret(
        &self,
        plugin: &PluginId,
        id: &AccountId,
    ) -> Result<Option<SecretString>, PluginError> {
        let snapshot = self.with_lock(|| self.load())?;
        Ok(snapshot.rows.into_iter().find_map(|row| {
            (row.plugin == plugin.as_str() && row.account == id.as_str())
                .then(|| SecretString::from(row.secret))
        }))
    }
}
