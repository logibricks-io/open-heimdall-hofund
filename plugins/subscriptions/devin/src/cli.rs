//! Isolated `devin models list` fallback.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_plugin_api::{HostContext, PluginError, SpawnRequest};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{timeout, uuid};

use crate::creds::Stored;
use crate::models::{self, Family};

pub(crate) async fn families(host: &HostContext, stored: &Stored) -> Option<Vec<Family>> {
    let program = cli_path()?;
    families_with_program(host, stored, &program).await
}

pub(crate) async fn families_with_program(
    host: &HostContext,
    stored: &Stored,
    program: &Path,
) -> Option<Vec<Family>> {
    let temp = TempDir::new().ok()?;
    temp.write_credentials(stored).ok()?;
    let data = temp.path().as_os_str().to_owned();
    let request = SpawnRequest::new(
        program,
        vec![
            OsString::from("models"),
            OsString::from("list"),
            OsString::from("--format"),
            OsString::from("json"),
        ],
        vec![
            (OsString::from("XDG_DATA_HOME"), data.clone()),
            (OsString::from("APPDATA"), data),
        ],
        temp.path(),
    )
    .ok()?;
    let child = host.spawn_process().spawn_piped(&request).await.ok()?;
    let output = timeout(Duration::from_secs(30), async move {
        let mut stdout = child.stdout().ok()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stdout.next_chunk().await {
            bytes.extend_from_slice(&chunk.ok()?);
        }
        Some(bytes)
    })
    .await
    .flatten()?;
    let families = models::parse_families(&output).ok()?;
    (!families.is_empty()).then_some(families)
}

fn cli_path() -> Option<PathBuf> {
    let executable = if cfg!(windows) { "devin.exe" } else { "devin" };
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            if let Some(program) = absolute_file(&directory.join(executable)) {
                return Some(program);
            }
        }
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    if let Some(home) = home {
        let candidate = PathBuf::from(home).join(".local").join("bin").join("devin");
        if let Some(program) = absolute_file(&candidate) {
            return Some(program);
        }
    }
    for candidate in ["/usr/local/bin/devin", "/opt/homebrew/bin/devin"] {
        if let Some(program) = absolute_file(Path::new(candidate)) {
            return Some(program);
        }
    }
    None
}

fn absolute_file(path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    std::fs::canonicalize(path).ok()
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Result<Self, PluginError> {
        let id = uuid::v4().map_err(|error| PluginError::io(error.to_string()))?;
        let path = std::env::temp_dir().join(format!("devin-models-{id}"));
        std::fs::create_dir(&path)
            .map_err(|_| PluginError::io("cannot create the Devin CLI temporary directory"))?;
        let temp = Self { path };
        private_dir(&temp.path)?;
        Ok(temp)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write_credentials(&self, stored: &Stored) -> Result<(), PluginError> {
        let directory = self.path.join("devin");
        private_dir(&directory)?;
        let body = format!(
            "windsurf_api_key = {}\napi_server_url = {}\ndevin_webapp_host = \"app.devin.ai\"\ndevin_api_url = {}\n",
            toml_string(secret_text(&stored.credential.key)),
            toml_string(&stored.credential.api_server),
            toml_string(&stored.credential.devin_api),
        );
        let path = directory.join("credentials.toml");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|_| PluginError::io("cannot write the Devin CLI credentials"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| PluginError::io("cannot restrict the Devin CLI credentials"))?;
        }
        std::io::Write::write_all(&mut file, body.as_bytes())
            .map_err(|_| PluginError::io("cannot write the Devin CLI credentials"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn private_dir(path: &Path) -> Result<(), PluginError> {
    std::fs::create_dir_all(path)
        .map_err(|_| PluginError::io("cannot create the Devin CLI data directory"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| PluginError::io("cannot restrict the Devin CLI data directory"))?;
    }
    Ok(())
}

fn toml_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}
