//! Isolated Devin CLI model and identity fallbacks.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hofund_plugin_api::{HostContext, PipedChild, PluginError, SpawnRequest};
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{race, sleep, timeout, uuid};

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

const INSTALL_SH: &str = "curl -fsSL https://cli.devin.ai/install.sh | bash";
const INSTALL_PS: &str = "irm https://static.devin.ai/cli/setup.ps1 | iex";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const INSTALL_TAIL: Duration = Duration::from_secs(5);

#[cfg(not(test))]
pub(crate) async fn ensure_installed(host: &HostContext) -> Result<(), PluginError> {
    install_with_finder(host, cfg!(windows), cli_path).await
}

async fn install_with_finder(
    host: &HostContext,
    windows: bool,
    find: impl Fn() -> Option<PathBuf>,
) -> Result<(), PluginError> {
    install_with_program(host, windows, find, None).await
}

async fn install_with_program(
    host: &HostContext,
    windows: bool,
    find: impl Fn() -> Option<PathBuf>,
    program: Option<PathBuf>,
) -> Result<(), PluginError> {
    if find().is_some() {
        return Ok(());
    }
    let output = match timeout(
        INSTALL_TIMEOUT,
        run_installer(host, windows, &find, program),
    )
    .await
    {
        Some(Ok(output)) => output,
        Some(Err(error)) => return Err(install_failed(error.to_string())),
        None => return Err(install_failed("the installer timed out")),
    };
    refresh_path(host).await;
    if find().is_some() {
        return Ok(());
    }
    let detail = if output.last.is_empty() {
        "the installer finished without a discoverable CLI".to_owned()
    } else {
        output.last
    };
    Err(install_failed(detail))
}

struct InstallOutput {
    last: String,
}

fn install_failed(detail: impl std::fmt::Display) -> PluginError {
    let manual = if cfg!(windows) {
        INSTALL_PS
    } else {
        INSTALL_SH
    };
    PluginError::io(format!(
        "installing Devin CLI failed ({detail}); install it yourself with `{manual}`, then sign in again"
    ))
}

async fn run_installer(
    host: &HostContext,
    windows: bool,
    find: &impl Fn() -> Option<PathBuf>,
    program: Option<PathBuf>,
) -> Result<InstallOutput, PluginError> {
    let child = spawn_installer(host, windows, program).await?;
    let last = Arc::new(Mutex::new(String::new()));
    if !windows {
        read_last(&child, &last).await?;
        return Ok(InstallOutput {
            last: take_string(&last),
        });
    }
    let running = read_last(&child, &last);
    let tail = async {
        loop {
            refresh_path(host).await;
            if find().is_some() {
                sleep(INSTALL_TAIL).await;
                return;
            }
            sleep(Duration::from_secs(1)).await;
        }
    };
    let _ = race(running, tail).await;
    drop(child);
    Ok(InstallOutput {
        last: take_string(&last),
    })
}

async fn spawn_installer(
    host: &HostContext,
    windows: bool,
    program: Option<PathBuf>,
) -> Result<PipedChild, PluginError> {
    let (program, args) = if windows {
        (
            program
                .or_else(|| find_program("powershell"))
                .ok_or_else(|| PluginError::io("PowerShell is not installed"))?,
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-Command"),
                OsString::from("-"),
            ],
        )
    } else {
        (
            find_program("bash").ok_or_else(|| PluginError::io("bash is not installed"))?,
            vec![OsString::from("-c"), OsString::from(INSTALL_SH)],
        )
    };
    let cwd = install_cwd();
    let request = SpawnRequest::new(program, args, Vec::new(), cwd)?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    if windows {
        child
            .write_stdin(format!("{INSTALL_PS}\n").into_bytes().into())
            .await?;
    }
    child.close_stdin().await?;
    Ok(child)
}

fn install_cwd() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::current_dir().ok())
        .expect("the current directory must be absolute")
}

async fn read_last(child: &PipedChild, last: &Arc<Mutex<String>>) -> Result<i32, PluginError> {
    let mut stdout = child.stdout()?;
    while let Some(chunk) = stdout.next_chunk().await {
        let chunk = chunk?;
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            let line = without_ansi(line);
            if !line.trim().is_empty() {
                *last
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = line;
            }
        }
    }
    child.exit_code().await
}

fn take_string(value: &Arc<Mutex<String>>) -> String {
    std::mem::take(
        &mut *value
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn without_ansi(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        if chars.next() == Some('[') {
            for character in chars.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push('\u{1b}');
        }
    }
    out.trim().to_owned()
}

async fn refresh_path(host: &HostContext) {
    if !cfg!(windows) {
        return;
    }
    let Some(program) = find_program("powershell") else {
        return;
    };
    const QUERY: &str = "$m=[Environment]::GetEnvironmentVariable('Path','Machine');$u=[Environment]::GetEnvironmentVariable('Path','User');if($m){[Environment]::ExpandEnvironmentVariables($m)};if($u){[Environment]::ExpandEnvironmentVariables($u)}";
    let Ok(request) = SpawnRequest::new(
        program,
        vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-Command"),
            OsString::from(QUERY),
        ],
        Vec::new(),
        install_cwd(),
    ) else {
        return;
    };
    let Ok(child) = host.spawn_process().spawn_piped(&request).await else {
        return;
    };
    let Ok(mut stdout) = child.stdout() else {
        return;
    };
    let mut refreshed = Vec::new();
    while let Some(chunk) = stdout.next_chunk().await {
        let Ok(chunk) = chunk else {
            return;
        };
        refreshed.extend(String::from_utf8_lossy(&chunk).lines().map(str::to_owned));
    }
    let _ = child.close_stdin().await;
    let _ = child.exit_code().await;
    let mut directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    for directory in refreshed {
        let directory = directory.trim();
        if directory.is_empty()
            || directories.iter().any(|existing| {
                existing
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(directory)
            })
        {
            continue;
        }
        directories.push(PathBuf::from(directory));
    }
    *refreshed_directories()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = directories;
}

fn refreshed_directories() -> &'static Mutex<Vec<PathBuf>> {
    static DIRECTORIES: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    DIRECTORIES.get_or_init(|| Mutex::new(Vec::new()))
}

fn find_program(name: &str) -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(&executable))
            .find_map(|candidate| absolute_file(&candidate))
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
    let mut directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    directories.extend(
        refreshed_directories()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned(),
    );
    let home =
        std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    cli_path_in(
        &directories,
        home.as_deref(),
        &[
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
        ],
    )
}

fn cli_path_in(
    directories: &[PathBuf],
    home: Option<&Path>,
    system: &[PathBuf],
) -> Option<PathBuf> {
    let executable = if cfg!(windows) { "devin.exe" } else { "devin" };
    for directory in directories {
        if let Some(program) = absolute_file(&directory.join(executable)) {
            return Some(program);
        }
    }
    if let Some(home) = home {
        let candidate = home.join(".local").join("bin").join("devin");
        if let Some(program) = absolute_file(&candidate) {
            return Some(program);
        }
    }
    for directory in system {
        if let Some(program) = absolute_file(&directory.join("devin")) {
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

#[cfg(test)]
pub(crate) async fn ensure_installed(_host: &HostContext) -> Result<(), PluginError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hofund_kit_subscription::{
        PipedScript, TestHost, advance_time, block_on, block_on_paused, yield_times,
    };
    use hofund_plugin_api::{PluginId, PluginKind};
    use hofund_plugin_sdk::{Race, race};

    use super::{INSTALL_PS, INSTALL_SH, cli_path_in, install_with_finder, install_with_program};

    fn host(dir: &tempfile::TempDir) -> TestHost {
        TestHost::open_for(
            PluginKind::Subscription,
            PluginId::from_static("devin"),
            dir.path(),
        )
    }

    #[test]
    fn executable_discovery_prefers_path_then_home_then_system_paths() {
        let dir = tempfile::tempdir().expect("dir");
        let path_bin = dir.path().join("path-bin");
        let home = dir.path().join("home");
        let system = dir.path().join("system");
        let path_program = path_bin.join("devin");
        let home_program = home.join(".local").join("bin").join("devin");
        let system_program = system.join("devin");
        for program in [&path_program, &home_program, &system_program] {
            std::fs::create_dir_all(program.parent().expect("parent")).expect("dir");
            std::fs::write(program, b"cli").expect("cli");
        }
        let directories = vec![dir.path().join("empty"), path_bin.clone()];
        let found =
            cli_path_in(&directories, Some(&home), std::slice::from_ref(&system)).expect("PATH");
        assert_eq!(found, path_program.canonicalize().expect("canonical"));
        std::fs::remove_file(&path_program).expect("remove PATH");
        let found =
            cli_path_in(&directories, Some(&home), std::slice::from_ref(&system)).expect("home");
        assert_eq!(found, home_program.canonicalize().expect("canonical"));
        std::fs::remove_file(&home_program).expect("remove home");
        let found =
            cli_path_in(&directories, Some(&home), std::slice::from_ref(&system)).expect("system");
        assert_eq!(found, system_program.canonicalize().expect("canonical"));
        std::fs::remove_file(&system_program).expect("remove system");
        assert_eq!(cli_path_in(&directories, Some(&home), &[system]), None);
    }

    #[test]
    fn installer_uses_reference_command_and_accepts_cli_after_setup_failure() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        let program = dir.path().join("devin");
        let scripted = program.clone();
        host.process().script_piped(
            PipedScript::new()
                .line("installed")
                .exits(move |spawn| {
                    assert!(spawn.program.is_absolute());
                    assert_eq!(
                        spawn.program.file_name().and_then(|name| name.to_str()),
                        Some("bash")
                    );
                    assert_eq!(spawn.args, ["-c", INSTALL_SH]);
                    std::fs::write(&scripted, b"cli").expect("installed CLI");
                    Vec::new()
                })
                .status(1),
        );
        let find = || {
            program
                .is_file()
                .then(|| program.canonicalize().ok())
                .flatten()
        };
        block_on(install_with_finder(host.context(), false, find)).expect("install");
        assert_eq!(host.process().piped_spawns().len(), 1);
    }

    #[test]
    fn installer_failure_includes_last_output_and_manual_command() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        host.process().script_piped(
            PipedScript::new()
                .line("downloading")
                .line("\u{1b}[31mError: Failed to download\u{1b}[0m")
                .exits(|_| Vec::new())
                .status(1),
        );
        let error = block_on(install_with_finder(host.context(), false, || None))
            .expect_err("no installed CLI");
        let text = error.to_string();
        assert!(text.contains("Error: Failed to download"), "{text}");
        assert!(text.contains(INSTALL_SH), "{text}");
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn windows_installer_uses_stdin_and_stops_setup_after_discovery_tail() {
        let dir = tempfile::tempdir().expect("dir");
        let host = host(&dir);
        host.process().script_piped(PipedScript::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let program = PathBuf::from("/opt/homebrew/bin/devin");
        let find = {
            let calls = Arc::clone(&calls);
            let program = program.clone();
            move || (calls.fetch_add(1, Ordering::SeqCst) > 0).then_some(program.clone())
        };
        block_on_paused(async {
            let install = install_with_program(
                host.context(),
                true,
                find,
                Some(dir.path().join("powershell")),
            );
            let control = async {
                advance_time(std::time::Duration::from_secs(1)).await;
                advance_time(std::time::Duration::from_secs(5)).await;
                advance_time(std::time::Duration::from_millis(1)).await;
                yield_times(4).await;
            };
            let result = race(install, control).await;
            let Race::Left(result) = result else {
                panic!("installer did not finish after the discovery tail")
            };
            result.expect("install");
        });
        let spawns = host.process().piped_spawns();
        assert_eq!(spawns.len(), 1);
        assert_eq!(
            spawns[0].args,
            ["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", "-"]
        );
        assert_eq!(
            host.process().stdin_writes(),
            [format!("{INSTALL_PS}\n").into_bytes()]
        );
        assert_eq!(host.process().piped_dropped(), 1);
    }
}
