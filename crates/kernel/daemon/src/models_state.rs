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
    /// Set for a one-shot process: every write takes the per-provider state
    /// lock and rereads the file under it. The inner value is the runtime
    /// directory for the lock when the state directory cannot hold one.
    shared: Option<Option<PathBuf>>,
    /// The provider whose state lock this store holds now.
    held: Option<String>,
}

impl StateStore {
    pub(super) fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }

    /// Makes every write take the per-provider `<state-file>.lock` and
    /// reread the state under it, for a process that shares the state files
    /// with other writers. `fallback` is the runtime directory used when the
    /// state directory is read-only.
    pub(super) fn shared(mut self, fallback: Option<PathBuf>) -> Self {
        self.shared = Some(fallback);
        self
    }

    /// Runs `body` holding the state lock of `id`, after dropping the cached
    /// state so `body` reads the file as it is now.
    ///
    /// Without [`Self::shared`] (or a state root) no lock is taken and the
    /// cache is kept. Calls nested inside `body` for the same provider reuse
    /// the held lock.
    ///
    /// # Errors
    ///
    /// Returns [`StateError::Lock`] when the lock file cannot be opened or
    /// locked; `body` is not run then.
    pub(super) fn exclusive<R>(
        &mut self,
        id: &str,
        body: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, StateError> {
        let (Some(root), Some(fallback)) = (&self.root, &self.shared) else {
            return Ok(body(self));
        };
        if self.held.as_deref() == Some(id) {
            return Ok(body(self));
        }
        let primary = lock_path(root, id);
        if let Some(directory) = primary.parent() {
            let _ = create_private_dir(directory);
        }
        let fallback = fallback
            .as_ref()
            .map(|directory| directory.join(format!("models-{}.lock", filename(id))));
        let lock = super::config_write::SidecarLock::acquire(&primary, fallback.as_deref())
            .map_err(|error| {
                tracing::warn!(provider.id = id, %error, "could not lock model state");
                StateError::Lock(error)
            })?;
        self.states.remove(id);
        self.mismatched.remove(id);
        self.held = Some(id.to_owned());
        let result = body(self);
        self.held = None;
        drop(lock);
        Ok(result)
    }

    pub(super) fn get(&mut self, id: &str) -> &ProviderState {
        self.load(id);
        self.states
            .get(id)
            .unwrap_or_else(|| unreachable!("loading inserts provider state"))
    }

    /// Applies `change` in memory and saves it, logging a failed save.
    ///
    /// The in-memory change is kept even when the save fails, so the
    /// scheduled refresh keeps serving a fetched list.
    pub(super) fn update(&mut self, id: &str, change: impl FnOnce(&mut ProviderState)) {
        let _ = self.update_saved(id, change);
    }

    /// Like [`Self::update`], but also reports whether the save succeeded.
    pub(super) fn update_saved(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut ProviderState),
    ) -> Result<(), StateError> {
        self.exclusive(id, |store| store.update_saved_held(id, change))?
    }

    fn update_saved_held(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut ProviderState),
    ) -> Result<(), StateError> {
        self.load(id);
        if let Some(state) = self.states.get_mut(id) {
            change(state);
        }
        self.save(id)
    }

    /// Applies `change` and saves it, or restores the previous in-memory
    /// state and returns the error when the file cannot be written.
    pub(super) fn try_update(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut ProviderState),
    ) -> Result<(), StateError> {
        self.exclusive(id, |store| store.try_update_held(id, change))?
    }

    fn try_update_held(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut ProviderState),
    ) -> Result<(), StateError> {
        self.load(id);
        let Some(state) = self.states.get_mut(id) else {
            return Ok(());
        };
        let before = state.clone();
        change(state);
        let saved = self.save(id);
        if saved.is_err() {
            self.states.insert(id.to_owned(), before);
        }
        saved
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

    fn save(&mut self, id: &str) -> Result<(), StateError> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        if self.mismatched.contains(id) {
            tracing::warn!(
                provider.id = id,
                "model state was not saved because the existing file was protected from replacement"
            );
            return Err(StateError::Mismatched);
        }
        let Some(state) = self.states.get(id) else {
            return Ok(());
        };
        save_state(root, state).map_err(|error| {
            tracing::warn!(provider.id = id, %error, "could not save model state");
            StateError::Save(error)
        })
    }
}

/// Why a provider's model state could not be written.
#[derive(Debug)]
pub(super) enum StateError {
    /// The existing file belongs to another id and is never overwritten.
    Mismatched,
    /// Writing the file failed.
    Save(io::Error),
    /// The per-provider state lock could not be taken.
    Lock(io::Error),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mismatched => {
                formatter.write_str("the existing model state file belongs to another provider id")
            }
            Self::Save(error) => write!(formatter, "could not save model state: {error}"),
            Self::Lock(error) => write!(formatter, "could not lock model state: {error}"),
        }
    }
}

/// Serializes provider state exactly as it is written to disk.
pub(super) fn encode_state(state: &ProviderState) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(state).map_err(io::Error::other)
}

/// Hex SHA-256 of the bytes [`save_state`] writes for `state`.
pub(super) fn revision(state: &ProviderState) -> String {
    encode_state(state)
        .map(|bytes| crate::control::revision(&bytes))
        .unwrap_or_default()
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    free: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input_price: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_price: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read_price: Option<u64>,
}

impl From<&ModelSpec> for ModelRow {
    fn from(model: &ModelSpec) -> Self {
        let (input_price, output_price, cache_read_price) = model.prices();
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
            free: model.free(),
            input_price,
            output_price,
            cache_read_price,
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
        let model = match (self.input_price, self.output_price, self.cache_read_price) {
            (Some(input), Some(output), Some(cache_read)) => {
                model.with_prices(input, output, cache_read)
            }
            _ => model,
        };
        let model = match self.free {
            Some(free) => model.with_free(free),
            None => model,
        };
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
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    read_state_at(root, id, millis)
}

fn read_state_at(root: &Path, id: &str, millis: u128) -> (ProviderState, bool) {
    let path = state_path(root, id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (ProviderState::new(id), false);
        }
        Err(error) => {
            tracing::warn!(provider.id = id, %error, "could not read model state");
            return (ProviderState::new(id), true);
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
    create_private_dir(directory)?;
    let bytes = encode_state(state)?;
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

fn create_private_dir(directory: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The per-provider state lock: `<state-file>.lock` beside the state file.
fn lock_path(root: &Path, id: &str) -> PathBuf {
    let mut name = filename(id);
    name.push_str(".lock");
    state_path(root, id).with_file_name(name)
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
    use std::path::Path;

    use hofund_plugin_api::ModelSpec;

    use super::{
        ProviderState, StateError, StateStore, filename, lock_path, models, rows, state_path,
    };

    struct Scratch(tempfile::TempDir);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    impl AsRef<Path> for Scratch {
        fn as_ref(&self) -> &Path {
            self.0.path()
        }
    }

    fn temp_root(_test: &str) -> Scratch {
        Scratch(tempfile::tempdir().expect("temp dir"))
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
            .with_service_tiers(vec!["flex".to_owned()])
            .with_free(true)
            .with_prices(0, 0, 0);
        let mut store = StateStore::new(Some(root.to_path_buf()));
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

        let mut loaded = StateStore::new(Some(root.to_path_buf()));
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

        let mut store = StateStore::new(Some(root.to_path_buf()));
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
        let path = state_path(&root, "wanted");
        let directory = path.parent().expect("state directory");
        std::fs::create_dir_all(directory).expect("state directory");
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755))
            .expect("wide permissions");
        let mut store = StateStore::new(Some(root.to_path_buf()));
        store.update("wanted", |state| {
            state.picks = vec!["first".to_owned()];
        });
        store.update("wanted", |state| {
            state.picks = vec!["second".to_owned()];
        });
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
    fn unreadable_state_is_not_overwritten() {
        let root = temp_root("unreadable");
        let path = state_path(&root, "wanted");
        std::fs::create_dir_all(&path).expect("unreadable state directory");
        std::fs::write(path.join("occupied"), "keep").expect("original contents");

        let mut store = StateStore::new(Some(root.to_path_buf()));
        assert_eq!(store.get("wanted"), &ProviderState::new("wanted"));
        store.update("wanted", |state| {
            state.picks = vec!["model".to_owned()];
        });

        assert_eq!(
            std::fs::read(path.join("occupied")).expect("original contents"),
            b"keep"
        );
        assert_eq!(store.get("wanted").picks, ["model"]);
    }

    #[test]
    fn failed_move_aside_preserves_corrupt_state() {
        let root = temp_root("failed-move-aside");
        let path = state_path(&root, "wanted");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        let bytes = b"not json";
        std::fs::write(&path, bytes).expect("corrupt state");
        let name = path.file_name().expect("file name").to_string_lossy();
        let corrupt = path.with_file_name(format!("{name}.corrupt-42"));
        std::fs::create_dir_all(&corrupt).expect("corrupt destination directory");
        std::fs::write(corrupt.join("occupied"), "keep").expect("destination contents");

        let (state, protected) = super::read_state_at(&root, "wanted", 42);

        assert_eq!(state, ProviderState::new("wanted"));
        assert!(protected);
        assert_eq!(std::fs::read(&path).expect("original bytes"), bytes);
        assert_eq!(
            std::fs::read(corrupt.join("occupied")).expect("destination contents"),
            b"keep"
        );
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

    #[test]
    fn shared_store_rereads_the_file_under_the_state_lock() {
        let root = temp_root("shared-reread");
        let mut first = StateStore::new(Some(root.to_path_buf())).shared(None);
        let mut second = StateStore::new(Some(root.to_path_buf())).shared(None);
        assert!(first.get("p").picks.is_empty());
        second
            .try_update("p", |state| state.picks = vec!["a".to_owned()])
            .expect("second saves");
        first
            .try_update("p", |state| state.picks.push("b".to_owned()))
            .expect("first saves");
        assert_eq!(first.get("p").picks, ["a", "b"]);
        assert!(lock_path(&root, "p").is_file());
        assert_eq!(
            lock_path(&root, "p").parent(),
            state_path(&root, "p").parent()
        );
    }

    #[test]
    fn private_store_keeps_its_cache_and_takes_no_lock() {
        let root = temp_root("private-cache");
        let mut first = StateStore::new(Some(root.to_path_buf()));
        let mut second = StateStore::new(Some(root.to_path_buf()));
        assert!(first.get("p").picks.is_empty());
        second
            .try_update("p", |state| state.picks = vec!["a".to_owned()])
            .expect("second saves");
        first
            .try_update("p", |state| state.picks.push("b".to_owned()))
            .expect("first saves");
        assert_eq!(first.get("p").picks, ["b"]);
        assert!(!lock_path(&root, "p").exists());
    }

    #[test]
    fn nested_exclusive_reuses_the_held_lock() {
        let root = temp_root("nested");
        let mut store = StateStore::new(Some(root.to_path_buf())).shared(None);
        let saved = store
            .exclusive("p", |store| {
                store.try_update("p", |state| state.picks = vec!["a".to_owned()])
            })
            .expect("locked");
        saved.expect("saved");
        assert_eq!(store.get("p").picks, ["a"]);
    }

    #[test]
    fn held_state_lock_blocks_another_writer() {
        let root = temp_root("blocks");
        let lock = lock_path(&root, "p");
        std::fs::create_dir_all(lock.parent().expect("parent")).expect("directory");
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .expect("lock file");
        holder.lock().expect("held");
        let path = root.to_path_buf();
        let (done, finished) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let mut store = StateStore::new(Some(path)).shared(None);
            let saved = store.try_update("p", |state| state.picks = vec!["a".to_owned()]);
            let _ = done.send(());
            saved
        });
        assert!(
            finished
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the writer must wait for the held lock"
        );
        drop(holder);
        finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the writer finishes once the lock is released");
        writer.join().expect("writer thread").expect("saved");
    }

    #[cfg(unix)]
    #[test]
    fn read_only_state_dir_locks_in_the_runtime_fallback() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("read-only");
        let runtime = temp_root("read-only-runtime");
        std::fs::set_permissions(&*root, std::fs::Permissions::from_mode(0o500))
            .expect("read-only root");
        if std::fs::create_dir(root.join("probe")).is_ok() {
            // Running as root: permissions do not apply.
            return;
        }
        let mut store =
            StateStore::new(Some(root.to_path_buf())).shared(Some(runtime.to_path_buf()));
        let saved = store.try_update("p", |state| state.picks = vec!["a".to_owned()]);
        std::fs::set_permissions(&*root, std::fs::Permissions::from_mode(0o700))
            .expect("writable again");
        assert!(matches!(saved, Err(StateError::Save(_))), "{saved:?}");
        assert!(
            runtime
                .join(format!("models-{}.lock", filename("p")))
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unopenable_state_lock_fails_without_writing() {
        let root = temp_root("no-lock");
        let lock = lock_path(&root, "p");
        std::fs::create_dir_all(&lock).expect("a directory where the lock goes");
        let mut store = StateStore::new(Some(root.to_path_buf())).shared(None);
        let saved = store.try_update("p", |state| state.picks = vec!["a".to_owned()]);
        assert!(matches!(saved, Err(StateError::Lock(_))), "{saved:?}");
        assert!(!state_path(&root, "p").exists());
    }
}
