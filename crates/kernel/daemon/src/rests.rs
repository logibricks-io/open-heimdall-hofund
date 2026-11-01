use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use hofund_plugin_api::MemberId;
use hofund_router::{Cooldowns, PersistedRest};

pub(crate) struct RestStore {
    path: PathBuf,
    last: Option<Vec<PersistedRest>>,
}

impl RestStore {
    pub(crate) fn new(config_path: &Path) -> Self {
        let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
        Self {
            path: parent.join("rests.json"),
            last: None,
        }
    }

    pub(crate) fn load(&mut self, cooldowns: &Cooldowns, configured: &HashSet<MemberId>) {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.last = Some(Vec::new());
                return;
            }
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "credit rests were not read");
                self.last = Some(Vec::new());
                return;
            }
        };
        let rests = match serde_json::from_slice::<Vec<PersistedRest>>(&bytes) {
            Ok(rests) => rests,
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "credit rests file is corrupt");
                self.last = Some(Vec::new());
                return;
            }
        };
        let now = SystemTime::now();
        cooldowns.restore_credit(rests, configured, now);
        self.last = Some(cooldowns.credit_snapshot(now));
    }

    pub(crate) fn save(&mut self, cooldowns: &Cooldowns) {
        let rests = cooldowns.credit_snapshot(SystemTime::now());
        if self
            .last
            .as_ref()
            .is_some_and(|last| same_rests(last, &rests))
        {
            return;
        }
        if rests.is_empty() && !self.path.exists() {
            self.last = Some(rests);
            return;
        }
        let bytes = encoded(&rests);
        if let Err(error) = self.write_atomic(&bytes) {
            tracing::warn!(path = %self.path.display(), %error, "credit rests were not saved");
            return;
        }
        self.last = Some(rests);
    }

    fn write_atomic(&self, bytes: &[u8]) -> std::io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temporary = self.path.with_file_name(format!(
            "{}.tmp",
            self.path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("rests.json")
        ));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

fn same_rests(left: &[PersistedRest], right: &[PersistedRest]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.member == right.member
                && left.model == right.model
                && left.rung == right.rung
                && match left.until.duration_since(right.until) {
                    Ok(difference) => difference <= std::time::Duration::from_secs(1),
                    Err(error) => error.duration() <= std::time::Duration::from_secs(1),
                }
        })
}

fn encoded(rests: &[PersistedRest]) -> Vec<u8> {
    serde_json::to_vec(rests).expect("credit rests serialize")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use hofund_plugin_api::{MemberKey, PluginError, PresetId};
    use hofund_router::{CooldownPolicy, Cooldowns, MemberId};

    use super::RestStore;

    fn fixture() -> (
        RestStore,
        Cooldowns,
        MemberId,
        Arc<Mutex<Instant>>,
        tempfile::TempDir,
    ) {
        let root = tempfile::tempdir().expect("rests directory");
        let store = RestStore::new(&root.path().join("hofund.toml"));
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = Arc::clone(&now);
        let book = Cooldowns::with_clock(
            CooldownPolicy::default(),
            Arc::new(move || *clock.lock().expect("clock")),
        );
        let id = MemberId::from_preset(
            PresetId::from_static("member"),
            MemberKey::from_static("default"),
        );
        (store, book, id, now, root)
    }

    #[test]
    fn b2a_persist_written_atomically_only_on_change() {
        let (mut store, book, id, clock, root) = fixture();
        book.record_failure(&id, &PluginError::credit_status(402, "out of credit"));
        store.save(&book);
        let path = root.path().join("rests.json");
        let saved: Vec<hofund_router::PersistedRest> =
            serde_json::from_slice(&std::fs::read(&path).expect("saved rests")).expect("rests");
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].rung, 1);
        assert!(!root.path().join("rests.json.tmp").exists());

        std::fs::write(&path, "unchanged sentinel").expect("sentinel");
        store.save(&book);
        assert_eq!(
            std::fs::read_to_string(&path).expect("unchanged file"),
            "unchanged sentinel"
        );

        *clock.lock().expect("clock") += Duration::from_secs(30 * 60 + 1);
        book.record_failure(&id, &PluginError::credit_status(402, "out of credit"));
        store.save(&book);
        let changed: Vec<hofund_router::PersistedRest> =
            serde_json::from_slice(&std::fs::read(&path).expect("changed rests")).expect("rests");
        assert_eq!(changed[0].rung, 2);
        assert!(!root.path().join("rests.json.tmp").exists());
    }

    #[test]
    fn b2b_unreadable_or_corrupt_file_adds_nothing() {
        for corrupt in ["not json", "[1,2]", "{\"member\":3}"] {
            let (mut store, book, id, _, root) = fixture();
            std::fs::write(root.path().join("rests.json"), corrupt).expect("corrupt file");
            store.load(&book, &HashSet::from([id.clone()]));
            assert_eq!(book.revision(&id), 0, "{corrupt}");
        }
        let (mut store, book, id, _, root) = fixture();
        std::fs::create_dir(root.path().join("rests.json")).expect("unreadable directory");
        store.load(&book, &HashSet::from([id.clone()]));
        assert_eq!(book.revision(&id), 0);
    }

    #[cfg(unix)]
    #[test]
    fn b2a_rests_file_is_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let (mut store, book, id, _, root) = fixture();
        book.record_failure(&id, &PluginError::credit_status(402, "out of credit"));
        store.save(&book);
        let mode = std::fs::metadata(root.path().join("rests.json"))
            .expect("saved rests")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn b2a_rung_change_at_same_deadline_is_a_change() {
        let until = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let left = [hofund_router::PersistedRest::new("m".to_owned(), until, 1)];
        let right = [hofund_router::PersistedRest::new("m".to_owned(), until, 2)];
        assert!(super::same_rests(&left, &left));
        assert!(!super::same_rests(&left, &right));
    }

    #[test]
    fn b5h_credit_rest_persistence_carries_and_restores_its_model() {
        let (mut store, book, id, clock, root) = fixture();
        let error = PluginError::credit_status(
            429,
            "All credentials for model model-a are cooling down (last error: insufficient balance)",
        );
        book.record_failure_for_model(&id, "model-a", &error)
            .expect("model credit");
        store.save(&book);

        let path = root.path().join("rests.json");
        let text = std::fs::read_to_string(&path).expect("saved model rest");
        assert!(text.contains("\"model\":\"model-a\""), "{text}");
        let saved: Vec<hofund_router::PersistedRest> =
            serde_json::from_str(&text).expect("saved rests");
        assert_eq!(saved[0].model.as_deref(), Some("model-a"));

        let restored_clock = Arc::clone(&clock);
        let restored = Cooldowns::with_clock(
            CooldownPolicy::default(),
            Arc::new(move || *restored_clock.lock().expect("clock")),
        );
        store.load(&restored, &HashSet::from([id.clone()]));
        assert!(restored.cooling(&id).is_none());
        let cooling = restored
            .cooling_for_model(&id, "model-a")
            .expect("restored model rest");
        assert_eq!(cooling.failures(), 1);
        assert!(restored.cooling_for_model(&id, "model-b").is_none());
    }
}
