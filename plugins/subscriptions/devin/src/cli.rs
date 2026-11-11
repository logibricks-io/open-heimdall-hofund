//! Isolated Devin CLI model and identity fallbacks.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hofund_plugin_api::{HostContext, PluginError, SpawnRequest};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{timeout, uuid};

use crate::creds::{Credential, Stored};
use crate::models::{self, Family};
use crate::sign_in::{Lookup, Who};

pub(crate) async fn families(host: &HostContext, stored: &Stored) -> Option<Vec<Family>> {
    let program = cli_path()?;
    families_with_program(host, stored, &program).await
}

pub(crate) async fn families_with_program(
    host: &HostContext,
    stored: &Stored,
    program: &Path,
) -> Option<Vec<Family>> {
    let args = ["models", "list", "--format", "json"].map(OsString::from);
    let output = run_with_program(host, &stored.credential, program, args.to_vec()).await?;
    let families = models::parse_families(&output.bytes).ok()?;
    (!families.is_empty()).then_some(families)
}

pub(crate) async fn identity(host: &HostContext, credential: &Credential) -> Lookup {
    let Some(program) = cli_path() else {
        return Lookup::Unknown;
    };
    identity_with_program(host, credential, &program).await
}

pub(crate) async fn identity_with_program(
    host: &HostContext,
    credential: &Credential,
    program: &Path,
) -> Lookup {
    let args = vec![OsString::from("auth"), OsString::from("status")];
    let Some(output) = run_with_program(host, credential, program, args).await else {
        return Lookup::Unknown;
    };
    let text = String::from_utf8_lossy(&output.bytes);
    if !reads_home(&text, &output.home) {
        return Lookup::Unknown;
    }
    if let Some(who) = parse_status(&text)
        && !who.user.is_empty()
    {
        return Lookup::Known(who);
    }
    if signed_out(&text) {
        return Lookup::Rejected;
    }
    Lookup::Unknown
}

struct CliOutput {
    bytes: Vec<u8>,
    home: PathBuf,
}

async fn run_with_program(
    host: &HostContext,
    credential: &Credential,
    program: &Path,
    args: Vec<OsString>,
) -> Option<CliOutput> {
    let temp = TempDir::new().ok()?;
    temp.write_credentials(credential).ok()?;
    let data = temp.path().as_os_str().to_owned();
    let request = SpawnRequest::new(
        program,
        args,
        vec![
            (OsString::from("XDG_DATA_HOME"), data.clone()),
            (OsString::from("APPDATA"), data),
        ],
        temp.path(),
    )
    .ok()?;
    let child = host.spawn_process().spawn_piped(&request).await.ok()?;
    let bytes = timeout(Duration::from_secs(30), async move {
        let mut stdout = child.stdout().ok()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stdout.next_chunk().await {
            bytes.extend_from_slice(&chunk.ok()?);
        }
        Some(bytes)
    })
    .await
    .flatten()?;
    Some(CliOutput {
        bytes,
        home: temp.path().to_path_buf(),
    })
}

fn status_field(output: &str, key: &str) -> String {
    for line in output.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix(&format!("{key}:")) {
            return value.trim().to_owned();
        }
    }
    String::new()
}

fn parse_status(output: &str) -> Option<Who> {
    if !output.contains("Logged in") {
        return None;
    }
    let team_id = status_field(output, "Team ID");
    Some(Who {
        user: {
            let email = status_field(output, "Email");
            if email.is_empty() {
                status_field(output, "Name")
            } else {
                email
            }
        },
        email: None,
        plan: {
            let tier = status_field(output, "Tier");
            if tier.is_empty() {
                status_field(output, "Plan")
            } else {
                tier
            }
        },
        org_id: None,
        team_id: (!team_id.is_empty()).then_some(team_id),
    })
}

fn signed_out(output: &str) -> bool {
    [
        "Not logged in",
        "Authentication required",
        "Invalid token",
        "try logging out and logging in again",
    ]
    .iter()
    .any(|text| output.contains(text))
}

fn reads_home(output: &str, home: &Path) -> bool {
    let file = status_field(output, "File");
    file.is_empty() || inside(Path::new(&file), home)
}

fn inside(file: &Path, directory: &Path) -> bool {
    let file = std::fs::canonicalize(file).unwrap_or_else(|_| normalized_absolute(file));
    let directory =
        std::fs::canonicalize(directory).unwrap_or_else(|_| normalized_absolute(directory));
    if cfg!(windows) {
        let file = PathBuf::from(file.to_string_lossy().to_lowercase());
        let directory = PathBuf::from(directory.to_string_lossy().to_lowercase());
        file.starts_with(&directory) && file != directory
    } else {
        file.starts_with(&directory) && file != directory
    }
}

fn normalized_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
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

    fn write_credentials(&self, credential: &Credential) -> Result<(), PluginError> {
        let directory = self.path.join("devin");
        private_dir(&directory)?;
        let body = format!(
            "windsurf_api_key = {}\napi_server_url = {}\ndevin_webapp_host = \"app.devin.ai\"\ndevin_api_url = {}\n",
            toml_string(secret_text(&credential.key)),
            toml_string(&credential.api_server),
            toml_string(&credential.devin_api),
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
