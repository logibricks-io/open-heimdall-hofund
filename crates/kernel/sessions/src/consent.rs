//! Persistent consent for reading agents' own session files.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const CONSENT_FILE: &str = "sessions-consent.json";
const LOCK_FILE: &str = "sessions-consent.lock";
const INDEX_FILE: &str = "sessions-index.json";

/// The saved answer to which agents' session files may be read.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Reads {
    /// Whether the user has been asked. No files are read before this is true.
    pub asked: bool,
    /// The default for agents without an explicit entry.
    #[serde(default, skip_serializing_if = "is_false")]
    pub all: bool,
    /// Per-agent answers. An entry wins over [`Reads::all`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, bool>,
}

impl Reads {
    /// Whether `agent` may be read.
    #[must_use]
    pub fn allows(&self, agent: &str) -> bool {
        self.asked && self.agents.get(agent).copied().unwrap_or(self.all)
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// A consent update failed without changing the in-process answer.
#[derive(Debug, Error)]
pub enum ConsentError {
    /// An agent id is not valid.
    #[error("invalid agent id `{0}`")]
    InvalidAgent(String),
    /// The consent store could not be read or written.
    #[error("cannot update session read consent: {0}")]
    Io(#[from] io::Error),
    /// The existing consent store is not valid JSON.
    #[error("cannot update session read consent: invalid stored consent")]
    InvalidStored,
}

/// Process-local consent state backed by one config directory.
///
/// Call [`Consent::settle`] once before listing. A failed settle remains
/// read-nothing for the lifetime of this value and is never retried.
#[derive(Debug)]
pub struct Consent {
    config_dir: PathBuf,
    reads: Reads,
    settled: bool,
}

impl Consent {
    /// Creates an unsettled consent gate rooted at `config_dir`.
    #[must_use]
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            reads: Reads::default(),
            settled: false,
        }
    }

    /// Loads the saved answer, or grants an upgrade only when the index existed
    /// before this call. Save and load failures leave this process read-nothing.
    pub fn settle(&mut self) {
        if self.settled {
            return;
        }
        self.settled = true;
        let index_existed = self.index_path().is_file();
        let settled = self.with_lock(|this| {
            let Ok(existing) = read_stored(&this.path()) else {
                return Reads::default();
            };
            if let Some(existing) = existing {
                return existing;
            }
            let proposed = Reads {
                asked: index_existed,
                all: index_existed,
                agents: BTreeMap::new(),
            };
            if atomic_write(&this.path(), &json_bytes(&proposed)).is_err() {
                return Reads::default();
            }
            proposed
        });
        self.reads = settled.unwrap_or_default();
    }

    /// The current answer.
    #[must_use]
    pub const fn reads(&self) -> &Reads {
        &self.reads
    }

    /// Whether `agent` may be read under the current answer.
    #[must_use]
    pub fn allows(&self, agent: &str) -> bool {
        self.reads.allows(agent)
    }

    /// Records explicit user choices and updates this process only after the
    /// atomic save succeeds. Every agent id is validated before any write.
    ///
    /// # Errors
    ///
    /// Returns [`ConsentError`] when an id is invalid or the store cannot be
    /// read, locked, or written.
    pub fn set_reads(
        &mut self,
        all: Option<bool>,
        agents: impl IntoIterator<Item = (String, bool)>,
    ) -> Result<(), ConsentError> {
        let changes: Vec<(String, bool)> = agents.into_iter().collect();
        for (agent, _) in &changes {
            if !valid_agent_id(agent) {
                return Err(ConsentError::InvalidAgent(agent.clone()));
            }
        }
        if !self.settled {
            self.settle();
        }
        let mut updated = self.with_lock(|this| {
            let current = match read_stored(&this.path()) {
                Ok(Some(current)) => current,
                Ok(None) => this.reads.clone(),
                Err(error) => return Err(error),
            };
            let mut next = Reads {
                asked: true,
                ..current
            };
            if let Some(all) = all {
                next.all = all;
            }
            for (agent, on) in &changes {
                next.agents.insert(agent.clone(), *on);
            }
            atomic_write(&this.path(), &json_bytes(&next))?;
            Ok(next)
        })??;
        std::mem::swap(&mut self.reads, &mut updated);
        Ok(())
    }

    /// The consent JSON path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.config_dir.join(CONSENT_FILE)
    }

    fn index_path(&self) -> PathBuf {
        self.config_dir.join("state").join(INDEX_FILE)
    }

    fn with_lock<T>(&self, action: impl FnOnce(&Self) -> T) -> io::Result<T> {
        std::fs::create_dir_all(&self.config_dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(self.config_dir.join(LOCK_FILE))?;
        lock.lock()?;
        let result = action(self);
        let unlocked = File::unlock(&lock);
        unlocked.map(|()| result)
    }
}

/// Whether `agent` matches the session instance id grammar.
#[must_use]
pub fn valid_agent_id(agent: &str) -> bool {
    if let Some((base, distro)) = agent.split_once("@wsl:") {
        valid_part(base, false) && valid_part(distro, true)
    } else {
        !agent.contains('@') && valid_part(agent, false)
    }
}

fn valid_part(value: &str, dot: bool) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= 64
        && first.is_ascii_alphanumeric()
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-') || (dot && byte == b'.')
        })
}

fn read_stored(path: &Path) -> io::Result<Option<Reads>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid stored consent")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn json_bytes(reads: &Reads) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(reads).unwrap_or_else(|_| b"{\"asked\":false}".to_vec());
    bytes.push(b'\n');
    bytes
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::storage::atomic_write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_fresh_reads_nothing() {
        let root = tempfile::tempdir().expect("temp dir");
        let mut consent = Consent::new(root.path());
        consent.settle();
        assert!(!consent.reads().asked);
        assert!(!consent.allows("example"));
        assert_eq!(
            std::fs::read_to_string(consent.path()).expect("consent"),
            "{\"asked\":false}\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(consent.path())
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn consent_unwritable_reads_nothing() {
        let root = tempfile::tempdir().expect("temp dir");
        let config = root.path().join("not-a-directory");
        std::fs::write(&config, "file").expect("blocking file");
        let mut consent = Consent::new(&config);
        consent.settle();
        assert!(!consent.allows("example"));
        std::fs::remove_file(&config).expect("remove blocker");
        std::fs::create_dir_all(config.join("state")).expect("config");
        std::fs::write(config.join("state").join(INDEX_FILE), "{}").expect("index");
        consent.settle();
        assert!(!consent.reads().asked);
        assert!(!consent.path().exists(), "failed settle must not retry");
    }

    #[test]
    fn consent_index_after_settle_stays_off() {
        let root = tempfile::tempdir().expect("temp dir");
        let mut consent = Consent::new(root.path());
        consent.settle();
        std::fs::create_dir_all(root.path().join("state")).expect("state");
        std::fs::write(root.path().join("state").join(INDEX_FILE), "{}").expect("index");
        consent.settle();
        assert!(!consent.reads().asked);
        assert!(!consent.reads().all);
        assert!(!consent.allows("example"));
    }

    #[test]
    fn consent_existing_index_grants_upgrade_once() {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(root.path().join("state")).expect("state");
        std::fs::write(root.path().join("state").join(INDEX_FILE), "{}").expect("index");
        let mut consent = Consent::new(root.path());
        consent.settle();
        assert!(consent.reads().asked);
        assert!(consent.reads().all);
        assert!(consent.allows("example"));
    }

    #[test]
    fn per_agent_answer_wins_over_all() {
        let mut reads = Reads {
            asked: true,
            all: true,
            agents: BTreeMap::from([("one".to_owned(), false)]),
        };
        assert!(!reads.allows("one"));
        assert!(reads.allows("other"));
        reads.asked = false;
        assert!(!reads.allows("other"));
    }

    #[test]
    fn invalid_agent_change_writes_nothing() {
        let root = tempfile::tempdir().expect("temp dir");
        let mut consent = Consent::new(root.path());
        consent.settle();
        let before = std::fs::read(consent.path()).expect("before");
        let result = consent.set_reads(None, [("../bad".to_owned(), true)]);
        assert!(matches!(result, Err(ConsentError::InvalidAgent(_))));
        assert_eq!(std::fs::read(consent.path()).expect("after"), before);
    }
}
