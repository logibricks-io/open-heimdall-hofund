//! Hidden gateway sessions after a user delete.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One deleted gateway session stamp.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct HiddenGateway {
    agent: String,
    id: String,
    /// Unix seconds of the last request before deletion.
    last_unix: i64,
}

/// Loads deleted gateway sessions from `<config>/state/gateway-sessions-hidden.json`.
#[must_use]
pub fn load_gateway_hidden(config_dir: &Path) -> BTreeMap<String, i64> {
    let path = hidden_path(config_dir);
    let bytes = std::fs::read(path).ok();
    let Some(bytes) = bytes else {
        return BTreeMap::new();
    };
    let Ok(entries) = serde_json::from_slice::<Vec<HiddenGateway>>(&bytes) else {
        return BTreeMap::new();
    };
    entries
        .into_iter()
        .map(|entry| (format!("{}|{}", entry.agent, entry.id), entry.last_unix))
        .collect()
}

/// Whether a gateway session stays hidden after delete.
#[must_use]
pub fn gateway_hidden(
    hidden: &BTreeMap<String, i64>,
    agent: &str,
    id: &str,
    last_unix: i64,
) -> bool {
    hidden
        .get(&format!("{agent}|{id}"))
        .is_some_and(|stamp| last_unix <= *stamp)
}

fn hidden_path(config_dir: &Path) -> PathBuf {
    config_dir
        .join("state")
        .join("gateway-sessions-hidden.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r33_gateway_hidden_hides_until_newer_row() {
        let hidden = BTreeMap::from([("fixture|s1".to_owned(), 100)]);
        assert!(gateway_hidden(&hidden, "fixture", "s1", 100));
        assert!(gateway_hidden(&hidden, "fixture", "s1", 50));
        assert!(!gateway_hidden(&hidden, "fixture", "s1", 101));
    }

    #[test]
    fn r33_load_gateway_hidden_reads_stamps_from_the_state_file() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(load_gateway_hidden(dir.path()).is_empty());
        std::fs::create_dir_all(dir.path().join("state")).expect("state");
        std::fs::write(
            hidden_path(dir.path()),
            br#"[{"agent":"fixture","id":"s1","last_unix":100}]"#,
        )
        .expect("write");
        let hidden = load_gateway_hidden(dir.path());
        assert_eq!(hidden.get("fixture|s1").copied(), Some(100));
        assert!(gateway_hidden(&hidden, "fixture", "s1", 100));
        assert!(!gateway_hidden(&hidden, "fixture", "s1", 101));
    }
}
