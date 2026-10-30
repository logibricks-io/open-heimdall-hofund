//! Work the CLI does without the control plane: token show, the structural
//! config check fallback, and the offline status report.

use std::path::{Path, PathBuf};

use hofund_config::{ListenSource, addr_env, effective_listen, load_path};

/// The exact label for a config check that could not run `hofundd`.
pub(crate) const STRUCTURAL_ONLY: &str = "structural check only; hofundd is unavailable, so provider construction and preflight did not run";

/// `value` resolved against the directory of `config_path` when relative.
pub(crate) fn beside(config_path: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

/// Prints the bearer token, creating the token file on first use.
///
/// # Errors
///
/// Returns a message when the config or the token file fails.
pub(crate) fn token_show(config: &Path) -> Result<String, String> {
    let loaded = load_path(config).map_err(|error| format!("{}: {error}", config.display()))?;
    hofund_secrets::TokenFile::new(beside(config, &loaded.token_file))
        .load_or_create()
        .map_err(|error| error.to_string())
}

/// The structural check `config check` falls back to without `hofundd`.
///
/// # Errors
///
/// Returns the parse error prefixed with the config path.
pub(crate) fn structural_check(config: &Path) -> Result<String, String> {
    load_path(config)
        .map(|_| format!("config ok: {}\n{STRUCTURAL_ONLY}", config.display()))
        .map_err(|error| format!("{}: {error}\n{STRUCTURAL_ONLY}", config.display()))
}

/// The model address the daemon would bind and where it came from.
///
/// # Errors
///
/// Returns a message when the config does not load or `HOFUND_ADDR` is not
/// a socket address.
pub(crate) fn model_listen(
    config: &Path,
    env: Option<&str>,
) -> Result<(std::net::SocketAddr, ListenSource), String> {
    let loaded = load_path(config).map_err(|error| format!("{}: {error}", config.display()))?;
    effective_listen(&loaded, env).map_err(|error| error.to_string())
}

/// The `HOFUND_ADDR` value in this process.
pub(crate) fn listen_env() -> Option<String> {
    addr_env()
}

/// The source label the status report prints.
pub(crate) fn source_label(source: ListenSource) -> &'static str {
    match source {
        ListenSource::Environment => "environment",
        ListenSource::Config => "config",
    }
}

#[cfg(test)]
mod tests {
    use super::{STRUCTURAL_ONLY, beside, source_label, structural_check};
    use hofund_config::ListenSource;
    use std::path::Path;

    #[test]
    fn relative_values_resolve_beside_the_config() {
        assert_eq!(
            beside(Path::new("/etc/hofund/hofund.toml"), "token"),
            Path::new("/etc/hofund/token")
        );
        let absolute = Path::new(env!("CARGO_MANIFEST_DIR")).join("token");
        assert_eq!(
            beside(
                Path::new("/etc/hofund/hofund.toml"),
                absolute.to_str().expect("utf-8")
            ),
            absolute
        );
        assert_eq!(
            beside(Path::new("hofund.toml"), "token"),
            Path::new("token")
        );
    }

    #[test]
    fn structural_check_always_carries_the_label() {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good.toml");
        std::fs::write(&good, "listen = \"127.0.0.1:0\"\ntoken_file = \"token\"\n").expect("write");
        let ok = structural_check(&good).expect("structural ok");
        assert!(ok.starts_with("config ok: "), "{ok}");
        assert!(ok.ends_with(STRUCTURAL_ONLY), "{ok}");
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "listen = 3\n").expect("write");
        let error = structural_check(&bad).expect_err("bad config");
        assert!(error.ends_with(STRUCTURAL_ONLY), "{error}");
    }

    #[test]
    fn sources_have_stable_labels() {
        assert_eq!(source_label(ListenSource::Environment), "environment");
        assert_eq!(source_label(ListenSource::Config), "config");
    }
}
