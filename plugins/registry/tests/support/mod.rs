use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_agent::{ConnectionStatus, DisconnectOutcome};

pub(crate) fn normalized_error(home: &Path, error: &hofund_agent::AgentError) -> String {
    error
        .to_string()
        .replace(home.to_string_lossy().as_ref(), "<home>")
}

pub(crate) fn normalized_status(
    home: &Path,
    status: &ConnectionStatus,
) -> (bool, bool, Vec<PathBuf>) {
    (
        status.installed(),
        status.connected(),
        relative_paths(home, status.paths()),
    )
}

pub(crate) fn normalized_outcome(home: &Path, outcome: &DisconnectOutcome) -> (bool, Vec<String>) {
    (
        outcome.changed(),
        outcome
            .warnings()
            .iter()
            .map(|warning| warning.replace(&home.to_string_lossy().to_string(), "<home>"))
            .collect(),
    )
}

pub(crate) fn relative_paths(home: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| path.strip_prefix(home).expect("relative").to_path_buf())
        .collect()
}

pub(crate) fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    collect_files(root, root, &mut files);
    files
}

fn collect_files(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.expect("tree entry").path();
        if path.is_dir() {
            let mut relative = path
                .strip_prefix(root)
                .expect("relative")
                .as_os_str()
                .to_os_string();
            relative.push("/");
            files.insert(PathBuf::from(relative), Vec::new());
            collect_files(root, &path, files);
        } else if path.is_file() {
            files.insert(
                path.strip_prefix(root).expect("relative").to_path_buf(),
                fs::read(path).expect("file"),
            );
        }
    }
}

pub(crate) fn copy_tree(source: &Path, destination: &Path) {
    for (relative, bytes) in tree(source) {
        let path = destination.join(&relative);
        if relative.as_os_str().to_string_lossy().ends_with('/') {
            fs::create_dir_all(path).expect("copy directory");
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("copy parent");
        }
        fs::write(path, bytes).expect("copy file");
    }
}

pub(crate) fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "hofund-agent-equivalence-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("scratch");
    path
}
