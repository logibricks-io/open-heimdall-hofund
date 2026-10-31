//! Credential material for the local gateway.
//!
//! Upstream keys are resolved from a config value. The gateway bearer token
//! lives in its own file. This crate does not open network connections.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

mod keys;
pub use keys::*;

/// A failure while resolving or storing a secret.
#[derive(Debug, Error)]
pub enum SecretError {
    /// An `env:` reference has no name, or the variable is unset.
    #[error("{0}")]
    Env(String),
    /// The token file could not be read or replaced.
    #[error("{0}")]
    File(String),
    /// A named gateway key change is invalid.
    #[error("{0}")]
    Key(String),
    /// The process could not draw random bytes.
    #[error("cannot generate a token: {0}")]
    Random(String),
}

/// Resolves a credential spec.
///
/// An empty string stays empty. `env:NAME` reads `NAME`. Any other string is
/// the credential itself.
///
/// # Errors
///
/// Returns [`SecretError::Env`] when the name is empty or the variable is unset.
pub fn resolve(spec: &str) -> Result<String, SecretError> {
    let Some(name) = spec.strip_prefix("env:") else {
        return Ok(spec.to_owned());
    };
    if name.is_empty() {
        return Err(SecretError::Env(
            "env: reference is missing a variable name".to_owned(),
        ));
    }
    std::env::var(name)
        .map_err(|_| SecretError::Env(format!("environment variable `{name}` is unset")))
}

/// The file that holds the single gateway bearer token.
#[derive(Clone, Debug)]
pub struct TokenFile {
    path: PathBuf,
}

impl TokenFile {
    /// A token file at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The path this file was constructed with.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the token, creating it when the file is absent.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the file cannot be read or created.
    pub fn load_or_create(&self) -> Result<String, SecretError> {
        match self.read() {
            Ok(token) => Ok(token),
            Err(SecretError::File(message)) if message.contains("not found") => self.rotate(),
            Err(error) => Err(error),
        }
    }

    /// Reads an existing token.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the file is missing, empty, or unreadable.
    pub fn read(&self) -> Result<String, SecretError> {
        let text = fs::read_to_string(&self.path).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                SecretError::File(format!("token file `{}` not found", self.path.display()))
            } else {
                SecretError::File(error.to_string())
            }
        })?;
        let token = text.trim().to_owned();
        if token.is_empty() {
            return Err(SecretError::File(format!(
                "token file `{}` is empty",
                self.path.display()
            )));
        }
        Ok(token)
    }

    /// Replaces the token by writing a temporary file and renaming it.
    ///
    /// On Unix the file mode is `0600`.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] when the random source or the filesystem fails.
    pub fn rotate(&self) -> Result<String, SecretError> {
        let token = generate()?;
        write_atomic(&self.path, &token)?;
        Ok(token)
    }
}

fn generate() -> Result<String, SecretError> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|error| SecretError::Random(error.to_string()))?;
    let mut token = String::from("hofund_");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

fn write_atomic(path: &Path, token: &str) -> Result<(), SecretError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| SecretError::File(error.to_string()))?;
    }
    let mut temporary_name = path.file_name().map_or_else(
        || std::ffi::OsString::from("hofund.token"),
        ToOwned::to_owned,
    );
    temporary_name.push(".tmp");
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let temporary = parent.unwrap_or(Path::new(".")).join(temporary_name);
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| SecretError::File(error.to_string()))?;
        file.write_all(token.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| SecretError::File(error.to_string()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&temporary)
            .map_err(|error| SecretError::File(error.to_string()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&temporary, permissions)
            .map_err(|error| SecretError::File(error.to_string()))?;
    }
    fs::rename(&temporary, path).map_err(|error| SecretError::File(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{TokenFile, resolve};

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn env_and_literal_specs_resolve() {
        let path = std::env::var("PATH").expect("PATH");
        assert_eq!(resolve("env:PATH").expect("env"), path);
        assert_eq!(resolve("literal-key").expect("literal"), "literal-key");
        assert!(resolve("env:").is_err());
        assert!(resolve("env:HOFUND_SECRET_DEFINITELY_UNSET").is_err());
    }

    #[test]
    fn rotate_replaces_the_file_atomically() {
        let dir = scratch();
        let file = TokenFile::new(dir.path().join("hofund.token"));
        let first = file.load_or_create().expect("create");
        assert!(first.starts_with("hofund_"));
        let second = file.rotate().expect("rotate");
        assert_ne!(first, second);
        assert_eq!(file.read().expect("read"), second);
        assert!(!dir.path().join("hofund.token.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(file.path())
                .expect("meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
