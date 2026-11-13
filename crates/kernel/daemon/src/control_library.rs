use std::path::Path;

use hofund_control_proto::LibraryResponse;

pub(crate) fn read(config_path: &Path) -> LibraryResponse {
    let config_dir = config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let library_file = config_dir.join("library.json");
    let error = std::fs::read(&library_file)
        .err()
        .filter(|error| error.kind() != std::io::ErrorKind::NotFound)
        .map(|error| format!("could not read {}: {error}", library_file.display()));
    let mut response = LibraryResponse::default();
    response.dir = config_dir.join("library").to_string_lossy().into_owned();
    response.error = error;
    response
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_store_has_a_real_directory_and_no_rows() {
        let dir = tempfile::tempdir().expect("temp dir");
        let response = super::read(&dir.path().join("hofund.toml"));
        assert_eq!(
            response.dir,
            dir.path().join("library").to_string_lossy().as_ref()
        );
        assert!(response.rows.is_empty());
        assert!(response.agents.is_empty());
        assert_eq!(response.error, None);
    }

    #[test]
    fn unreadable_library_file_is_reported_in_the_view() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir(dir.path().join("library.json")).expect("unreadable fixture");
        let response = super::read(&dir.path().join("hofund.toml"));
        assert!(response.error.is_some());
        assert!(response.rows.is_empty());
    }
}
