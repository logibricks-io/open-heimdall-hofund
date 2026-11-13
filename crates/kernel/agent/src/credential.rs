//! hofund's key in the OS credential store, for agents that read it there.
//!
//! A record beside the managed file names the item hofund wrote and holds a
//! SHA-256 digest of the key, never the key. Disconnect compares the item's
//! secret with that digest, so it removes only a key hofund put there.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hofund_plugin_api::AgentCredential;
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};

use crate::catalog::fill_template;
use crate::store::{delete_file, read_optional, write_atomic};
use crate::{AgentError, Credential, CredentialError, CredentialStore, FileChange, GatewayTarget};

/// Text a plan path starts with when it names a credential store item rather
/// than a file.
pub const CREDENTIAL_CHANGE_PREFIX: &str = "credential store: ";

/// Reverses one tracked [`EngineCredential::put`].
#[derive(Clone)]
pub(crate) struct CredentialUndo {
    store: Arc<dyn CredentialStore>,
    item: Credential,
    prior_item_secret: Option<String>,
    removed_item: Option<Credential>,
    removed_secret: Option<String>,
}

pub(crate) struct EngineCredential<'a> {
    agent: &'static str,
    spec: &'a AgentCredential,
    record: PathBuf,
    store: Option<&'a Arc<dyn CredentialStore>>,
}

#[derive(PartialEq, Eq)]
struct Record {
    item: Credential,
    digest: String,
}

impl Record {
    fn bytes(&self) -> Vec<u8> {
        let value = serde_json::json!({
            "url": self.item.url(),
            "account": self.item.account(),
            "label": self.item.label(),
            "windows_target": self.item.windows_target(),
            "sha256": self.digest,
        });
        let mut bytes = serde_json::to_vec_pretty(&value).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }

    fn parse(path: &Path, bytes: &[u8]) -> Result<Self, AgentError> {
        let malformed = || {
            AgentError::message(format!(
                "{}: credential record is not one hofund wrote",
                path.display()
            ))
        };
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| malformed())?;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(malformed)
        };
        Ok(Self {
            item: Credential::new(
                field("url")?,
                field("account")?,
                field("label")?,
                field("windows_target")?,
            ),
            digest: field("sha256")?,
        })
    }
}

impl<'a> EngineCredential<'a> {
    pub(crate) fn new(
        agent: &'static str,
        spec: &'a AgentCredential,
        file: &Path,
        store: Option<&'a Arc<dyn CredentialStore>>,
    ) -> Self {
        Self {
            agent,
            spec,
            record: crate::store::credential_record(file),
            store,
        }
    }

    /// The item for `target`.
    pub(crate) fn item(&self, target: &GatewayTarget) -> Credential {
        Credential::new(
            fill_template(self.spec.url(), target),
            self.spec.account(),
            self.spec.label(),
            fill_template(self.spec.windows_target(), target),
        )
    }

    fn store(&self) -> Result<&dyn CredentialStore, AgentError> {
        self.store.map(AsRef::as_ref).ok_or_else(|| {
            AgentError::message(format!(
                "`{}` reads its key from the OS credential store, and none is available here",
                self.agent
            ))
        })
    }

    /// The plan entry connect would add: `None` when the item already holds
    /// the key.
    pub(crate) fn planned(&self, target: &GatewayTarget) -> Result<Option<FileChange>, AgentError> {
        let store = self.store()?;
        let item = self.item(target);
        if holds(store, &item, target.token())? {
            return Ok(None);
        }
        Ok(Some(change(&item)))
    }

    /// Puts the key in the item unless it is already there, and records it.
    /// An item hofund wrote for an earlier gateway URL is removed first.
    pub(crate) fn put(&self, target: &GatewayTarget) -> Result<Option<FileChange>, AgentError> {
        let store = self.store()?;
        let item = self.item(target);
        let previous = self.read_record()?;
        if let Some(previous) = &previous
            && previous.item != item
            && secret_matches(store, &previous.item, &previous.digest)?
        {
            store.delete(&previous.item).map_err(failure)?;
        }
        let changed = !holds(store, &item, target.token())?;
        if changed {
            store.write(&item, target.token()).map_err(failure)?;
        }
        let record = Record {
            item,
            digest: digest(target.token()),
        };
        if previous.as_ref() != Some(&record) {
            write_atomic(&self.record, &record.bytes())?;
        }
        Ok(changed.then(|| change(&record.item)))
    }

    /// Re-puts the key after a rotation, when connect recorded an item.
    /// Returns the plan path of the item when its secret changed.
    pub(crate) fn refresh(&self, target: &GatewayTarget) -> Result<Option<PathBuf>, AgentError> {
        if !self.record.is_file() {
            return Ok(None);
        }
        Ok(self.put(target)?.map(|change| change.path().to_path_buf()))
    }

    /// Whether the item holds the key. No store means not connected; a store
    /// that cannot be read also means not connected, with a warning naming it.
    pub(crate) fn connected(&self, target: &GatewayTarget) -> (bool, Option<String>) {
        let Ok(store) = self.store() else {
            return (false, None);
        };
        let item = self.item(target);
        match store.read(&item) {
            Ok(held) => (
                held.is_some_and(|secret| secret.expose_secret() == target.token()),
                None,
            ),
            Err(error) => (
                false,
                Some(format!(
                    "the OS credential store could not be read ({error}), so the item for {} is shown as not connected",
                    item.url()
                )),
            ),
        }
    }

    /// Removes the item when it still holds the key hofund wrote, or with
    /// `force` whatever it holds, then drops the record.
    pub(crate) fn release(&self, force: bool) -> Result<(bool, Vec<String>), AgentError> {
        let Some(record) = self.read_record()? else {
            return Ok((false, Vec::new()));
        };
        let url = record.item.url();
        let Ok(store) = self.store() else {
            return Ok((
                false,
                vec![format!(
                    "the OS credential store is not available here, so its item for {url} was left in place"
                )],
            ));
        };
        // A store that cannot be reached must not stop the files from being
        // restored: warn, and keep the record so a later disconnect retries.
        let unreachable = |error: CredentialError| {
            vec![format!(
                "the OS credential store could not be updated ({error}), so its item for {url} was left in place; disconnect again to retry"
            )]
        };
        let held = match store.read(&record.item) {
            Ok(held) => held,
            Err(error) => return Ok((false, unreachable(error))),
        };
        let mut warnings = Vec::new();
        if let Some(secret) = held {
            if force || digest(secret.expose_secret()) == record.digest {
                if let Err(error) = store.delete(&record.item) {
                    return Ok((false, unreachable(error)));
                }
            } else {
                warnings.push(format!(
                    "the credential store item for {url} holds a key hofund did not write, so it was left in place"
                ));
            }
        }
        delete_file(&self.record)?;
        Ok((true, warnings))
    }

    /// Captures credential-store state before [`Self::put`].
    pub(crate) fn capture_put_undo(
        &self,
        target: &GatewayTarget,
    ) -> Result<Option<CredentialUndo>, AgentError> {
        let Some(store) = self.store.cloned() else {
            return Ok(None);
        };
        let item = self.item(target);
        let prior_item_secret = store
            .read(&item)
            .map_err(failure)?
            .map(|secret| secret.expose_secret().to_owned());
        let mut removed_item = None;
        let mut removed_secret = None;
        if let Some(previous) = self.read_record()?
            && previous.item != item
            && secret_matches(store.as_ref(), &previous.item, &previous.digest)?
        {
            removed_item = Some(previous.item.clone());
            removed_secret = store
                .read(&previous.item)
                .map_err(failure)?
                .map(|secret| secret.expose_secret().to_owned());
        }
        Ok(Some(CredentialUndo {
            store,
            item,
            prior_item_secret,
            removed_item,
            removed_secret,
        }))
    }

    fn read_record(&self) -> Result<Option<Record>, AgentError> {
        read_optional(&self.record)?
            .map(|bytes| Record::parse(&self.record, &bytes))
            .transpose()
    }
}

impl CredentialUndo {
    pub(crate) fn restore(self) -> Result<(), AgentError> {
        if let Some(secret) = self.prior_item_secret {
            self.store.write(&self.item, &secret).map_err(failure)?;
        } else {
            let _ = self.store.delete(&self.item);
        }
        if let Some(item) = self.removed_item
            && let Some(secret) = self.removed_secret
        {
            self.store.write(&item, &secret).map_err(failure)?;
        }
        Ok(())
    }
}

fn holds(store: &dyn CredentialStore, item: &Credential, token: &str) -> Result<bool, AgentError> {
    Ok(store
        .read(item)
        .map_err(failure)?
        .is_some_and(|secret| secret.expose_secret() == token))
}

fn secret_matches(
    store: &dyn CredentialStore,
    item: &Credential,
    expected: &str,
) -> Result<bool, AgentError> {
    Ok(store
        .read(item)
        .map_err(failure)?
        .is_some_and(|secret| digest(secret.expose_secret()) == expected))
}

fn change(item: &Credential) -> FileChange {
    let path = format!("{CREDENTIAL_CHANGE_PREFIX}{}", item.url());
    let diff = format!("{path}\n+ account {}: hofund's key\n", item.account());
    FileChange::new(PathBuf::from(path), Vec::new(), Vec::new(), diff)
}

fn digest(secret: &str) -> String {
    let hash = Sha256::digest(secret.as_bytes());
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn failure(error: CredentialError) -> AgentError {
    AgentError::message(format!("credential store: {error}"))
}
