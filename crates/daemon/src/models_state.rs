//! Persistent, host-owned state for configured provider model lists.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_plugin_api::{AuthChoice, ModelSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
pub(super) struct StateStore {
    root: Option<PathBuf>,
    states: HashMap<String, ProviderState>,
    mismatched: HashSet<String>,
}

impl StateStore {
    pub(super) fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }

    pub(super) fn get(&mut self, id: &str) -> &ProviderState {
        self.load(id);
        self.states
            .get(id)
            .unwrap_or_else(|| unreachable!("loading inserts provider state"))
    }

    pub(super) fn update(&mut self, id: &str, change: impl FnOnce(&mut ProviderState)) {
        self.load(id);
        if let Some(state) = self.states.get_mut(id) {
            change(state);
        }
        self.save(id);
    }

    fn load(&mut self, id: &str) {
        if self.states.contains_key(id) {
            return;
        }
        let state = self.root.as_ref().map_or_else(
            || (ProviderState::new(id), false),
            |root| read_state(root, id),
        );
        if state.1 {
            self.mismatched.insert(id.to_owned());
        }
        self.states.insert(id.to_owned(), state.0);
    }

    fn save(&mut self, id: &str) {
        let Some(root) = &self.root else {
            return;
        };
        if self.mismatched.contains(id) {
            tracing::warn!(
                provider.id = id,
                "model state was not saved because the existing file belongs to another id"
            );
            return;
        }
        let Some(state) = self.states.get(id) else {
            return;
        };
        if let Err(error) = save_state(root, state) {
            tracing::warn!(provider.id = id, %error, "could not save model state");
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ProviderState {
    pub(super) version: u32,
    pub(super) id: String,
    #[serde(default)]
    pub(super) fetched_at: Option<u64>,
    #[serde(default)]
    pub(super) tried_at: Option<u64>,
    #[serde(default)]
    pub(super) retry_at: Option<u64>,
    #[serde(default)]
    pub(super) list: Vec<ModelRow>,
    #[serde(default)]
    pub(super) sides: BTreeMap<String, Vec<ModelRow>>,
    #[serde(default)]
    pub(super) picks: Vec<String>,
    #[serde(default)]
    pub(super) picked_from: Vec<String>,
    #[serde(default)]
    pub(super) delisted: Vec<String>,
    #[serde(default)]
    pub(super) auth: Option<StoredAuthChoice>,
    #[serde(default)]
    pub(super) bases: BTreeMap<String, String>,
}

impl ProviderState {
    fn new(id: &str) -> Self {
        Self {
            version: 1,
            id: id.to_owned(),
            fetched_at: None,
            tried_at: None,
            retry_at: None,
            list: Vec::new(),
            sides: BTreeMap::new(),
            picks: Vec::new(),
            picked_from: Vec::new(),
            delisted: Vec::new(),
            auth: None,
            bases: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum StoredAuthChoice {
    Both,
    XApiKey,
    Bearer,
}

impl From<AuthChoice> for StoredAuthChoice {
    fn from(choice: AuthChoice) -> Self {
        match choice {
            AuthChoice::Both => Self::Both,
            AuthChoice::XApiKey => Self::XApiKey,
            AuthChoice::Bearer => Self::Bearer,
            _ => Self::Both,
        }
    }
}

impl From<StoredAuthChoice> for AuthChoice {
    fn from(choice: StoredAuthChoice) -> Self {
        match choice {
            StoredAuthChoice::Both => Self::Both,
            StoredAuthChoice::XApiKey => Self::XApiKey,
            StoredAuthChoice::Bearer => Self::Bearer,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ModelRow {
    pub(super) id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context: u64,
    #[serde(default)]
    max_output: u64,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
    #[serde(default)]
    image_input: Option<bool>,
    #[serde(default)]
    endpoints: Vec<String>,
    #[serde(default)]
    draws: bool,
    #[serde(default)]
    films: bool,
    #[serde(default)]
    decides: bool,
    #[serde(default)]
    efforts: Vec<String>,
    #[serde(default)]
    web_search: Option<String>,
    #[serde(default)]
    service_tiers: Vec<String>,
}

impl From<&ModelSpec> for ModelRow {
    fn from(model: &ModelSpec) -> Self {
        Self {
            id: model.id().to_owned(),
            display_name: model.display_name().map(str::to_owned),
            context: model.context(),
            max_output: model.max_output(),
            input_modalities: model.input_modalities().to_vec(),
            output_modalities: model.output_modalities().to_vec(),
            image_input: model.image_input(),
            endpoints: model.endpoints().to_vec(),
            draws: model.draws(),
            films: model.films(),
            decides: model.decides(),
            efforts: model.efforts().to_vec(),
            web_search: model.web_search().map(str::to_owned),
            service_tiers: model.service_tiers().to_vec(),
        }
    }
}

impl ModelRow {
    fn to_model(&self) -> Option<ModelSpec> {
        let model = ModelSpec::new(self.id.clone(), self.display_name.clone())
            .ok()?
            .with_context(self.context)
            .with_max_output(self.max_output)
            .with_input_modalities(self.input_modalities.clone())
            .with_output_modalities(self.output_modalities.clone())
            .with_image_input(self.image_input)
            .with_endpoints(self.endpoints.clone())
            .with_draws(self.draws)
            .with_films(self.films)
            .with_decides(self.decides)
            .with_efforts(self.efforts.clone())
            .with_service_tiers(self.service_tiers.clone());
        Some(match &self.web_search {
            Some(search) => model.with_web_search(search),
            None => model,
        })
    }
}

pub(super) fn rows(models: &[ModelSpec]) -> Vec<ModelRow> {
    models.iter().map(ModelRow::from).collect()
}

pub(super) fn models(rows: &[ModelRow]) -> Vec<ModelSpec> {
    rows.iter().filter_map(ModelRow::to_model).collect()
}

fn read_state(root: &Path, id: &str) -> (ProviderState, bool) {
    let path = state_path(root, id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (ProviderState::new(id), false);
        }
        Err(error) => {
            tracing::warn!(provider.id = id, %error, "could not read model state");
            return (ProviderState::new(id), false);
        }
    };
    match serde_json::from_slice::<ProviderState>(&bytes) {
        Ok(state) if state.version == 1 && state.id == id => (state, false),
        Ok(_) => {
            tracing::warn!(
                provider.id = id,
                "ignoring model state whose version or stored id does not match"
            );
            (ProviderState::new(id), true)
        }
        Err(error) => {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("models-state.json");
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let corrupt_name = format!("{name}.corrupt-{millis}");
            match std::fs::rename(&path, path.with_file_name(&corrupt_name)) {
                Ok(()) => {
                    tracing::warn!(
                        provider.id = id,
                        corrupt.file = corrupt_name,
                        "moved unreadable model state aside"
                    );
                    (ProviderState::new(id), false)
                }
                Err(rename_error) => {
                    tracing::warn!(
                        provider.id = id,
                        %error,
                        %rename_error,
                        "could not move unreadable model state aside; it will not be overwritten"
                    );
                    (ProviderState::new(id), true)
                }
            }
        }
    }
}

fn save_state(root: &Path, state: &ProviderState) -> io::Result<()> {
    let path = state_path(root, &state.id);
    let directory = path
        .parent()
        .unwrap_or_else(|| unreachable!("the state path always has a parent"));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("models-state"),
        std::process::id(),
        sequence
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &path)?;
        #[cfg(unix)]
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn state_path(root: &Path, id: &str) -> PathBuf {
    root.join("host").join("models").join(filename(id))
}

fn filename(id: &str) -> String {
    let slug = id
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .take(40)
        .collect::<String>();
    let hash = format!("{:x}", Sha256::digest(id.as_bytes()));
    format!("{slug}-{}.json", &hash[..16])
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use hofund_plugin_api::ModelSpec;

    use super::{ProviderState, StateStore, filename, models, rows, state_path};

    fn temp_root(test: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("hofund-model-state-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn assert_safe_filename(id: &str) {
        let name = filename(id);
        let stem = name.strip_suffix(".json").expect("JSON suffix");
        let (slug, hash) = stem.rsplit_once('-').expect("hash separator");
        assert!(slug.len() <= 40);
        assert!(slug.chars().all(|character| character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '-'));
        assert_eq!(hash.len(), 16);
        assert!(
            hash.chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
        );
        let path = Path::new("root").join("host").join("models").join(&name);
        assert_eq!(path.parent(), Some(Path::new("root/host/models")));
        assert_eq!(
            path.file_name().and_then(|part| part.to_str()),
            Some(name.as_str())
        );
    }

    #[test]
    fn filenames_are_case_distinct_and_have_sixteen_hash_characters() {
        let upper = filename("Primary");
        let lower = filename("primary");
        assert!(upper.starts_with("primary-"));
        assert!(lower.starts_with("primary-"));
        assert_eq!(&upper[..8], &lower[..8]);
        assert_ne!(upper, lower);
        assert_safe_filename("Primary");
        assert_safe_filename("primary");
    }

    #[test]
    fn filenames_contain_unusual_and_long_ids() {
        for id in [
            "a/b".to_owned(),
            "../x".to_owned(),
            "..".to_owned(),
            r"C:\con".to_owned(),
            "a".repeat(100),
            "provider-é".to_owned(),
        ] {
            assert_safe_filename(&id);
        }
    }

    #[test]
    fn state_round_trips_every_host_model_field() {
        let root = temp_root("round-trip");
        let model = ModelSpec::new("model", Some("Model".to_owned()))
            .expect("model")
            .with_context(1000)
            .with_max_output(100)
            .with_input_modalities(vec!["text".to_owned(), "image".to_owned()])
            .with_output_modalities(vec!["text".to_owned()])
            .with_image_input(Some(true))
            .with_endpoints(vec!["chat".to_owned()])
            .with_draws(true)
            .with_films(true)
            .with_decides(true)
            .with_efforts(vec!["high".to_owned()])
            .with_web_search("native")
            .with_service_tiers(vec!["flex".to_owned()]);
        let mut store = StateStore::new(Some(root.clone()));
        store.update("Primary", |state| {
            state.fetched_at = Some(100);
            state.tried_at = Some(90);
            state.retry_at = Some(200);
            state.list = rows(std::slice::from_ref(&model));
            state.sides.insert(
                "https://example.test/v1".to_owned(),
                rows(std::slice::from_ref(&model)),
            );
            state.picks = vec!["model".to_owned()];
            state.picked_from = vec!["model".to_owned()];
            state.delisted = vec!["old".to_owned()];
            state.auth = Some(super::StoredAuthChoice::XApiKey);
            state
                .bases
                .insert("chat".to_owned(), "https://example.test/v1".to_owned());
        });

        let mut loaded = StateStore::new(Some(root.clone()));
        let state = loaded.get("Primary");
        assert_eq!(state.fetched_at, Some(100));
        assert_eq!(state.tried_at, Some(90));
        assert_eq!(state.retry_at, Some(200));
        assert_eq!(models(&state.list), vec![model.clone()]);
        assert_eq!(models(&state.sides["https://example.test/v1"]), vec![model]);
        assert_eq!(state.picks, ["model"]);
        assert_eq!(state.picked_from, ["model"]);
        assert_eq!(state.delisted, ["old"]);
        assert_eq!(state.auth, Some(super::StoredAuthChoice::XApiKey));
        assert_eq!(state.bases["chat"], "https://example.test/v1");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mismatched_stored_id_is_ignored_and_never_overwritten() {
        let root = temp_root("mismatch");
        let path = state_path(&root, "wanted");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        let bytes = br#"{"version":1,"id":"other"}"#.as_slice();
        std::fs::write(&path, bytes).expect("mismatched state");

        let mut store = StateStore::new(Some(root.clone()));
        assert_eq!(store.get("wanted"), &ProviderState::new("wanted"));
        store.update("wanted", |state| {
            state.picks = vec!["model".to_owned()];
        });
        assert_eq!(std::fs::read(&path).expect("existing bytes"), bytes);
        assert_eq!(store.get("wanted").picks, ["model"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn saved_state_uses_private_modes_and_leaves_no_temp_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("private-modes");
        let mut store = StateStore::new(Some(root.clone()));
        store.update("wanted", |state| {
            state.picks = vec!["first".to_owned()];
        });
        store.update("wanted", |state| {
            state.picks = vec!["second".to_owned()];
        });
        let path = state_path(&root, "wanted");
        let directory = path.parent().expect("state directory");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("state metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(
            std::fs::read_dir(directory)
                .expect("directory entries")
                .all(|entry| entry
                    .expect("entry")
                    .path()
                    .extension()
                    .and_then(|part| part.to_str())
                    != Some("tmp"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failed_rename_removes_the_temp_file() {
        let root = temp_root("failed-rename");
        let path = state_path(&root, "wanted");
        std::fs::create_dir_all(&path).expect("destination directory");
        std::fs::write(path.join("occupied"), "keep").expect("destination contents");

        super::save_state(&root, &ProviderState::new("wanted")).expect_err("rename must fail");
        let directory = path.parent().expect("state directory");
        assert!(
            std::fs::read_dir(directory)
                .expect("directory entries")
                .all(|entry| entry
                    .expect("entry")
                    .path()
                    .extension()
                    .and_then(|part| part.to_str())
                    != Some("tmp"))
        );
        assert_eq!(
            std::fs::read(path.join("occupied")).expect("contents"),
            b"keep"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
