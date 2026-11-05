//! Restores agent config files when a multi-step sidecar edit fails partway through.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::AgentError;
use crate::store::{delete_file, read_optional, write_atomic};

/// Bytes that belonged to a path before a sidecar edit touched it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarCheckpoint {
    prior: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl SidecarCheckpoint {
    /// Creates an empty checkpoint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prior: BTreeMap::new(),
        }
    }

    /// Records the current contents of `path` the first time it is seen.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when the file cannot be read.
    pub fn record(&mut self, path: &Path) -> Result<(), AgentError> {
        if self.prior.contains_key(path) {
            return Ok(());
        }
        self.prior.insert(path.to_path_buf(), read_optional(path)?);
        Ok(())
    }

    /// Puts every recorded path back to its captured state.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] when a path cannot be restored.
    pub fn restore(self) -> Result<(), AgentError> {
        for (path, prior) in self.prior {
            match prior {
                None => {
                    if path.exists() {
                        delete_file(&path)?;
                    }
                }
                Some(bytes) => write_atomic(&path, &bytes)?,
            }
        }
        Ok(())
    }
}

impl Default for SidecarCheckpoint {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::SidecarCheckpoint;

    #[test]
    fn sidecar_checkpoint_restores_prior_bytes_and_absence() {
        let dir = tempfile::tempdir().expect("dir");
        let kept = dir.path().join("kept.json");
        let created = dir.path().join("created.json");
        std::fs::write(&kept, b"before").expect("kept");
        let mut checkpoint = SidecarCheckpoint::new();
        checkpoint.record(&kept).expect("record kept");
        checkpoint.record(&created).expect("record absence");
        std::fs::write(&kept, b"after").expect("mutate kept");
        std::fs::write(&created, b"new").expect("create during edit");
        checkpoint.restore().expect("restore");
        assert_eq!(std::fs::read(&kept).expect("kept bytes"), b"before");
        assert!(!created.exists());
    }
}
