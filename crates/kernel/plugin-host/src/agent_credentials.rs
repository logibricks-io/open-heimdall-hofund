//! Agent keys in the OS credential store.
//!
//! Each OS keeps the item where the agent's own client looks for it:
//!
//! - macOS: an internet password in the default keychain. The server is the
//!   item URL and the account is the item account.
//! - Linux: a Secret Service item in the default collection with the
//!   attributes `url` and `username`, and the item label.
//! - Windows: a generic credential named by the item's Windows target, with
//!   local persistence and a UTF-8 blob.
//!
//! No call starts a child process. Every call runs on a worker thread and
//! gives up after [`DEADLINE`]. A Secret Service prompt is dismissed after
//! [`PROMPT_SECONDS`]. Errors name the store and never hold the secret.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use hofund_plugin_api::{Credential, CredentialError, CredentialStore, SecretString};
use zeroize::Zeroizing;

/// How long one store call may take, prompts included.
const DEADLINE: Duration = Duration::from_secs(90);

/// How long a Secret Service unlock prompt may stay open.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PROMPT_SECONDS: u64 = 60;

/// Content type of a Secret Service item.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CONTENT_TYPE: &str = "text/plain; charset=utf8";

/// The OS credential store for agent keys.
///
/// Building it does not touch the store. Each call connects, works and lets
/// go. Tests must use a fake store instead.
#[must_use]
pub fn native_credential_store() -> Arc<dyn CredentialStore> {
    Arc::new(NativeCredentialStore { deadline: DEADLINE })
}

struct NativeCredentialStore {
    deadline: Duration,
}

impl CredentialStore for NativeCredentialStore {
    fn read(&self, item: &Credential) -> Result<Option<SecretString>, CredentialError> {
        let item = item.clone();
        within(self.deadline, "read", move || native::read(&item))?
            .map(decode)
            .transpose()
    }

    fn write(&self, item: &Credential, secret: &str) -> Result<(), CredentialError> {
        let item = item.clone();
        let secret = Zeroizing::new(secret.to_owned());
        within(self.deadline, "write", move || {
            native::write(&item, secret.as_bytes())
        })
    }

    fn delete(&self, item: &Credential) -> Result<(), CredentialError> {
        let item = item.clone();
        within(self.deadline, "delete", move || native::delete(&item))
    }
}

/// Runs `work` on a worker thread and waits at most `deadline` for it.
///
/// A call that misses the deadline keeps running on its thread until the OS
/// answers; its result is dropped.
fn within<T, F>(deadline: Duration, action: &str, work: F) -> Result<T, CredentialError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, CredentialError> + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("hofund-credential-store".to_owned())
        .spawn(move || {
            let _ = sender.send(work());
        })
        .map_err(|error| CredentialError::new(format!("credential store {action}: {error}")))?;
    match receiver.recv_timeout(deadline) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(CredentialError::new(format!(
            "credential store {action}: no answer within {} s",
            deadline.as_secs()
        ))),
        Err(RecvTimeoutError::Disconnected) => Err(CredentialError::new(format!(
            "credential store {action}: the store call stopped"
        ))),
    }
}

/// The secret held in `bytes`, which must be UTF-8 text.
fn decode(bytes: Zeroizing<Vec<u8>>) -> Result<SecretString, CredentialError> {
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| CredentialError::new("credential store: the item does not hold UTF-8 text"))?;
    Ok(SecretString::from(text.to_owned()))
}

/// An error from `store`. Store errors carry no secret.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
    allow(dead_code)
)]
fn failure(store: &str, error: &dyn std::fmt::Display) -> CredentialError {
    CredentialError::new(format!("{store}: {error}"))
}

/// Secret Service attributes the agent looks the item up by.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn secret_service_attributes(item: &Credential) -> HashMap<&str, &str> {
    HashMap::from([("url", item.url()), ("username", item.account())])
}

/// Windows entry modifiers: the agent's target name and local persistence.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn windows_modifiers(item: &Credential) -> HashMap<&str, &str> {
    HashMap::from([("target", item.windows_target()), ("persistence", "Local")])
}

#[cfg(target_os = "macos")]
mod native {
    use security_framework::os::macos::keychain::SecKeychain;
    use security_framework::os::macos::keychain_item::SecKeychainItem;
    use security_framework::os::macos::passwords::{
        SecAuthenticationType, SecKeychainItemPassword, SecProtocolType, find_internet_password,
    };
    use zeroize::Zeroizing;

    use super::{Credential, CredentialError, failure};

    const STORE: &str = "keychain";
    /// `errSecItemNotFound`.
    const ITEM_NOT_FOUND: i32 = -25_300;

    /// The internet password for the item URL and account. An empty path, no
    /// port and the `Any` protocol and authentication match every value, so an
    /// item the agent wrote itself is found too.
    fn find(
        item: &Credential,
    ) -> Result<Option<(SecKeychainItemPassword, SecKeychainItem)>, CredentialError> {
        match find_internet_password(
            None,
            item.url(),
            None,
            item.account(),
            "",
            None,
            SecProtocolType::Any,
            SecAuthenticationType::Any,
        ) {
            Ok(found) => Ok(Some(found)),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(None),
            Err(error) => Err(failure(STORE, &error)),
        }
    }

    pub(super) fn read(item: &Credential) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialError> {
        Ok(find(item)?.map(|(password, _)| Zeroizing::new(password.to_vec())))
    }

    pub(super) fn write(item: &Credential, secret: &[u8]) -> Result<(), CredentialError> {
        match find(item)? {
            Some((_, mut existing)) => existing
                .set_password(secret)
                .map_err(|error| failure(STORE, &error)),
            None => SecKeychain::default()
                .and_then(|keychain| {
                    keychain.add_internet_password(
                        item.url(),
                        None,
                        item.account(),
                        "",
                        None,
                        SecProtocolType::Any,
                        SecAuthenticationType::Any,
                        secret,
                    )
                })
                .map_err(|error| failure(STORE, &error)),
        }
    }

    pub(super) fn delete(item: &Credential) -> Result<(), CredentialError> {
        if let Some((_, existing)) = find(item)? {
            existing.delete();
            if find(item)?.is_some() {
                return Err(CredentialError::new("keychain: the item was not removed"));
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod native {
    use dbus_secret_service::{Collection, EncryptionType, Error, SecretService};
    use zeroize::Zeroizing;

    use super::{
        CONTENT_TYPE, Credential, CredentialError, PROMPT_SECONDS, failure,
        secret_service_attributes,
    };

    const STORE: &str = "Secret Service";

    fn connect() -> Result<SecretService, CredentialError> {
        SecretService::connect_with_max_prompt_timeout(EncryptionType::Plain, PROMPT_SECONDS)
            .map_err(|error| failure(STORE, &error))
    }

    /// The default collection, unlocked, or `None` when there is none.
    fn default_collection(
        service: &SecretService,
    ) -> Result<Option<Collection<'_>>, CredentialError> {
        let collection = match service.get_default_collection() {
            Ok(collection) => collection,
            Err(Error::NoResult) => return Ok(None),
            Err(error) => return Err(failure(STORE, &error)),
        };
        collection
            .ensure_unlocked()
            .map_err(|error| failure(STORE, &error))?;
        Ok(Some(collection))
    }

    pub(super) fn read(item: &Credential) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialError> {
        let service = connect()?;
        let Some(collection) = default_collection(&service)? else {
            return Ok(None);
        };
        let found = collection
            .search_items(secret_service_attributes(item))
            .map_err(|error| failure(STORE, &error))?;
        let Some(first) = found.first() else {
            return Ok(None);
        };
        first
            .get_secret()
            .map(|secret| Some(Zeroizing::new(secret)))
            .map_err(|error| failure(STORE, &error))
    }

    pub(super) fn write(item: &Credential, secret: &[u8]) -> Result<(), CredentialError> {
        let service = connect()?;
        let collection = match default_collection(&service)? {
            Some(collection) => collection,
            None => {
                let created = service
                    .create_collection("Default", "default")
                    .map_err(|error| failure(STORE, &error))?;
                created
                    .ensure_unlocked()
                    .map_err(|error| failure(STORE, &error))?;
                created
            }
        };
        collection
            .create_item(
                item.label(),
                secret_service_attributes(item),
                secret,
                true,
                CONTENT_TYPE,
            )
            .map(drop)
            .map_err(|error| failure(STORE, &error))
    }

    pub(super) fn delete(item: &Credential) -> Result<(), CredentialError> {
        let service = connect()?;
        let Some(collection) = default_collection(&service)? else {
            return Ok(());
        };
        let found = collection
            .search_items(secret_service_attributes(item))
            .map_err(|error| failure(STORE, &error))?;
        for each in &found {
            each.delete().map_err(|error| failure(STORE, &error))?;
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod native {
    use keyring_core::api::CredentialStoreApi;
    use keyring_core::{Entry, Error};
    use zeroize::Zeroizing;

    use super::{Credential, CredentialError, failure, windows_modifiers};

    const STORE: &str = "Windows Credential Manager";

    /// The generic credential named by the item's Windows target. The target
    /// overrides the service, so the label only fills a required field.
    fn entry(item: &Credential) -> Result<Entry, CredentialError> {
        let store =
            windows_native_keyring_store::Store::new().map_err(|error| failure(STORE, &error))?;
        store
            .build(item.label(), item.account(), Some(&windows_modifiers(item)))
            .map_err(|error| failure(STORE, &error))
    }

    pub(super) fn read(item: &Credential) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialError> {
        match entry(item)?.get_secret() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(failure(STORE, &error)),
        }
    }

    pub(super) fn write(item: &Credential, secret: &[u8]) -> Result<(), CredentialError> {
        entry(item)?
            .set_secret(secret)
            .map_err(|error| failure(STORE, &error))
    }

    pub(super) fn delete(item: &Credential) -> Result<(), CredentialError> {
        match entry(item)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(error) => Err(failure(STORE, &error)),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod native {
    use zeroize::Zeroizing;

    use super::{Credential, CredentialError};

    fn unsupported() -> CredentialError {
        CredentialError::new("no OS credential store on this system")
    }

    pub(super) fn read(_: &Credential) -> Result<Option<Zeroizing<Vec<u8>>>, CredentialError> {
        Err(unsupported())
    }

    pub(super) fn write(_: &Credential, _: &[u8]) -> Result<(), CredentialError> {
        Err(unsupported())
    }

    pub(super) fn delete(_: &Credential) -> Result<(), CredentialError> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use secrecy::ExposeSecret;

    use super::*;

    fn item() -> Credential {
        Credential::new(
            "http://127.0.0.1:9/v1",
            "Bearer",
            "zed-github-account",
            "zed:url=http://127.0.0.1:9/v1",
        )
    }

    #[test]
    fn a_call_inside_the_deadline_returns_its_result() {
        let value = within(Duration::from_secs(5), "read", || Ok(7)).expect("result");
        assert_eq!(value, 7);
        let error = within::<(), _>(Duration::from_secs(5), "write", || {
            Err(CredentialError::new("denied"))
        })
        .expect_err("error");
        assert_eq!(error.to_string(), "denied");
    }

    #[test]
    fn a_call_past_the_deadline_gives_up() {
        let started = Instant::now();
        let error = within(Duration::from_millis(50), "delete", || {
            std::thread::sleep(Duration::from_secs(2));
            Ok(())
        })
        .expect_err("timeout");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            error.to_string(),
            "credential store delete: no answer within 0 s"
        );
    }

    #[test]
    fn a_call_that_panics_reports_a_stopped_call() {
        let error = within::<(), _>(Duration::from_secs(5), "read", || panic!("store crashed"))
            .expect_err("stopped");
        assert_eq!(
            error.to_string(),
            "credential store read: the store call stopped"
        );
    }

    #[test]
    fn a_utf8_item_decodes_and_other_bytes_do_not() {
        let secret = decode(Zeroizing::new(b"hk-test".to_vec())).expect("text");
        assert_eq!(secret.expose_secret(), "hk-test");
        let error = decode(Zeroizing::new(vec![0xff, 0xfe])).expect_err("bytes");
        assert_eq!(
            error.to_string(),
            "credential store: the item does not hold UTF-8 text"
        );
    }

    #[test]
    fn a_store_error_names_the_store() {
        assert_eq!(
            failure("keychain", &"locked").to_string(),
            "keychain: locked"
        );
    }

    #[test]
    fn secret_service_items_are_found_by_url_and_user_name() {
        let item = item();
        let attributes = secret_service_attributes(&item);
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes["url"], "http://127.0.0.1:9/v1");
        assert_eq!(attributes["username"], "Bearer");
    }

    #[test]
    fn windows_items_use_the_agent_target_with_local_persistence() {
        let item = item();
        let modifiers = windows_modifiers(&item);
        assert_eq!(modifiers.len(), 2);
        assert_eq!(modifiers["target"], "zed:url=http://127.0.0.1:9/v1");
        assert_eq!(modifiers["persistence"], "Local");
    }

    #[test]
    fn building_the_native_store_does_not_touch_the_os() {
        let store = native_credential_store();
        assert_eq!(Arc::strong_count(&store), 1);
    }
}
