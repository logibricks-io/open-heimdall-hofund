use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hofund_plugin_api::kernel;
use hofund_plugin_api::{PluginKind, SecretString};
#[cfg(target_os = "windows")]
use keyring_core::api::CredentialStoreApi;
use secrecy::{ExposeSecret, SecretBox};
use zeroize::{Zeroize, Zeroizing};

use crate::accounts::{
    AccountStorageKind, StoreError, backend_error, set_mode, sibling_path, sync_parent,
    write_atomic,
};

pub(crate) const KEYCHAIN_REFERENCE_V1: &str = "os-keychain:v1";
pub(crate) const KEYCHAIN_REFERENCE: &str = "os-keychain:v2";
pub(crate) const FILE_REFERENCE_V1: &str = "encrypted-file:v1";
pub(crate) const FILE_REFERENCE: &str = "encrypted-file:v2";
const KEYCHAIN_SERVICE: &str = "io.logibricks.hofund.accounts";
const MAGIC: &[u8; 8] = b"HOFACCT\0";
const FORMAT_VERSION: u8 = 2;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

pub(crate) trait AccountSecretBackend: Send + Sync {
    fn prepare(&self) -> Result<(), StoreError> {
        Ok(())
    }
    fn kind(&self) -> Result<AccountStorageKind, StoreError>;
    fn reference(&self) -> Result<&'static str, StoreError>;
    fn get(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError>;
    fn set(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError>;
    fn delete(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<(), StoreError>;
}

pub(crate) fn default_backend(metadata_path: &Path) -> Arc<dyn AccountSecretBackend> {
    Arc::new(AutoBackend {
        selected: Mutex::new(None),
        native: Mutex::new(None),
        native_error: None,
        file: Arc::new(EncryptedFileBackend::new(
            sibling_path(metadata_path, ".secrets"),
            sibling_path(metadata_path, ".key"),
        )),
    })
}

struct AutoBackend {
    selected: Mutex<Option<Arc<dyn AccountSecretBackend>>>,
    native: Mutex<Option<Arc<dyn AccountSecretBackend>>>,
    native_error: Option<String>,
    file: Arc<dyn AccountSecretBackend>,
}

impl AutoBackend {
    #[cfg(test)]
    fn with_backends(
        selected: Arc<dyn AccountSecretBackend>,
        native: Result<Arc<dyn AccountSecretBackend>, String>,
        file: Arc<dyn AccountSecretBackend>,
    ) -> Self {
        let (native, native_error) = match native {
            Ok(backend) => (Some(backend), None),
            Err(error) => (None, Some(error)),
        };
        Self {
            selected: Mutex::new(Some(selected)),
            native: Mutex::new(native),
            native_error,
            file,
        }
    }

    fn selection_lock(&self) -> StoreError {
        StoreError::Io("account backend selection lock is poisoned".to_owned())
    }

    fn native_backend(&self) -> Result<Arc<dyn AccountSecretBackend>, StoreError> {
        if let Some(error) = &self.native_error {
            return Err(StoreError::Io(error.clone()));
        }
        {
            let guard = self.native.lock().map_err(|_| self.selection_lock())?;
            if let Some(backend) = guard.as_ref() {
                return Ok(Arc::clone(backend));
            }
        }
        let backend: Arc<dyn AccountSecretBackend> =
            Arc::new(OsKeychainBackend::probe().map_err(StoreError::Io)?);
        let mut guard = self.native.lock().map_err(|_| self.selection_lock())?;
        Ok(Arc::clone(guard.get_or_insert(backend)))
    }

    fn select(&self) -> Result<Arc<dyn AccountSecretBackend>, StoreError> {
        {
            let guard = self.selected.lock().map_err(|_| self.selection_lock())?;
            if let Some(backend) = guard.as_ref() {
                return Ok(Arc::clone(backend));
            }
        }
        let candidate: Arc<dyn AccountSecretBackend> = match self.native_backend() {
            Ok(backend) => {
                tracing::debug!(backend = "os-keychain", "selected account secret backend");
                backend
            }
            Err(error) => {
                let reason = error.to_string();
                tracing::warn!(
                    backend = "encrypted-file",
                    reason = %reason,
                    "OS credential store unavailable; using encrypted account file"
                );
                Arc::clone(&self.file)
            }
        };
        let mut guard = self.selected.lock().map_err(|_| self.selection_lock())?;
        Ok(Arc::clone(guard.get_or_insert(candidate)))
    }

    fn for_reference(&self, reference: &str) -> Result<Arc<dyn AccountSecretBackend>, StoreError> {
        match reference {
            KEYCHAIN_REFERENCE_V1 | KEYCHAIN_REFERENCE => self.native_backend(),
            FILE_REFERENCE_V1 | FILE_REFERENCE => Ok(Arc::clone(&self.file)),
            _ => Err(StoreError::Format(
                "account secret reference has an unknown backend or version".to_owned(),
            )),
        }
    }
}

impl AccountSecretBackend for AutoBackend {
    fn prepare(&self) -> Result<(), StoreError> {
        self.select()?.prepare()
    }

    fn kind(&self) -> Result<AccountStorageKind, StoreError> {
        self.select()?.kind()
    }

    fn reference(&self) -> Result<&'static str, StoreError> {
        self.select()?.reference()
    }

    fn get(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        let backend = self.for_reference(reference)?;
        backend.get(kind, plugin, account, reference)
    }

    fn set(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        let backend = self.select()?;
        backend.prepare()?;
        backend.set(kind, plugin, account, reference, secret)
    }

    fn delete(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<(), StoreError> {
        let backend = self.for_reference(reference)?;
        backend.delete(kind, plugin, account, reference)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
type NativeEntry = keyring::Entry;
#[cfg(any(target_os = "macos", target_os = "linux"))]
type NativeError = keyring::Error;
#[cfg(target_os = "windows")]
type NativeEntry = keyring_core::Entry;
#[cfg(target_os = "windows")]
type NativeError = keyring_core::Error;

#[cfg(not(target_os = "windows"))]
#[derive(Clone, Copy)]
struct OsKeychainBackend;

#[cfg(target_os = "windows")]
struct OsKeychainBackend {
    store: Arc<windows_native_keyring_store::Store>,
}

fn native_reference_version(reference: &str) -> Option<u8> {
    match reference {
        KEYCHAIN_REFERENCE_V1 => Some(1),
        KEYCHAIN_REFERENCE => Some(2),
        _ => None,
    }
}

fn native_entry_name(kind: &str, plugin: &str, account: &str, version: u8) -> String {
    if version == 1 {
        format!("{plugin}/{account}")
    } else {
        format!("{kind}/{plugin}/{account}")
    }
}

impl OsKeychainBackend {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn probe() -> Result<Self, String> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, "backend-probe")
            .map_err(|error| error.to_string())?;
        match entry.get_secret() {
            Ok(mut secret) => {
                secret.zeroize();
                Ok(Self)
            }
            Err(keyring::Error::NoEntry) => Ok(Self),
            Err(error) => Err(error.to_string()),
        }
    }

    #[cfg(target_os = "windows")]
    fn probe() -> Result<Self, String> {
        let backend = Self {
            store: windows_native_keyring_store::Store::new().map_err(|error| error.to_string())?,
        };
        let entry = backend.entry("backend", "backend", "probe", KEYCHAIN_REFERENCE)?;
        match entry.get_secret() {
            Ok(mut secret) => {
                secret.zeroize();
                Ok(backend)
            }
            Err(keyring_core::Error::NoEntry) => Ok(backend),
            Err(error) => Err(error.to_string()),
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    fn probe() -> Result<Self, String> {
        Err("this platform has no configured native credential backend".to_owned())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn entry(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<NativeEntry, String> {
        let version = native_reference_version(reference).unwrap_or(2);
        keyring::Entry::new(
            KEYCHAIN_SERVICE,
            &native_entry_name(kind, plugin, account, version),
        )
        .map_err(|error| error.to_string())
    }

    #[cfg(target_os = "windows")]
    fn entry(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<NativeEntry, String> {
        let version = native_reference_version(reference).unwrap_or(2);
        self.store
            .build(
                KEYCHAIN_SERVICE,
                &native_entry_name(kind, plugin, account, version),
                None,
            )
            .map_err(|error| error.to_string())
    }

    fn check_reference(plugin: &str, account: &str, reference: &str) -> Result<u8, StoreError> {
        native_reference_version(reference).ok_or_else(|| {
            backend_error(
                AccountStorageKind::OsKeychain,
                plugin,
                account,
                "secret reference belongs to a different backend or format",
            )
        })
    }
}

impl AccountSecretBackend for OsKeychainBackend {
    fn kind(&self) -> Result<AccountStorageKind, StoreError> {
        Ok(AccountStorageKind::OsKeychain)
    }

    fn reference(&self) -> Result<&'static str, StoreError> {
        Ok(KEYCHAIN_REFERENCE)
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn get(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let entry = self
            .entry(kind, plugin, account, reference)
            .map_err(|error| {
                backend_error(AccountStorageKind::OsKeychain, plugin, account, error)
            })?;
        let plaintext = entry.get_secret().map_err(|error| {
            backend_error(
                AccountStorageKind::OsKeychain,
                plugin,
                account,
                error.to_string(),
            )
        })?;
        match String::from_utf8(plaintext) {
            Ok(secret) => Ok(SecretString::from(secret)),
            Err(error) => {
                error.into_bytes().zeroize();
                Err(backend_error(
                    AccountStorageKind::OsKeychain,
                    plugin,
                    account,
                    "stored secret is not UTF-8",
                ))
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    fn get(
        &self,
        _kind: &str,
        plugin: &str,
        account: &str,
        _reference: &str,
    ) -> Result<SecretString, StoreError> {
        Err(backend_error(
            AccountStorageKind::OsKeychain,
            plugin,
            account,
            "native credential backend is unavailable",
        ))
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn set(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let entry = self
            .entry(kind, plugin, account, reference)
            .map_err(|error| {
                backend_error(AccountStorageKind::OsKeychain, plugin, account, error)
            })?;
        entry
            .set_secret(kernel::expose_secret(&secret).as_bytes())
            .map_err(|error| {
                backend_error(
                    AccountStorageKind::OsKeychain,
                    plugin,
                    account,
                    error.to_string(),
                )
            })
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    fn set(
        &self,
        _kind: &str,
        plugin: &str,
        account: &str,
        _reference: &str,
        _secret: SecretString,
    ) -> Result<(), StoreError> {
        Err(backend_error(
            AccountStorageKind::OsKeychain,
            plugin,
            account,
            "native credential backend is unavailable",
        ))
    }

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    fn delete(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let entry = self
            .entry(kind, plugin, account, reference)
            .map_err(|error| {
                backend_error(AccountStorageKind::OsKeychain, plugin, account, error)
            })?;
        match entry.delete_credential() {
            Ok(()) | Err(NativeError::NoEntry) => Ok(()),
            Err(error) => Err(backend_error(
                AccountStorageKind::OsKeychain,
                plugin,
                account,
                error.to_string(),
            )),
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    fn delete(
        &self,
        _kind: &str,
        plugin: &str,
        account: &str,
        _reference: &str,
    ) -> Result<(), StoreError> {
        Err(backend_error(
            AccountStorageKind::OsKeychain,
            plugin,
            account,
            "native credential backend is unavailable",
        ))
    }
}

#[derive(Clone)]
struct CipherRow {
    version: u8,
    kind: String,
    plugin: String,
    account: String,
    nonce: [u8; NONCE_LEN],
    ciphertext: Vec<u8>,
}

struct EncryptedFileBackend {
    path: PathBuf,
    key_path: PathBuf,
    key: std::sync::OnceLock<SecretBox<[u8; KEY_LEN]>>,
}

impl std::fmt::Debug for EncryptedFileBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptedFileBackend")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn file_reference_version(reference: &str) -> Option<u8> {
    match reference {
        FILE_REFERENCE_V1 => Some(1),
        FILE_REFERENCE => Some(2),
        _ => None,
    }
}

impl EncryptedFileBackend {
    fn new(path: PathBuf, key_path: PathBuf) -> Self {
        Self {
            path,
            key_path,
            key: std::sync::OnceLock::new(),
        }
    }

    #[cfg(test)]
    fn open(path: PathBuf, key_path: PathBuf) -> Result<Self, StoreError> {
        let backend = Self::new(path, key_path);
        backend.prepare()?;
        Ok(backend)
    }

    #[cfg(test)]
    fn from_key(path: PathBuf, key: [u8; KEY_LEN]) -> Self {
        let cell = std::sync::OnceLock::new();
        let _ = cell.set(SecretBox::new(Box::new(key)));
        Self {
            path,
            key_path: PathBuf::new(),
            key: cell,
        }
    }

    fn cipher(&self, plugin: &str, account: &str) -> Result<XChaCha20Poly1305, StoreError> {
        if self.key.get().is_none() {
            let key = load_existing_key(&self.key_path).map_err(|error| {
                backend_error(
                    AccountStorageKind::EncryptedFile,
                    plugin,
                    account,
                    error.to_string(),
                )
            })?;
            let _ = self.key.set(key);
        }
        let key = self.key.get().ok_or_else(|| {
            backend_error(
                AccountStorageKind::EncryptedFile,
                plugin,
                account,
                "encryption key is unavailable",
            )
        })?;
        XChaCha20Poly1305::new_from_slice(key.expose_secret()).map_err(|_| {
            backend_error(
                AccountStorageKind::EncryptedFile,
                plugin,
                account,
                "encryption key has an invalid length",
            )
        })
    }

    fn check_reference(plugin: &str, account: &str, reference: &str) -> Result<u8, StoreError> {
        file_reference_version(reference).ok_or_else(|| {
            backend_error(
                AccountStorageKind::EncryptedFile,
                plugin,
                account,
                "secret reference belongs to a different backend or format",
            )
        })
    }

    fn read(&self, plugin: &str, account: &str) -> Result<Vec<CipherRow>, StoreError> {
        read_records(&self.path).map_err(|error| {
            backend_error(AccountStorageKind::EncryptedFile, plugin, account, error)
        })
    }

    fn write(&self, plugin: &str, account: &str, rows: &[CipherRow]) -> Result<(), StoreError> {
        let bytes = encode_records(rows).map_err(|error| {
            backend_error(AccountStorageKind::EncryptedFile, plugin, account, error)
        })?;
        write_atomic(&self.path, &bytes).map_err(|error| {
            backend_error(
                AccountStorageKind::EncryptedFile,
                plugin,
                account,
                error.to_string(),
            )
        })
    }
}

impl AccountSecretBackend for EncryptedFileBackend {
    fn prepare(&self) -> Result<(), StoreError> {
        if self.key.get().is_none() {
            let key = load_or_create_key(&self.path, &self.key_path)?;
            let _ = self.key.set(key);
        }
        Ok(())
    }

    fn kind(&self) -> Result<AccountStorageKind, StoreError> {
        Ok(AccountStorageKind::EncryptedFile)
    }

    fn reference(&self) -> Result<&'static str, StoreError> {
        Ok(FILE_REFERENCE)
    }

    fn get(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        let version = Self::check_reference(plugin, account, reference)?;
        let rows = self.read(plugin, account)?;
        let row = rows
            .iter()
            .find(|row| {
                row.version == version
                    && row.kind == kind
                    && row.plugin == plugin
                    && row.account == account
            })
            .ok_or_else(|| {
                backend_error(
                    AccountStorageKind::EncryptedFile,
                    plugin,
                    account,
                    "secret record not found",
                )
            })?;
        let plaintext = self
            .cipher(plugin, account)?
            .decrypt(
                XNonce::from_slice(&row.nonce),
                Payload {
                    msg: &row.ciphertext,
                    aad: associated_data(version, kind, plugin, account).as_bytes(),
                },
            )
            .map_err(|_| {
                backend_error(
                    AccountStorageKind::EncryptedFile,
                    plugin,
                    account,
                    "authentication failed; the key or record is not valid",
                )
            })?;
        match String::from_utf8(plaintext) {
            Ok(secret) => Ok(SecretString::from(secret)),
            Err(error) => {
                error.into_bytes().zeroize();
                Err(backend_error(
                    AccountStorageKind::EncryptedFile,
                    plugin,
                    account,
                    "decrypted secret is not UTF-8",
                ))
            }
        }
    }

    fn set(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        let version = Self::check_reference(plugin, account, reference)?;
        let mut rows = self.read(plugin, account)?;
        rows.retain(|row| {
            !(row.version == version
                && row.kind == kind
                && row.plugin == plugin
                && row.account == account)
        });
        let mut nonce = [0_u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|error| {
            backend_error(
                AccountStorageKind::EncryptedFile,
                plugin,
                account,
                format!("cannot generate a nonce: {error}"),
            )
        })?;
        let ciphertext = self
            .cipher(plugin, account)?
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: kernel::expose_secret(&secret).as_bytes(),
                    aad: associated_data(version, kind, plugin, account).as_bytes(),
                },
            )
            .map_err(|_| {
                backend_error(
                    AccountStorageKind::EncryptedFile,
                    plugin,
                    account,
                    "encryption failed",
                )
            })?;
        rows.push(CipherRow {
            version,
            kind: kind.to_owned(),
            plugin: plugin.to_owned(),
            account: account.to_owned(),
            nonce,
            ciphertext,
        });
        self.write(plugin, account, &rows)
    }

    fn delete(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<(), StoreError> {
        let version = Self::check_reference(plugin, account, reference)?;
        let mut rows = self.read(plugin, account)?;
        rows.retain(|row| {
            !(row.version == version
                && row.kind == kind
                && row.plugin == plugin
                && row.account == account)
        });
        self.write(plugin, account, &rows)
    }
}

fn associated_data(version: u8, kind: &str, plugin: &str, account: &str) -> String {
    if version == 1 {
        format!("hofund-account-secret\0format-v1\0{plugin}\0{account}")
    } else {
        format!("hofund-account-secret\0format-v2\0{kind}\0{plugin}\0{account}")
    }
}

fn load_existing_key(key_path: &Path) -> Result<SecretBox<[u8; KEY_LEN]>, StoreError> {
    read_key_file(key_path)?.ok_or_else(|| {
        StoreError::Format(format!(
            "account encryption key `{}` is missing",
            key_path.display()
        ))
    })
}

fn read_key_file(key_path: &Path) -> Result<Option<SecretBox<[u8; KEY_LEN]>>, StoreError> {
    match fs::symlink_metadata(key_path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(StoreError::Format(format!(
                    "account encryption key `{}` is not a regular file",
                    key_path.display()
                )));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o777 != 0o600 {
                    return Err(StoreError::Format(format!(
                        "account encryption key `{}` does not have mode 0600",
                        key_path.display()
                    )));
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error.to_string())),
    }
    let mut bytes = match fs::read(key_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::Io(error.to_string())),
    };
    if bytes.len() != KEY_LEN {
        bytes.zeroize();
        return Err(StoreError::Format(format!(
            "account encryption key `{}` is not {KEY_LEN} bytes",
            key_path.display()
        )));
    }
    let mut array = [0_u8; KEY_LEN];
    array.copy_from_slice(&bytes);
    bytes.zeroize();
    Ok(Some(boxed_key(array)))
}

fn boxed_key(mut array: [u8; KEY_LEN]) -> SecretBox<[u8; KEY_LEN]> {
    let secret = SecretBox::new(Box::new(array));
    array.zeroize();
    secret
}

fn load_or_create_key(
    secrets_path: &Path,
    key_path: &Path,
) -> Result<SecretBox<[u8; KEY_LEN]>, StoreError> {
    if let Some(key) = read_key_file(key_path)? {
        return Ok(key);
    }
    match fs::metadata(secrets_path) {
        Ok(metadata) if metadata.len() > 0 => {
            return Err(StoreError::Format(format!(
                "account encryption key `{}` is missing while encrypted records exist",
                key_path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(StoreError::Io(error.to_string())),
    }
    let mut key = Zeroizing::new([0_u8; KEY_LEN]);
    getrandom::getrandom(&mut key[..])
        .map_err(|error| StoreError::Io(format!("cannot generate account key: {error}")))?;
    publish_key(key_path, &key)
}

fn publish_key(
    key_path: &Path,
    key: &[u8; KEY_LEN],
) -> Result<SecretBox<[u8; KEY_LEN]>, StoreError> {
    let mut suffix = [0_u8; 8];
    getrandom::getrandom(&mut suffix)
        .map_err(|error| StoreError::Io(format!("cannot generate key temporary name: {error}")))?;
    let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = sibling_path(key_path, &format!(".tmp-{suffix}"));
    let write_result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| StoreError::Io(error.to_string()))?;
        set_mode(&temporary)?;
        file.write_all(key)
            .and_then(|()| file.sync_all())
            .map_err(|error| StoreError::Io(error.to_string()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        fs::remove_file(&temporary).ok();
        return Err(error);
    }
    match fs::hard_link(&temporary, key_path) {
        Ok(()) => {
            fs::remove_file(&temporary).map_err(|error| StoreError::Io(error.to_string()))?;
            sync_parent(key_path)?;
            Ok(boxed_key(*key))
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            fs::remove_file(&temporary).ok();
            sync_parent(key_path)?;
            load_existing_key(key_path)
        }
        Err(error) => {
            fs::remove_file(&temporary).ok();
            Err(StoreError::Io(error.to_string()))
        }
    }
}

fn read_records(path: &Path) -> Result<Vec<CipherRow>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    decode_records(&bytes)
}

fn decode_records(bytes: &[u8]) -> Result<Vec<CipherRow>, String> {
    let mut offset = 0;
    if take(bytes, &mut offset, MAGIC.len())? != MAGIC {
        return Err("encrypted account file has invalid magic".to_owned());
    }
    let container_version = take(bytes, &mut offset, 1)?[0];
    if container_version != 1 && container_version != FORMAT_VERSION {
        return Err("encrypted account file has an unsupported version".to_owned());
    }
    let count = read_u32(bytes, &mut offset)?;
    let mut rows = Vec::new();
    for _ in 0..count {
        let version = if container_version == 1 {
            1
        } else {
            take(bytes, &mut offset, 1)?[0]
        };
        if version != 1 && version != FORMAT_VERSION {
            return Err("encrypted account record has an unsupported version".to_owned());
        }
        let kind_len = if version == 1 {
            0
        } else {
            usize::from(read_u16(bytes, &mut offset)?)
        };
        let plugin_len = usize::from(read_u16(bytes, &mut offset)?);
        let account_len = usize::from(read_u16(bytes, &mut offset)?);
        let nonce_slice = take(bytes, &mut offset, NONCE_LEN)?;
        let nonce: [u8; NONCE_LEN] = nonce_slice
            .try_into()
            .map_err(|_| "encrypted account nonce has an invalid length".to_owned())?;
        let ciphertext_len = usize::try_from(read_u32(bytes, &mut offset)?)
            .map_err(|_| "encrypted account ciphertext is too large".to_owned())?;
        let kind = if version == 1 {
            PluginKind::Subscription.as_str().to_owned()
        } else {
            std::str::from_utf8(take(bytes, &mut offset, kind_len)?)
                .map_err(|_| "encrypted account kind is not UTF-8".to_owned())?
                .to_owned()
        };
        let plugin = std::str::from_utf8(take(bytes, &mut offset, plugin_len)?)
            .map_err(|_| "encrypted account plugin id is not UTF-8".to_owned())?
            .to_owned();
        let account = std::str::from_utf8(take(bytes, &mut offset, account_len)?)
            .map_err(|_| "encrypted account id is not UTF-8".to_owned())?
            .to_owned();
        let ciphertext = take(bytes, &mut offset, ciphertext_len)?.to_vec();
        rows.push(CipherRow {
            version,
            kind,
            plugin,
            account,
            nonce,
            ciphertext,
        });
    }
    if offset != bytes.len() {
        return Err("encrypted account file has trailing bytes".to_owned());
    }
    Ok(rows)
}

fn encode_records(rows: &[CipherRow]) -> Result<Vec<u8>, String> {
    let count = u32::try_from(rows.len()).map_err(|_| "too many account secrets".to_owned())?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.push(FORMAT_VERSION);
    bytes.extend_from_slice(&count.to_le_bytes());
    for row in rows {
        if row.version != 1 && row.version != FORMAT_VERSION {
            return Err("encrypted account record has an unsupported version".to_owned());
        }
        bytes.push(row.version);
        let kind_len =
            u16::try_from(row.kind.len()).map_err(|_| "account kind is too long".to_owned())?;
        let plugin_len = u16::try_from(row.plugin.len())
            .map_err(|_| "account plugin id is too long".to_owned())?;
        let account_len =
            u16::try_from(row.account.len()).map_err(|_| "account id is too long".to_owned())?;
        let ciphertext_len = u32::try_from(row.ciphertext.len())
            .map_err(|_| "account ciphertext is too large".to_owned())?;
        if row.version == FORMAT_VERSION {
            bytes.extend_from_slice(&kind_len.to_le_bytes());
        }
        bytes.extend_from_slice(&plugin_len.to_le_bytes());
        bytes.extend_from_slice(&account_len.to_le_bytes());
        bytes.extend_from_slice(&row.nonce);
        bytes.extend_from_slice(&ciphertext_len.to_le_bytes());
        if row.version == FORMAT_VERSION {
            bytes.extend_from_slice(row.kind.as_bytes());
        }
        bytes.extend_from_slice(row.plugin.as_bytes());
        bytes.extend_from_slice(row.account.as_bytes());
        bytes.extend_from_slice(&row.ciphertext);
    }
    Ok(bytes)
}

fn take<'a>(bytes: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| "encrypted account record length overflows".to_owned())?;
    let slice = bytes
        .get(*offset..end)
        .ok_or_else(|| "encrypted account record is truncated".to_owned())?;
    *offset = end;
    Ok(slice)
}

fn read_u16(bytes: &[u8], offset: &mut usize) -> Result<u16, String> {
    let array: [u8; 2] = take(bytes, offset, 2)?
        .try_into()
        .map_err(|_| "encrypted account record is truncated".to_owned())?;
    Ok(u16::from_le_bytes(array))
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, String> {
    let array: [u8; 4] = take(bytes, offset, 4)?
        .try_into()
        .map_err(|_| "encrypted account record is truncated".to_owned())?;
    Ok(u32::from_le_bytes(array))
}

#[derive(Default)]
pub(crate) struct MemoryAccountSecrets {
    values: Mutex<BTreeMap<(String, String, String), SecretString>>,
}

impl MemoryAccountSecrets {
    fn check_reference(plugin: &str, account: &str, reference: &str) -> Result<(), StoreError> {
        OsKeychainBackend::check_reference(plugin, account, reference).map(|_| ())
    }
}

impl AccountSecretBackend for MemoryAccountSecrets {
    fn kind(&self) -> Result<AccountStorageKind, StoreError> {
        Ok(AccountStorageKind::OsKeychain)
    }

    fn reference(&self) -> Result<&'static str, StoreError> {
        Ok(KEYCHAIN_REFERENCE)
    }

    fn get(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        Self::check_reference(plugin, account, reference)?;
        self.values
            .lock()
            .map_err(|_| {
                backend_error(
                    AccountStorageKind::OsKeychain,
                    plugin,
                    account,
                    "memory credential lock is poisoned",
                )
            })?
            .get(&(kind.to_owned(), plugin.to_owned(), account.to_owned()))
            .cloned()
            .ok_or_else(|| {
                backend_error(
                    AccountStorageKind::OsKeychain,
                    plugin,
                    account,
                    "secret record not found",
                )
            })
    }

    fn set(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        self.values
            .lock()
            .map_err(|_| {
                backend_error(
                    AccountStorageKind::OsKeychain,
                    plugin,
                    account,
                    "memory credential lock is poisoned",
                )
            })?
            .insert(
                (kind.to_owned(), plugin.to_owned(), account.to_owned()),
                secret,
            );
        Ok(())
    }

    fn delete(
        &self,
        kind: &str,
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        self.values
            .lock()
            .map_err(|_| {
                backend_error(
                    AccountStorageKind::OsKeychain,
                    plugin,
                    account,
                    "memory credential lock is poisoned",
                )
            })?
            .remove(&(kind.to_owned(), plugin.to_owned(), account.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    pub(super) const MARKER: &str = "encrypted-secret-marker-61b42d";
    const PLUGIN: &str = "example-agent";
    const ACCOUNT: &str = "work";
    const KIND: &str = "subscription";

    pub(super) struct TestDir(pub(super) PathBuf);

    impl TestDir {
        pub(super) fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    pub(super) fn scratch() -> TestDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "hofund-encrypted-accounts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("scratch");
        TestDir(path)
    }

    pub(super) fn secret() -> SecretString {
        SecretString::from(MARKER)
    }

    #[test]
    fn encrypted_file_round_trip_is_private_and_uses_random_nonces() {
        let root = scratch();
        let path = root.join("accounts.json.secrets");
        let key_path = root.join("accounts.json.key");
        let backend = EncryptedFileBackend::open(path.clone(), key_path.clone()).expect("open");
        backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let loaded = backend
            .get(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect("get");
        assert_eq!(kernel::expose_secret(&loaded), MARKER);
        assert!(
            !fs::read(&path)
                .expect("ciphertext")
                .windows(MARKER.len())
                .any(|window| window == MARKER.as_bytes())
        );
        let first_nonce = read_records(&path).expect("records")[0].nonce;
        backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("replace");
        let second_nonce = read_records(&path).expect("records")[0].nonce;
        assert_ne!(first_nonce, second_nonce);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in [path, key_path] {
                let mode = fs::metadata(file).expect("metadata").permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
            }
        }
    }

    #[test]
    fn wrong_key_fails_closed_without_leaking_the_secret() {
        let dir = scratch();
        let path = dir.join("accounts.secrets");
        let writer = EncryptedFileBackend::from_key(path.clone(), [7_u8; KEY_LEN]);
        writer
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let reader = EncryptedFileBackend::from_key(path, [8_u8; KEY_LEN]);
        let error = reader
            .get(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect_err("wrong key");
        assert!(!format!("{error} {error:?}").contains(MARKER));
    }

    #[test]
    fn tampered_ciphertext_and_nonce_fail_closed() {
        for tamper_nonce in [false, true] {
            let dir = scratch();
            let path = dir.join("accounts.secrets");
            let backend = EncryptedFileBackend::from_key(path.clone(), [11_u8; KEY_LEN]);
            backend
                .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
                .expect("set");
            if tamper_nonce {
                let mut rows = read_records(&path).expect("records");
                rows[0].nonce[0] ^= 1;
                fs::write(&path, encode_records(&rows).expect("encode")).expect("tamper");
            } else {
                let mut bytes = fs::read(&path).expect("ciphertext");
                let last = bytes.last_mut().expect("nonempty");
                *last ^= 1;
                fs::write(&path, bytes).expect("tamper");
            }
            backend
                .get(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
                .expect_err("tampering");
        }
    }

    #[test]
    fn trailing_bytes_after_the_last_record_fail_closed() {
        let dir = scratch();
        let path = dir.join("accounts.secrets");
        let backend = EncryptedFileBackend::from_key(path.clone(), [13_u8; KEY_LEN]);
        backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let mut bytes = fs::read(&path).expect("ciphertext");
        bytes.extend_from_slice(b"junk");
        fs::write(&path, bytes).expect("append");
        backend
            .get(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect_err("trailing bytes");
    }

    #[test]
    fn moving_a_record_to_another_account_fails_associated_data_check() {
        let dir = scratch();
        let path = dir.join("accounts.secrets");
        let backend = EncryptedFileBackend::from_key(path.clone(), [19_u8; KEY_LEN]);
        backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let mut rows = read_records(&path).expect("records");
        rows[0].account = "other".to_owned();
        fs::write(&path, encode_records(&rows).expect("encode")).expect("move");
        let error = backend
            .get(KIND, PLUGIN, "other", FILE_REFERENCE)
            .expect_err("wrong associated data");
        assert!(!error.to_string().contains(MARKER));
    }

    #[test]
    fn kind_scoped_file_records_do_not_collide() {
        let dir = scratch();
        let path = dir.join("accounts.secrets");
        let backend = EncryptedFileBackend::from_key(path, [23_u8; KEY_LEN]);
        for (kind, value) in [("subscription", "one"), ("agent", "two")] {
            backend
                .set(
                    kind,
                    PLUGIN,
                    ACCOUNT,
                    FILE_REFERENCE,
                    SecretString::from(value),
                )
                .expect("set");
        }
        for (kind, expected) in [("subscription", "one"), ("agent", "two")] {
            let secret = backend
                .get(kind, PLUGIN, ACCOUNT, FILE_REFERENCE)
                .expect("get");
            assert_eq!(kernel::expose_secret(&secret), expected);
        }
        assert_ne!(
            native_entry_name("subscription", PLUGIN, ACCOUNT, 2),
            native_entry_name("agent", PLUGIN, ACCOUNT, 2)
        );
        assert_eq!(
            native_entry_name("subscription", PLUGIN, ACCOUNT, 1),
            format!("{PLUGIN}/{ACCOUNT}")
        );
    }

    #[test]
    fn native_backend_rejects_encrypted_file_operations() {
        let backend = MemoryAccountSecrets::default();
        backend
            .set(KIND, PLUGIN, ACCOUNT, KEYCHAIN_REFERENCE, secret())
            .expect("seed");
        let get_error = backend
            .get(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect_err("wrong backend reference");
        assert!(get_error.to_string().contains("different backend"));
        let set_error = backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect_err("wrong backend reference");
        assert!(set_error.to_string().contains("different backend"));
        let delete_error = backend
            .delete(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect_err("wrong backend reference");
        assert!(delete_error.to_string().contains("different backend"));
        backend
            .get(KIND, PLUGIN, ACCOUNT, KEYCHAIN_REFERENCE)
            .expect("original entry");
    }

    #[test]
    fn file_backend_rejects_native_operations() {
        let dir = scratch();
        let path = dir.join("accounts.secrets");
        let backend = EncryptedFileBackend::from_key(path, [29_u8; KEY_LEN]);
        backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("seed");
        let get_error = backend
            .get(KIND, PLUGIN, ACCOUNT, KEYCHAIN_REFERENCE)
            .expect_err("wrong backend reference");
        assert!(get_error.to_string().contains("different backend"));
        let set_error = backend
            .set(KIND, PLUGIN, ACCOUNT, KEYCHAIN_REFERENCE, secret())
            .expect_err("wrong backend reference");
        assert!(set_error.to_string().contains("different backend"));
        let delete_error = backend
            .delete(KIND, PLUGIN, ACCOUNT, KEYCHAIN_REFERENCE)
            .expect_err("wrong backend reference");
        assert!(delete_error.to_string().contains("different backend"));
        backend
            .get(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect("original entry");
    }

    #[test]
    fn changing_a_record_kind_fails_associated_data_authentication() {
        let dir = scratch();
        let path = dir.join("accounts.secrets");
        let backend = EncryptedFileBackend::from_key(path.clone(), [31_u8; KEY_LEN]);
        backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let mut rows = read_records(&path).expect("records");
        rows[0].kind = "agent".to_owned();
        fs::write(&path, encode_records(&rows).expect("encode")).expect("tamper");
        let error = backend
            .get("agent", PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect_err("wrong kind");
        assert!(error.to_string().contains("authentication failed"));
    }

    #[test]
    fn missing_key_and_backend_debug_do_not_expose_secret_material() {
        let root = scratch();
        let path = root.join("accounts.secrets");
        let key_path = root.join("accounts.key");
        let backend = EncryptedFileBackend::open(path.clone(), key_path.clone()).expect("open");
        backend
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        assert!(!format!("{backend:?}").contains(MARKER));
        fs::remove_file(&key_path).expect("remove key");
        let error = EncryptedFileBackend::open(path, key_path).expect_err("missing key");
        assert!(!format!("{error} {error:?}").contains(MARKER));
    }

    #[test]
    fn second_key_publish_does_not_replace_the_first_key() {
        let root = scratch();
        let secrets_path = root.join("accounts.secrets");
        let key_path = root.join("accounts.key");
        load_or_create_key(&secrets_path, &key_path).expect("first key");
        let first = fs::read(&key_path).expect("first bytes");
        load_or_create_key(&secrets_path, &key_path).expect("second key");
        let second = fs::read(&key_path).expect("second bytes");
        assert!(first == second, "second publish replaced the key");
        assert!(!root.0.read_dir().expect("directory").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[test]
    fn publish_key_keeps_an_existing_key_and_returns_its_bytes() {
        let root = scratch();
        let key_path = root.join("accounts.key");
        let winner = [5_u8; KEY_LEN];
        let loser = [9_u8; KEY_LEN];
        let published = publish_key(&key_path, &winner).expect("first publish");
        assert!(published.expose_secret() == &winner);
        let returned = publish_key(&key_path, &loser).expect("racing publish");
        assert!(
            returned.expose_secret() == &winner,
            "racing publisher did not adopt the existing key"
        );
        let on_disk = fs::read(&key_path).expect("key bytes");
        assert!(on_disk == winner, "racing publish replaced the key file");
        assert!(!root.0.read_dir().expect("directory").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
    }

    #[test]
    fn unprepared_backend_with_absent_key_fails_without_creating_the_key() {
        let root = scratch();
        let path = root.join("accounts.secrets");
        let key_path = root.join("accounts.key");

        let fresh = EncryptedFileBackend::new(path.clone(), key_path.clone());
        let error = fresh
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect_err("set without a key");
        assert!(error.to_string().contains("is missing"));
        assert!(!key_path.exists(), "set created the key file");
        assert!(!path.exists(), "set wrote records without a key");

        EncryptedFileBackend::from_key(path.clone(), [37_u8; KEY_LEN])
            .set(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("seed record");
        let fresh = EncryptedFileBackend::new(path, key_path.clone());
        let error = fresh
            .get(KIND, PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect_err("get without a key");
        assert!(!format!("{error} {error:?}").contains(MARKER));
        assert!(!key_path.exists(), "get created the key file");
    }

    #[test]
    fn key_file_must_be_a_regular_private_file() {
        let root = scratch();
        let path = root.join("accounts.secrets");
        let key_path = root.join("accounts.key");
        EncryptedFileBackend::open(path.clone(), key_path.clone()).expect("create");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&key_path).expect("metadata").permissions();
            permissions.set_mode(0o644);
            fs::set_permissions(&key_path, permissions).expect("permissions");
            EncryptedFileBackend::open(path.clone(), key_path.clone())
                .expect_err("insecure key mode");
            fs::remove_file(&key_path).expect("remove key");
            std::os::unix::fs::symlink(&path, &key_path).expect("symlink");
            EncryptedFileBackend::open(path, key_path).expect_err("symlink key");
        }
    }
}

#[cfg(test)]
mod store_tests {
    use hofund_plugin_api::kernel::AccountBackend;
    use hofund_plugin_api::{Account, AccountId, PluginId, PluginKey, PluginKind};

    use super::*;
    use crate::AccountStore;

    fn key() -> PluginKey {
        PluginKey::new(
            PluginKind::Subscription,
            PluginId::from_static("example-agent"),
        )
    }

    fn account(id: &'static str) -> Account {
        Account::from_stored_scoped(key(), AccountId::from_static(id), "Work").expect("account")
    }

    async fn read_secret(store: &AccountStore, id: &'static str) -> SecretString {
        store
            .secret(&key(), &AccountId::from_static(id))
            .await
            .expect("secret")
            .expect("present")
    }

    fn write_rows(path: &Path, rows: &[(&str, &str)]) {
        let rows: Vec<_> = rows
            .iter()
            .map(|(account, reference)| {
                format!(
                    "{{\"kind\":\"subscription\",\"plugin\":\"example-agent\",\"account\":\"{account}\",\"label\":\"Work\",\"secret_ref\":\"{reference}\"}}"
                )
            })
            .collect();
        fs::write(path, format!("[{}]", rows.join(","))).expect("metadata");
    }

    #[tokio::test]
    async fn account_store_migrates_plaintext_into_authenticated_ciphertext() {
        let root = tests::scratch();
        let path = root.join("accounts.json");
        fs::write(
            &path,
            format!(
                "[{{\"plugin\":\"example-agent\",\"account\":\"work\",\"label\":\"Work\",\"secret\":\"{}\"}}]",
                tests::MARKER
            ),
        )
        .expect("legacy");
        let backend = Arc::new(EncryptedFileBackend::new(
            sibling_path(&path, ".secrets"),
            sibling_path(&path, ".key"),
        ));
        let store = AccountStore::open_with_backend(&path, backend).expect("migrate");
        assert_eq!(
            store.storage_kind().expect("kind"),
            AccountStorageKind::EncryptedFile
        );
        assert_eq!(
            kernel::expose_secret(&read_secret(&store, "work").await),
            tests::MARKER
        );
        assert!(
            !fs::read_to_string(&path)
                .expect("metadata")
                .contains(tests::MARKER)
        );
        assert!(
            !fs::read(sibling_path(&path, ".secrets"))
                .expect("ciphertext")
                .windows(tests::MARKER.len())
                .any(|window| window == tests::MARKER.as_bytes())
        );
    }

    #[test]
    fn opening_an_empty_store_does_not_probe_the_native_backend() {
        let dir = tests::scratch();
        let path = dir.join("accounts.json");
        let backend = Arc::new(AutoBackend {
            selected: Mutex::new(None),
            native: Mutex::new(None),
            native_error: None,
            file: Arc::new(EncryptedFileBackend::new(
                sibling_path(&path, ".secrets"),
                sibling_path(&path, ".key"),
            )),
        });
        let _store = AccountStore::open_with_backend(&path, backend.clone()).expect("open");
        assert!(backend.selected.lock().expect("selection").is_none());
        assert!(backend.native.lock().expect("native").is_none());
        assert!(!sibling_path(&path, ".lock").exists());
    }

    #[tokio::test]
    async fn read_with_missing_key_does_not_create_key_file() {
        let root = tests::scratch();
        let path = root.join("accounts.json");
        write_rows(&path, &[("work", FILE_REFERENCE)]);
        let backend = Arc::new(EncryptedFileBackend::new(
            sibling_path(&path, ".secrets"),
            sibling_path(&path, ".key"),
        ));
        let store = AccountStore::open_with_backend(&path, backend).expect("open");
        store
            .secret(&key(), &AccountId::from_static("work"))
            .await
            .expect_err("missing key");
        assert!(!sibling_path(&path, ".key").exists());
    }

    #[tokio::test]
    async fn one_store_reads_native_and_file_rows_by_their_stored_references() {
        let root = tests::scratch();
        let path = root.join("accounts.json");
        let native = Arc::new(MemoryAccountSecrets::default());
        native
            .set(
                "subscription",
                "example-agent",
                "native",
                KEYCHAIN_REFERENCE,
                SecretString::from("native-secret"),
            )
            .expect("native");
        let file = Arc::new(EncryptedFileBackend::new(
            sibling_path(&path, ".secrets"),
            sibling_path(&path, ".key"),
        ));
        file.prepare().expect("prepare");
        file.set(
            "subscription",
            "example-agent",
            "file",
            FILE_REFERENCE,
            SecretString::from("file-secret"),
        )
        .expect("file");
        write_rows(
            &path,
            &[("native", KEYCHAIN_REFERENCE), ("file", FILE_REFERENCE)],
        );
        let backend = Arc::new(AutoBackend::with_backends(native.clone(), Ok(native), file));
        let store = AccountStore::open_with_backend(&path, backend).expect("open");
        assert_eq!(
            kernel::expose_secret(&read_secret(&store, "native").await),
            "native-secret"
        );
        assert_eq!(
            kernel::expose_secret(&read_secret(&store, "file").await),
            "file-secret"
        );
    }

    #[tokio::test]
    async fn unavailable_native_backend_fails_only_native_rows() {
        let root = tests::scratch();
        let path = root.join("accounts.json");
        let selected = Arc::new(MemoryAccountSecrets::default());
        let file = Arc::new(EncryptedFileBackend::new(
            sibling_path(&path, ".secrets"),
            sibling_path(&path, ".key"),
        ));
        file.prepare().expect("prepare");
        file.set(
            "subscription",
            "example-agent",
            "file",
            FILE_REFERENCE,
            SecretString::from("file-secret"),
        )
        .expect("file");
        write_rows(
            &path,
            &[("native", KEYCHAIN_REFERENCE), ("file", FILE_REFERENCE)],
        );
        let backend = Arc::new(AutoBackend::with_backends(
            selected,
            Err("native unavailable".to_owned()),
            file,
        ));
        let store = AccountStore::open_with_backend(&path, backend).expect("open");
        assert_eq!(store.list(&key()).await.expect("list").len(), 2);
        assert_eq!(
            kernel::expose_secret(&read_secret(&store, "file").await),
            "file-secret"
        );
        store
            .secret(&key(), &AccountId::from_static("native"))
            .await
            .expect_err("native failure");
    }

    #[tokio::test]
    async fn corrupt_secret_does_not_block_list_or_other_writes() {
        let root = tests::scratch();
        let path = root.join("accounts.json");
        let native = Arc::new(MemoryAccountSecrets::default());
        let file = Arc::new(EncryptedFileBackend::new(
            sibling_path(&path, ".secrets"),
            sibling_path(&path, ".key"),
        ));
        file.prepare().expect("prepare");
        for id in ["good", "bad"] {
            file.set(
                "subscription",
                "example-agent",
                id,
                FILE_REFERENCE,
                SecretString::from("file-secret"),
            )
            .expect("set");
        }
        write_rows(&path, &[("good", FILE_REFERENCE), ("bad", FILE_REFERENCE)]);
        let mut rows = read_records(&sibling_path(&path, ".secrets")).expect("records");
        let bad = rows
            .iter_mut()
            .find(|row| row.account == "bad")
            .expect("bad row");
        bad.ciphertext[0] ^= 1;
        fs::write(
            sibling_path(&path, ".secrets"),
            encode_records(&rows).expect("encode"),
        )
        .expect("corrupt");
        let backend = Arc::new(AutoBackend::with_backends(native.clone(), Ok(native), file));
        let store = AccountStore::open_with_backend(&path, backend).expect("open");
        assert_eq!(store.list(&key()).await.expect("list").len(), 2);
        store
            .upsert(account("third"), SecretString::from("third-secret"))
            .await
            .expect("upsert another");
        assert_eq!(
            kernel::expose_secret(&read_secret(&store, "good").await),
            "file-secret"
        );
        assert_eq!(
            kernel::expose_secret(&read_secret(&store, "third").await),
            "third-secret"
        );
        store
            .secret(&key(), &AccountId::from_static("bad"))
            .await
            .expect_err("corrupt secret");
    }

    #[tokio::test]
    async fn remove_deletes_the_encrypted_file_record() {
        let root = tests::scratch();
        let path = root.join("accounts.json");
        let backend = Arc::new(EncryptedFileBackend::new(
            sibling_path(&path, ".secrets"),
            sibling_path(&path, ".key"),
        ));
        let store = AccountStore::open_with_backend(&path, backend.clone()).expect("open");
        store
            .upsert(account("work"), tests::secret())
            .await
            .expect("upsert");
        store
            .remove(&key(), &AccountId::from_static("work"))
            .await
            .expect("remove");
        backend
            .get("subscription", "example-agent", "work", FILE_REFERENCE)
            .expect_err("deleted record");
        assert!(
            read_records(&sibling_path(&path, ".secrets"))
                .expect("records")
                .is_empty()
        );
    }
}
