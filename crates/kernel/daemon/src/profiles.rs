//! Persisted agent setting profiles beside the config file.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hofund_agent::{ProfileFields, Session, catalog, snapshot_fields};
use hofund_plugin_api::AgentDescription;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProfilesDocument {
    #[serde(default)]
    profiles: BTreeMap<String, ProfileFields>,
}

pub(crate) struct ProfileStore {
    path: PathBuf,
    backup: PathBuf,
    descriptions: &'static [&'static AgentDescription],
    cached: Mutex<Option<ProfilesDocument>>,
}

impl ProfileStore {
    pub(crate) fn new(
        config_path: &Path,
        descriptions: &'static [&'static AgentDescription],
    ) -> Self {
        let root = crate::beside(config_path, ".hofund");
        Self {
            path: root.join("profiles.toml"),
            backup: root.join("profiles.toml.bak"),
            descriptions,
            cached: Mutex::new(None),
        }
    }

    pub(crate) fn load(&self) -> Result<BTreeMap<String, ProfileFields>, String> {
        if let Some(document) = self.cached.lock().expect("profiles").clone() {
            return Ok(document.profiles);
        }
        let document = self.read_document()?;
        *self.cached.lock().expect("profiles") = Some(document.clone());
        Ok(document.profiles)
    }

    pub(crate) fn names(&self) -> Result<Vec<String>, String> {
        let mut names: Vec<String> = self.load()?.into_keys().collect();
        names.sort();
        Ok(names)
    }

    pub(crate) fn get(&self, name: &str) -> Result<Option<ProfileFields>, String> {
        Ok(self.load()?.get(name).cloned())
    }

    pub(crate) fn save(&self, name: &str, fields: ProfileFields) -> Result<(), String> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("profile name is empty".to_owned());
        }
        let mut profiles = self.load().unwrap_or_default();
        profiles.insert(trimmed.to_owned(), fields);
        self.store(profiles)
    }

    pub(crate) fn delete(&self, name: &str) -> Result<(), String> {
        let mut profiles = self.load()?;
        if profiles.remove(name).is_none() {
            return Err(format!("no profile named `{name}`"));
        }
        self.store(profiles)
    }

    pub(crate) fn snapshot(&self, session: &Session) -> Result<ProfileFields, String> {
        let catalog =
            catalog(self.descriptions).map_err(|error| format!("agent catalog: {error}"))?;
        snapshot_fields(&catalog, session).map_err(|error| error.to_string())
    }

    fn read_document(&self) -> Result<ProfilesDocument, String> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfilesDocument::default());
            }
            Err(error) => return Err(format!("{}: {error}", self.path.display())),
        };
        if bytes.iter().all(|byte| *byte == 0) {
            return self.read_backup();
        }
        match toml::from_str::<ProfilesDocument>(
            &String::from_utf8(bytes).map_err(|error| error.to_string())?,
        ) {
            Ok(document) => Ok(document),
            Err(error) => {
                if let Ok(document) = self.read_backup() {
                    return Ok(document);
                }
                Err(format!("{}: {error}", self.path.display()))
            }
        }
    }

    fn read_backup(&self) -> Result<ProfilesDocument, String> {
        let bytes = fs::read(&self.backup)
            .map_err(|error| format!("{}: {error}", self.backup.display()))?;
        if bytes.iter().all(|byte| *byte == 0) {
            return Ok(ProfilesDocument::default());
        }
        toml::from_str(&String::from_utf8(bytes).map_err(|error| error.to_string())?)
            .map_err(|error| format!("{}: {error}", self.backup.display()))
    }

    fn store(&self, profiles: BTreeMap<String, ProfileFields>) -> Result<(), String> {
        if self.path.is_file()
            && let Ok(bytes) = fs::read(&self.path)
            && valid_profiles(&bytes)
        {
            self.write_atomic(&self.backup, &bytes)?;
        }
        let document = ProfilesDocument { profiles };
        let text = toml::to_string_pretty(&document).map_err(|error| error.to_string())?;
        self.write_atomic(&self.path, text.as_bytes())?;
        *self.cached.lock().expect("profiles") = Some(document);
        Ok(())
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temporary = path.with_extension("tmp");
        let result = (|| -> std::io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(|error| format!("{}: {error}", path.display()))
    }
}

fn valid_profiles(bytes: &[u8]) -> bool {
    if bytes.iter().all(|byte| *byte == 0) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    toml::from_str::<ProfilesDocument>(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    static EMPTY_AGENTS: &[&AgentDescription] = &[];

    #[test]
    fn zeroed_main_file_reads_backup() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("hofund.toml");
        fs::write(&config, "[gateway]\nlisten = \"127.0.0.1:0\"\n").expect("config");
        let store = ProfileStore::new(&config, EMPTY_AGENTS);
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "work".to_owned(),
            BTreeMap::from([("codex.model".to_owned(), "a".to_owned())]),
        );
        store.store(profiles).expect("store");
        fs::write(&store.path, vec![0_u8; 512]).expect("zero");
        let loaded = store.load().expect("load");
        assert_eq!(
            loaded
                .get("work")
                .and_then(|fields| fields.get("codex.model"))
                .map(String::as_str),
            Some("a")
        );
    }
}
