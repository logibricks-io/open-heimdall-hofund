use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use hofund_plugin_api::SecretString;
use hofund_plugin_api::kernel;
#[cfg(target_os = "windows")]
use keyring_core::CredentialStoreApi;
use secrecy::{ExposeSecret, SecretBox};
use zeroize::Zeroize;

use crate::accounts::{
    AccountStorageKind, StoreError, backend_error, set_mode, sibling_path, write_atomic,
};

const KEYCHAIN_REFERENCE: &str = "os-keychain:v1";
const FILE_REFERENCE: &str = "encrypted-file:v1";
const KEYCHAIN_SERVICE: &str = "io.logibricks.hofund.accounts";
const MAGIC: &[u8; 8] = b"HOFACCT\0";
const FORMAT_VERSION: u8 = 1;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

pub(crate) trait AccountSecretBackend: Send + Sync {
    fn prepare(&self) -> Result<(), StoreError> {
        Ok(())
    }
    fn kind(&self) -> Result<AccountStorageKind, StoreError>;
    fn reference(&self) -> Result<&'static str, StoreError>;
    fn get(&self, plugin: &str, account: &str, reference: &str)
    -> Result<SecretString, StoreError>;
    fn set(
        &self,
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError>;
    fn delete(&self, plugin: &str, account: &str, reference: &str) -> Result<(), StoreError>;
}

pub(crate) fn default_backend(metadata_path: &Path) -> Arc<dyn AccountSecretBackend> {
    Arc::new(AutoBackend {
        metadata_path: metadata_path.to_path_buf(),
        selected: Mutex::new(None),
    })
}

struct AutoBackend {
    metadata_path: PathBuf,
    selected: Mutex<Option<Arc<dyn AccountSecretBackend>>>,
}

impl AutoBackend {
    fn select(&self) -> Result<Arc<dyn AccountSecretBackend>, StoreError> {
        {
            let guard = self.selected.lock().map_err(|_| {
                StoreError::Io("account backend selection lock is poisoned".to_owned())
            })?;
            if let Some(backend) = guard.as_ref() {
                return Ok(Arc::clone(backend));
            }
        }
        let candidate: Arc<dyn AccountSecretBackend> = match OsKeychainBackend::probe() {
            Ok(backend) => {
                tracing::debug!(backend = "os-keychain", "selected account secret backend");
                Arc::new(backend)
            }
            Err(reason) => {
                tracing::warn!(
                    backend = "encrypted-file",
                    reason = %reason,
                    "OS credential store unavailable; using encrypted account file"
                );
                Arc::new(EncryptedFileBackend::new(
                    sibling_path(&self.metadata_path, ".secrets"),
                    sibling_path(&self.metadata_path, ".key"),
                ))
            }
        };
        let mut guard = self
            .selected
            .lock()
            .map_err(|_| StoreError::Io("account backend selection lock is poisoned".to_owned()))?;
        Ok(Arc::clone(guard.get_or_insert(candidate)))
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
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        let backend = self.select()?;
        backend.prepare()?;
        backend.get(plugin, account, reference)
    }

    fn set(
        &self,
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        let backend = self.select()?;
        backend.prepare()?;
        backend.set(plugin, account, reference, secret)
    }

    fn delete(&self, plugin: &str, account: &str, reference: &str) -> Result<(), StoreError> {
        let backend = self.select()?;
        backend.prepare()?;
        backend.delete(plugin, account, reference)
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
        let entry = backend.entry("backend", "probe")?;
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
    fn entry(&self, plugin: &str, account: &str) -> Result<NativeEntry, String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &format!("{plugin}/{account}"))
            .map_err(|error| error.to_string())
    }

    #[cfg(target_os = "windows")]
    fn entry(&self, plugin: &str, account: &str) -> Result<NativeEntry, String> {
        self.store
            .build(KEYCHAIN_SERVICE, &format!("{plugin}/{account}"), None)
            .map_err(|error| error.to_string())
    }

    fn check_reference(plugin: &str, account: &str, reference: &str) -> Result<(), StoreError> {
        if reference == KEYCHAIN_REFERENCE {
            Ok(())
        } else {
            Err(backend_error(
                AccountStorageKind::OsKeychain,
                plugin,
                account,
                "secret reference belongs to a different backend or format",
            ))
        }
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
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let entry = self.entry(plugin, account).map_err(|error| {
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
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let entry = self.entry(plugin, account).map_err(|error| {
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
    fn delete(&self, plugin: &str, account: &str, reference: &str) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let entry = self.entry(plugin, account).map_err(|error| {
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
    fn delete(&self, plugin: &str, account: &str, _reference: &str) -> Result<(), StoreError> {
        Err(backend_error(
            AccountStorageKind::OsKeychain,
            plugin,
            account,
            "native credential backend is unavailable",
        ))
    }
}

struct CipherRow {
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
        let key = self.key.get().ok_or_else(|| {
            backend_error(
                AccountStorageKind::EncryptedFile,
                plugin,
                account,
                "encrypted backend was not prepared under the account lock",
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

    fn check_reference(plugin: &str, account: &str, reference: &str) -> Result<(), StoreError> {
        if reference == FILE_REFERENCE {
            Ok(())
        } else {
            Err(backend_error(
                AccountStorageKind::EncryptedFile,
                plugin,
                account,
                "secret reference belongs to a different backend or format",
            ))
        }
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
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let rows = self.read(plugin, account)?;
        let row = rows
            .iter()
            .find(|row| row.plugin == plugin && row.account == account)
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
                    aad: associated_data(plugin, account).as_bytes(),
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
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let mut rows = self.read(plugin, account)?;
        rows.retain(|row| !(row.plugin == plugin && row.account == account));
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
                    aad: associated_data(plugin, account).as_bytes(),
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
            plugin: plugin.to_owned(),
            account: account.to_owned(),
            nonce,
            ciphertext,
        });
        self.write(plugin, account, &rows)
    }

    fn delete(&self, plugin: &str, account: &str, reference: &str) -> Result<(), StoreError> {
        Self::check_reference(plugin, account, reference)?;
        let mut rows = self.read(plugin, account)?;
        rows.retain(|row| !(row.plugin == plugin && row.account == account));
        self.write(plugin, account, &rows)
    }
}

fn associated_data(plugin: &str, account: &str) -> String {
    format!("hofund-account-secret\0format-v1\0{plugin}\0{account}")
}

fn load_or_create_key(
    secrets_path: &Path,
    key_path: &Path,
) -> Result<SecretBox<[u8; KEY_LEN]>, StoreError> {
    match fs::read(key_path) {
        Ok(mut bytes) => {
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
            let secret = SecretBox::new(Box::new(array));
            array.zeroize();
            Ok(secret)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if fs::metadata(secrets_path)
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false)
            {
                return Err(StoreError::Format(format!(
                    "account encryption key `{}` is missing while encrypted records exist",
                    key_path.display()
                )));
            }
            let mut key = [0_u8; KEY_LEN];
            getrandom::getrandom(&mut key).map_err(|error| {
                key.zeroize();
                StoreError::Io(format!("cannot generate account key: {error}"))
            })?;
            write_atomic(key_path, &key).inspect_err(|_| {
                key.zeroize();
            })?;
            set_mode(key_path).inspect_err(|_| {
                key.zeroize();
            })?;
            let secret = SecretBox::new(Box::new(key));
            key.zeroize();
            Ok(secret)
        }
        Err(error) => Err(StoreError::Io(error.to_string())),
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
    if take(bytes, &mut offset, 1)? != [FORMAT_VERSION] {
        return Err("encrypted account file has an unsupported version".to_owned());
    }
    let count = read_u32(bytes, &mut offset)?;
    let mut rows = Vec::new();
    for _ in 0..count {
        let plugin_len = usize::from(read_u16(bytes, &mut offset)?);
        let account_len = usize::from(read_u16(bytes, &mut offset)?);
        let nonce_slice = take(bytes, &mut offset, NONCE_LEN)?;
        let nonce: [u8; NONCE_LEN] = nonce_slice
            .try_into()
            .map_err(|_| "encrypted account nonce has an invalid length".to_owned())?;
        let ciphertext_len = usize::try_from(read_u32(bytes, &mut offset)?)
            .map_err(|_| "encrypted account ciphertext is too large".to_owned())?;
        let plugin = std::str::from_utf8(take(bytes, &mut offset, plugin_len)?)
            .map_err(|_| "encrypted account plugin id is not UTF-8".to_owned())?
            .to_owned();
        let account = std::str::from_utf8(take(bytes, &mut offset, account_len)?)
            .map_err(|_| "encrypted account id is not UTF-8".to_owned())?
            .to_owned();
        let ciphertext = take(bytes, &mut offset, ciphertext_len)?.to_vec();
        rows.push(CipherRow {
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
        let plugin_len = u16::try_from(row.plugin.len())
            .map_err(|_| "account plugin id is too long".to_owned())?;
        let account_len =
            u16::try_from(row.account.len()).map_err(|_| "account id is too long".to_owned())?;
        let ciphertext_len = u32::try_from(row.ciphertext.len())
            .map_err(|_| "account ciphertext is too large".to_owned())?;
        bytes.extend_from_slice(&plugin_len.to_le_bytes());
        bytes.extend_from_slice(&account_len.to_le_bytes());
        bytes.extend_from_slice(&row.nonce);
        bytes.extend_from_slice(&ciphertext_len.to_le_bytes());
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
    values: Mutex<BTreeMap<(String, String), SecretString>>,
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
        plugin: &str,
        account: &str,
        reference: &str,
    ) -> Result<SecretString, StoreError> {
        OsKeychainBackend::check_reference(plugin, account, reference)?;
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
            .get(&(plugin.to_owned(), account.to_owned()))
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
        plugin: &str,
        account: &str,
        reference: &str,
        secret: SecretString,
    ) -> Result<(), StoreError> {
        OsKeychainBackend::check_reference(plugin, account, reference)?;
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
            .insert((plugin.to_owned(), account.to_owned()), secret);
        Ok(())
    }

    fn delete(&self, plugin: &str, account: &str, reference: &str) -> Result<(), StoreError> {
        OsKeychainBackend::check_reference(plugin, account, reference)?;
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
            .remove(&(plugin.to_owned(), account.to_owned()));
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

    pub(super) fn scratch() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "hofund-encrypted-accounts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("scratch");
        path
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
            .set(PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let loaded = backend.get(PLUGIN, ACCOUNT, FILE_REFERENCE).expect("get");
        assert_eq!(kernel::expose_secret(&loaded), MARKER);
        assert!(
            !fs::read(&path)
                .expect("ciphertext")
                .windows(MARKER.len())
                .any(|window| window == MARKER.as_bytes())
        );
        let first_nonce = read_records(&path).expect("records")[0].nonce;
        backend
            .set(PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
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
        let path = scratch().join("accounts.secrets");
        let writer = EncryptedFileBackend::from_key(path.clone(), [7_u8; KEY_LEN]);
        writer
            .set(PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let reader = EncryptedFileBackend::from_key(path, [8_u8; KEY_LEN]);
        let error = reader
            .get(PLUGIN, ACCOUNT, FILE_REFERENCE)
            .expect_err("wrong key");
        assert!(!format!("{error} {error:?}").contains(MARKER));
    }

    #[test]
    fn tampered_ciphertext_and_nonce_fail_closed() {
        for tamper_nonce in [false, true] {
            let path = scratch().join("accounts.secrets");
            let backend = EncryptedFileBackend::from_key(path.clone(), [11_u8; KEY_LEN]);
            backend
                .set(PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
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
                .get(PLUGIN, ACCOUNT, FILE_REFERENCE)
                .expect_err("tampering");
        }
    }

    #[test]
    fn moving_a_record_to_another_account_fails_associated_data_check() {
        let path = scratch().join("accounts.secrets");
        let backend = EncryptedFileBackend::from_key(path.clone(), [19_u8; KEY_LEN]);
        backend
            .set(PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        let mut rows = read_records(&path).expect("records");
        rows[0].account = "other".to_owned();
        fs::write(&path, encode_records(&rows).expect("encode")).expect("move");
        let error = backend
            .get(PLUGIN, "other", FILE_REFERENCE)
            .expect_err("wrong associated data");
        assert!(!error.to_string().contains(MARKER));
    }

    #[test]
    fn missing_key_and_backend_debug_do_not_expose_secret_material() {
        let root = scratch();
        let path = root.join("accounts.secrets");
        let key_path = root.join("accounts.key");
        let backend = EncryptedFileBackend::open(path.clone(), key_path.clone()).expect("open");
        backend
            .set(PLUGIN, ACCOUNT, FILE_REFERENCE, secret())
            .expect("set");
        assert!(!format!("{backend:?}").contains(MARKER));
        fs::remove_file(&key_path).expect("remove key");
        let error = EncryptedFileBackend::open(path, key_path).expect_err("missing key");
        assert!(!format!("{error} {error:?}").contains(MARKER));
    }
}

#[cfg(test)]
mod store_tests {
    use hofund_plugin_api::kernel::AccountBackend;
    use hofund_plugin_api::{Account, AccountId, PluginId};

    use super::*;
    use crate::AccountStore;

    fn account() -> Account {
        Account::from_stored(
            PluginId::from_static("example-agent"),
            AccountId::from_static("work"),
            "Work",
        )
        .expect("account")
    }

    async fn read_secret(store: &AccountStore) -> SecretString {
        store
            .secret(
                &PluginId::from_static("example-agent"),
                &AccountId::from_static("work"),
            )
            .await
            .expect("secret")
            .expect("present")
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
            kernel::expose_secret(&read_secret(&store).await),
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
        let path = tests::scratch().join("accounts.json");
        let backend = Arc::new(AutoBackend {
            metadata_path: path.clone(),
            selected: Mutex::new(None),
        });
        let _store = AccountStore::open_with_backend(&path, backend.clone()).expect("open");
        assert!(backend.selected.lock().expect("selection").is_none());
        assert!(!sibling_path(&path, ".lock").exists());
    }

    #[tokio::test]
    async fn existing_keychain_references_do_not_silently_use_the_file_backend() {
        let root = tests::scratch();
        let path = root.join("accounts.json");
        let memory = Arc::new(MemoryAccountSecrets::default());
        let store = AccountStore::open_with_backend(&path, memory).expect("open");
        store
            .upsert(account(), tests::secret())
            .await
            .expect("upsert");
        let fallback = Arc::new(EncryptedFileBackend::new(
            sibling_path(&path, ".secrets"),
            sibling_path(&path, ".key"),
        ));
        let reopened = AccountStore::open_with_backend(&path, fallback).expect("open fallback");
        let error = reopened
            .secret(
                &PluginId::from_static("example-agent"),
                &AccountId::from_static("work"),
            )
            .await
            .expect_err("backend mismatch");
        assert!(error.to_string().contains("different backend"));
        assert!(!error.to_string().contains(tests::MARKER));
    }
}
