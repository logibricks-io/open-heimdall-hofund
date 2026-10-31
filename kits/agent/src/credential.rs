//! An in-memory OS credential store for tests.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use hofund_plugin_api::{Credential, CredentialError, CredentialStore, SecretString};

/// A credential store held in memory. Tests never touch the OS store.
///
/// Items are keyed by URL and account, the pair agents look an item up by;
/// the label and Windows target of the last write are kept with the secret.
#[derive(Debug, Default)]
pub struct MemoryCredentialStore {
    items: Mutex<BTreeMap<(String, String), (Credential, String)>>,
}

impl MemoryCredentialStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A separate store holding the same items.
    #[must_use]
    pub fn fork(&self) -> Self {
        Self {
            items: Mutex::new(self.lock().clone()),
        }
    }

    /// The secret of the item at `url` and `account`.
    #[must_use]
    pub fn secret(&self, url: &str, account: &str) -> Option<String> {
        self.lock()
            .get(&(url.to_owned(), account.to_owned()))
            .map(|(_, secret)| secret.clone())
    }

    /// Every item, in URL order.
    #[must_use]
    pub fn items(&self) -> Vec<Credential> {
        self.lock().values().map(|(item, _)| item.clone()).collect()
    }

    /// Replaces the secret of `item`, as a user editing the store would.
    pub fn set(&self, item: &Credential, secret: &str) {
        self.lock()
            .insert(key(item), (item.clone(), secret.to_owned()));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), (Credential, String)>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn read(&self, item: &Credential) -> Result<Option<SecretString>, CredentialError> {
        Ok(self
            .lock()
            .get(&key(item))
            .map(|(_, secret)| SecretString::from(secret.clone())))
    }

    fn write(&self, item: &Credential, secret: &str) -> Result<(), CredentialError> {
        self.set(item, secret);
        Ok(())
    }

    fn delete(&self, item: &Credential) -> Result<(), CredentialError> {
        self.lock().remove(&key(item));
        Ok(())
    }
}

fn key(item: &Credential) -> (String, String) {
    (item.url().to_owned(), item.account().to_owned())
}

#[cfg(test)]
mod tests {
    use hofund_plugin_api::{Credential, CredentialStore};

    use super::MemoryCredentialStore;

    #[test]
    fn items_are_keyed_by_url_and_account() {
        let store = MemoryCredentialStore::new();
        let item = Credential::new("http://h/v1", "Bearer", "label", "t");
        assert!(store.read(&item).expect("read").is_none());
        store.write(&item, "one").expect("write");
        let other = Credential::new("http://h/v1", "Other", "label", "t");
        assert!(store.read(&other).expect("read").is_none());
        let relabeled = Credential::new("http://h/v1", "Bearer", "new", "t2");
        store.write(&relabeled, "two").expect("rewrite");
        assert_eq!(store.items(), vec![relabeled.clone()]);
        assert!(store.read(&item).expect("read").is_some());
        assert_eq!(
            store.secret("http://h/v1", "Bearer").as_deref(),
            Some("two")
        );
        let copy = store.fork();
        store.delete(&item).expect("delete");
        store.delete(&item).expect("delete missing");
        assert_eq!(store.secret("http://h/v1", "Bearer"), None);
        assert_eq!(copy.secret("http://h/v1", "Bearer").as_deref(), Some("two"));
        copy.set(&item, "user");
        assert_eq!(
            copy.secret("http://h/v1", "Bearer").as_deref(),
            Some("user")
        );
    }
}
