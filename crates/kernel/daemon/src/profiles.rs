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
        let mut profiles = self.load()?;
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
    fn g1_7_zeroed_main_file_reads_backup() {
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
        fs::copy(&store.path, &store.backup).expect("seed backup");
        fs::write(&store.path, vec![0_u8; 512]).expect("zero");
        let fresh = ProfileStore::new(&config, EMPTY_AGENTS);
        let loaded = fresh.load().expect("load");
        assert_eq!(
            loaded
                .get("work")
                .and_then(|fields| fields.get("codex.model"))
                .map(String::as_str),
            Some("a")
        );
    }

    #[test]
    fn g1_12_corrupt_main_reads_valid_backup() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("hofund.toml");
        fs::write(&config, "[gateway]\nlisten = \"127.0.0.1:0\"\n").expect("config");
        let store = ProfileStore::new(&config, EMPTY_AGENTS);
        let mut profiles = BTreeMap::new();
        profiles.insert(
            "saved".to_owned(),
            BTreeMap::from([("agent.field".to_owned(), "from-backup".to_owned())]),
        );
        store.store(profiles).expect("seed");
        fs::copy(&store.path, &store.backup).expect("seed backup");
        fs::write(&store.path, b"{{not valid toml").expect("corrupt main");
        let fresh = ProfileStore::new(&config, EMPTY_AGENTS);
        let loaded = fresh.load().expect("load from backup");
        assert_eq!(
            loaded
                .get("saved")
                .and_then(|fields| fields.get("agent.field"))
                .map(String::as_str),
            Some("from-backup")
        );
    }

    #[test]
    fn g1_13_store_refreshes_backup_with_previous_main() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("hofund.toml");
        fs::write(&config, "[gateway]\nlisten = \"127.0.0.1:0\"\n").expect("config");
        let store = ProfileStore::new(&config, EMPTY_AGENTS);
        let first = BTreeMap::from([(
            "alpha".to_owned(),
            BTreeMap::from([("a".to_owned(), "1".to_owned())]),
        )]);
        store.store(first).expect("first write");
        let before = fs::read(&store.path).expect("read main");
        let mut second = BTreeMap::new();
        second.insert(
            "beta".to_owned(),
            BTreeMap::from([("b".to_owned(), "2".to_owned())]),
        );
        store.store(second).expect("second write");
        let backup = fs::read(&store.backup).expect("read backup");
        assert_eq!(backup, before);
    }

    #[test]
    fn g1_14_delete_unknown_profile_errors() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("hofund.toml");
        fs::write(&config, "[gateway]\nlisten = \"127.0.0.1:0\"\n").expect("config");
        let store = ProfileStore::new(&config, EMPTY_AGENTS);
        let error = store.delete("missing").expect_err("delete unknown profile");
        assert!(error.contains("missing"));
    }

    #[test]
    fn g1_15_empty_profile_name_errors() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("hofund.toml");
        fs::write(&config, "[gateway]\nlisten = \"127.0.0.1:0\"\n").expect("config");
        let store = ProfileStore::new(&config, EMPTY_AGENTS);
        let fields = BTreeMap::from([("a".to_owned(), "b".to_owned())]);
        store
            .save("   ", fields.clone())
            .expect_err("whitespace name");
        store.save("", fields).expect_err("empty name");
    }

    #[test]
    fn g1_16_save_fails_when_profiles_unreadable() {
        let dir = tempfile::tempdir().expect("dir");
        let config = dir.path().join("hofund.toml");
        fs::write(&config, "[gateway]\nlisten = \"127.0.0.1:0\"\n").expect("config");
        let root = crate::beside(&config, ".hofund");
        fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("profiles.toml");
        let backup = root.join("profiles.toml.bak");
        let good = r#"
[profiles.alpha]
a = "1"

[profiles.beta]
b = "2"
"#;
        fs::write(&path, good).expect("write main");
        fs::write(&backup, b"{{bad backup").expect("bad backup");
        fs::write(&path, b"{{bad main").expect("corrupt main");
        let store = ProfileStore::new(&config, EMPTY_AGENTS);
        let fields = BTreeMap::from([("x".to_owned(), "y".to_owned())]);
        let error = store
            .save("gamma", fields)
            .expect_err("save must not drop profiles");
        assert!(error.contains("profiles.toml"), "unexpected error: {error}");
        let still = fs::read_to_string(&path).expect("main untouched");
        assert!(still.contains("{{bad main"));
        assert!(!still.contains("[profiles.gamma]"));
    }
}
