//! Native Qoder device identity and the client headers that carry it.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{HostContext, PluginError, SecretString, SpawnRequest};
use hofund_plugin_sdk::json::Value;
use hofund_plugin_sdk::oauth::secret_text;
use hofund_plugin_sdk::{Call, digest};

use crate::creds::{Credential, Site};

const IDENTITY_TTL: Duration = Duration::from_secs(60 * 60);
const PROCESS_WAIT: Duration = Duration::from_secs(20);
const COSY_VERSION: &str = "1.1.69";

#[derive(Clone)]
pub(crate) struct NativeIdentity {
    token: SecretString,
    machine_type: String,
}

struct CachedIdentity {
    read_at: SystemTime,
    identity: NativeIdentity,
}

pub(crate) struct Devices {
    env: RuntimeEnv,
    identities: Mutex<HashMap<(Site, String), CachedIdentity>>,
    hostname: OnceLock<String>,
}

impl Devices {
    pub(crate) fn new() -> Self {
        Self::with_env(RuntimeEnv::from_process())
    }

    fn with_env(env: RuntimeEnv) -> Self {
        Self {
            env,
            identities: Mutex::new(HashMap::new()),
            hostname: OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(env: TestEnv, hostname: &str) -> Self {
        Self::with_env(env.into_runtime(hostname))
    }

    pub(crate) async fn chat_headers(
        &self,
        host: &HostContext,
        credential: &Credential,
    ) -> Vec<(String, String)> {
        let mut headers = common_headers(credential, false);
        if let Some(identity) = self.native(host, credential).await.ok().flatten() {
            headers.extend(identity.headers());
        }
        let hostname = self.hostname(host).await;
        if !hostname.is_empty() {
            headers.push(("Cosy-MachineHostname".to_owned(), hostname));
        }
        headers
    }

    pub(crate) async fn checkin_identity(
        &self,
        host: &HostContext,
        credential: &Credential,
    ) -> Result<Option<NativeIdentity>, PluginError> {
        self.native(host, credential).await
    }

    async fn native(
        &self,
        host: &HostContext,
        credential: &Credential,
    ) -> Result<Option<NativeIdentity>, PluginError> {
        if credential.uid.is_empty() {
            return Err(PluginError::auth(
                "Qoder device identity needs the account uid; import credentials again",
            ));
        }
        let now = host.clock().now();
        let key = (credential.site, credential.uid.clone());
        {
            let cache = self
                .identities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = cache.get(&key)
                && now.duration_since(cached.read_at).unwrap_or_default() < IDENTITY_TTL
            {
                return Ok(Some(cached.identity.clone()));
            }
        }
        let identity = match read_native(host, &self.env, credential).await {
            Ok(identity) => identity,
            Err(_) if credential.site == Site::China => return Ok(None),
            Err(error) => return Err(error),
        };
        self.identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                key,
                CachedIdentity {
                    read_at: now,
                    identity: identity.clone(),
                },
            );
        Ok(Some(identity))
    }

    async fn hostname(&self, host: &HostContext) -> String {
        if let Some(hostname) = self.hostname.get() {
            return hostname.clone();
        }
        let hostname = machine_hostname(&read_hostname(host, &self.env).await);
        let _ = self.hostname.set(hostname.clone());
        hostname
    }
}

impl NativeIdentity {
    fn headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "Cosy-MachineToken".to_owned(),
                secret_text(&self.token).to_owned(),
            ),
            ("Cosy-MachineType".to_owned(), self.machine_type.clone()),
        ]
    }
}

pub(crate) fn apply_checkin(mut call: Call, identity: Option<&NativeIdentity>) -> Call {
    call = call
        .header("Cosy-ClientType", "10")
        .header("User-Agent", "Qoder");
    if let Some(identity) = identity {
        for (name, value) in identity.headers() {
            call = if name == "Cosy-MachineToken" {
                call.sensitive_header(name, value)
            } else {
                call.header(name, value)
            };
        }
    }
    call
}

pub(crate) fn apply_quota(call: Call) -> Call {
    call.header("Cosy-ClientType", "10")
        .header("User-Agent", "Qoder")
}

pub(crate) fn model_headers(credential: &Credential) -> Vec<(String, String)> {
    let mut headers = common_headers(credential, true);
    if !credential.machine_id.is_empty() {
        headers.push((
            "Cosy-MachineToken".to_owned(),
            credential.machine_id.clone(),
        ));
        headers.push(("Cosy-MachineType".to_owned(), "5".to_owned()));
    }
    headers
}

fn common_headers(credential: &Credential, client_ip: bool) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Accept-Encoding".to_owned(), "identity".to_owned()),
        ("Cosy-Business-Product".to_owned(), "app".to_owned()),
        ("Cosy-Business-Type".to_owned(), "agent".to_owned()),
        ("Cosy-ClientType".to_owned(), "10".to_owned()),
        ("Cosy-Data-Policy".to_owned(), "disagree".to_owned()),
        (
            "Cosy-MachineOS".to_owned(),
            machine_os(
                node_arch(std::env::consts::ARCH),
                node_os(std::env::consts::OS),
            ),
        ),
        ("Cosy-Scene".to_owned(), "app".to_owned()),
        ("Cosy-User".to_owned(), credential.uid.clone()),
        ("Cosy-Version".to_owned(), COSY_VERSION.to_owned()),
        ("Login-Version".to_owned(), "v2".to_owned()),
    ];
    if !credential.machine_id.is_empty() {
        if client_ip {
            headers.push(("Cosy-ClientIp".to_owned(), credential.machine_id.clone()));
        }
        headers.push(("Cosy-MachineId".to_owned(), credential.machine_id.clone()));
    }
    headers
}

async fn read_native(
    host: &HostContext,
    env: &RuntimeEnv,
    credential: &Credential,
) -> Result<NativeIdentity, PluginError> {
    let runtime = runtime_spec(credential.site);
    let program = runtime_path(env, runtime).ok_or_else(|| {
        PluginError::config(format!(
            "{} check-in: device identity runtime not found; install the desktop app or CLI, or set {}",
            runtime.name, runtime.override_var
        ))
    })?;
    let mut args = vec![OsString::from(runtime.environment)];
    if env.platform != "linux" {
        args.push(OsString::from("--account-stdin"));
    }
    let cwd = program
        .parent()
        .filter(|path| path.is_absolute())
        .unwrap_or(&env.cwd)
        .to_path_buf();
    let request = SpawnRequest::new(program, args, Vec::new(), cwd)?;
    let child = host.spawn_process().spawn_piped(&request).await?;
    let platform = env.platform.clone();
    let uid = credential.uid.clone();
    let output = hofund_plugin_sdk::timeout(PROCESS_WAIT, async move {
        if platform != "linux" {
            let input = format!("{}\n", hofund_plugin_sdk::json::json!({"account": uid}));
            child.write_stdin(input.into_bytes().into()).await?;
        }
        child.close_stdin().await?;
        let mut stdout = child.stdout()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stdout.next_chunk().await {
            bytes.extend_from_slice(&chunk?);
        }
        if child.exit_code().await? != 0 {
            return Err(PluginError::io(format!(
                "{} check-in: the device identity helper failed",
                runtime.name
            )));
        }
        Ok(bytes)
    })
    .await
    .ok_or_else(|| {
        PluginError::io(format!(
            "{} check-in: timed out reading device identity",
            runtime.name
        ))
    })??;
    parse_native(&output).map_err(|_| {
        PluginError::decode(format!(
            "{} check-in: invalid device identity from the runtime",
            runtime.name
        ))
    })
}

fn parse_native(output: &[u8]) -> Result<NativeIdentity, ()> {
    let line = output.split(|byte| *byte == b'\n').next().ok_or(())?;
    let root: Value = hofund_plugin_sdk::json::from_slice(line).map_err(|_| ())?;
    let field = |name: &str| {
        root.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 4096
                    && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
            })
            .ok_or(())
    };
    Ok(NativeIdentity {
        token: SecretString::from(field("machineToken")?.to_owned()),
        machine_type: field("machineType")?.to_owned(),
    })
}

#[derive(Clone)]
struct RuntimeEnv {
    vars: BTreeMap<OsString, OsString>,
    platform: String,
    arch: String,
    home: Option<PathBuf>,
    cwd: PathBuf,
    opt: PathBuf,
    hostname: Option<String>,
}

impl RuntimeEnv {
    fn from_process() -> Self {
        const NAMES: &[&str] = &[
            "HOME",
            "USERPROFILE",
            "QODER_RUNTIME_INFO",
            "QODERCN_RUNTIME_INFO",
            "QODER_CONFIG_DIR",
            "QODERCN_CONFIG_DIR",
            "QODER_CLI_HOME",
            "QODERCN_CLI_HOME",
            "QODER_CONFIG_DIR_NAME",
            "QODERCN_CONFIG_DIR_NAME",
            "GEMINI_CLI_HOME",
            "LOCALAPPDATA",
            "PROGRAMFILES",
            "ProgramFiles",
            "ProgramW6432",
            "SystemRoot",
            "HOSTNAME",
            "COMPUTERNAME",
        ];
        let vars = NAMES
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
            .collect();
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let hostname = std::env::var("HOSTNAME")
            .ok()
            .or_else(|| std::env::var("COMPUTERNAME").ok());
        Self {
            vars,
            platform: node_os(std::env::consts::OS).to_owned(),
            arch: node_arch(std::env::consts::ARCH).to_owned(),
            home,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            opt: PathBuf::from("/opt"),
            hostname,
        }
    }

    fn var(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .get(OsStr::new(name))
            .map(OsString::as_os_str)
            .filter(|value| !value.is_empty())
    }

    fn absolute(&self, path: impl Into<PathBuf>) -> PathBuf {
        let path = path.into();
        if path.is_absolute() {
            path
        } else {
            self.cwd.join(path)
        }
    }

    fn config_home(&self, runtime: RuntimeSpec) -> PathBuf {
        if let Some(path) = self.var(runtime.config_var) {
            return self.absolute(path);
        }
        let parent = self
            .var(runtime.cli_home_var)
            .or_else(|| self.var("GEMINI_CLI_HOME"))
            .map(PathBuf::from)
            .or_else(|| self.home.clone())
            .unwrap_or_default();
        let name = self
            .var(runtime.config_name_var)
            .unwrap_or_else(|| OsStr::new(runtime.config_dir));
        self.absolute(parent.join(name))
    }
}

#[cfg(test)]
pub(crate) struct TestEnv {
    vars: BTreeMap<OsString, OsString>,
    platform: String,
    arch: String,
    home: Option<PathBuf>,
    cwd: PathBuf,
    opt: PathBuf,
}

#[cfg(test)]
impl TestEnv {
    pub(crate) fn new(cwd: &Path) -> Self {
        Self {
            vars: BTreeMap::new(),
            platform: "linux".to_owned(),
            arch: "x64".to_owned(),
            home: None,
            cwd: cwd.to_path_buf(),
            opt: cwd.join("opt"),
        }
    }

    pub(crate) fn var(mut self, name: &str, value: impl Into<OsString>) -> Self {
        self.vars.insert(OsString::from(name), value.into());
        self
    }

    pub(crate) fn home(mut self, home: &Path) -> Self {
        self.home = Some(home.to_path_buf());
        self
    }

    fn into_runtime(self, hostname: &str) -> RuntimeEnv {
        let home = self
            .home
            .or_else(|| self.vars.get(OsStr::new("HOME")).map(PathBuf::from))
            .or_else(|| self.vars.get(OsStr::new("USERPROFILE")).map(PathBuf::from));
        RuntimeEnv {
            vars: self.vars,
            platform: self.platform,
            arch: self.arch,
            home,
            cwd: self.cwd,
            opt: self.opt,
            hostname: Some(hostname.to_owned()),
        }
    }
}

#[derive(Clone, Copy)]
struct RuntimeSpec {
    name: &'static str,
    environment: &'static str,
    override_var: &'static str,
    app: &'static str,
    linux: &'static str,
    config_var: &'static str,
    cli_home_var: &'static str,
    config_name_var: &'static str,
    config_dir: &'static str,
}

fn runtime_spec(site: Site) -> RuntimeSpec {
    match site {
        Site::Global => RuntimeSpec {
            name: "Qoder",
            environment: "3",
            override_var: "QODER_RUNTIME_INFO",
            app: "Qoder",
            linux: "Qoder",
            config_var: "QODER_CONFIG_DIR",
            cli_home_var: "QODER_CLI_HOME",
            config_name_var: "QODER_CONFIG_DIR_NAME",
            config_dir: ".qoder",
        },
        Site::China => RuntimeSpec {
            name: "Qoder CN",
            environment: "0",
            override_var: "QODERCN_RUNTIME_INFO",
            app: "Qoder CN",
            linux: "Qoder-CN",
            config_var: "QODERCN_CONFIG_DIR",
            cli_home_var: "QODERCN_CLI_HOME",
            config_name_var: "QODERCN_CONFIG_DIR_NAME",
            config_dir: ".qoder-cn",
        },
    }
}

fn runtime_path(env: &RuntimeEnv, runtime: RuntimeSpec) -> Option<PathBuf> {
    if let Some(path) = env.var(runtime.override_var) {
        return Some(env.absolute(path));
    }
    let executable = if env.platform == "win32" {
        "runtime-info.exe"
    } else {
        "runtime-info"
    };
    let mut candidates = Vec::new();
    match env.platform.as_str() {
        "win32" => {
            let local = env
                .var("LOCALAPPDATA")
                .map(PathBuf::from)
                .or_else(|| env.home.as_ref().map(|home| home.join("AppData/Local")));
            if let Some(local) = local {
                candidates.push(
                    local
                        .join("Programs")
                        .join(runtime.app)
                        .join("resources/umid")
                        .join(executable),
                );
            }
            for name in ["ProgramFiles", "PROGRAMFILES", "ProgramW6432"] {
                if let Some(base) = env.var(name) {
                    candidates.push(
                        PathBuf::from(base)
                            .join(runtime.app)
                            .join("resources/umid")
                            .join(executable),
                    );
                }
            }
        }
        "darwin" => {
            candidates.push(
                PathBuf::from("/Applications")
                    .join(format!("{}.app", runtime.app))
                    .join("Contents/Resources/umid")
                    .join(executable),
            );
            if let Some(home) = &env.home {
                candidates.push(
                    home.join("Applications")
                        .join(format!("{}.app", runtime.app))
                        .join("Contents/Resources/umid")
                        .join(executable),
                );
            }
        }
        "linux" => candidates.push(
            env.opt
                .join(runtime.linux)
                .join("resources/umid")
                .join(executable),
        ),
        _ => {}
    }
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return Some(path);
    }
    let cache = env.config_home(runtime).join(".bin");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(cache).ok()?.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if env.platform == "win32" {
            if kind.is_dir() && name.starts_with(&format!("umid-win32-{}-", env.arch)) {
                entry.path().join(executable)
            } else {
                continue;
            }
        } else {
            if !kind.is_file()
                || !name.starts_with(&format!("runtime-info-{}-{}-", env.platform, env.arch))
                || name.ends_with(".tmp")
            {
                continue;
            }
            entry.path()
        };
        if path.is_file() {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((modified, path));
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    found.pop().map(|(_, path)| path)
}

async fn read_hostname(host: &HostContext, env: &RuntimeEnv) -> String {
    if let Some(hostname) = &env.hostname {
        return hostname.clone();
    }
    let program = if env.platform == "win32" {
        env.var("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Windows"))
            .join("System32/hostname.exe")
    } else if Path::new("/bin/hostname").is_file() {
        PathBuf::from("/bin/hostname")
    } else {
        PathBuf::from("/usr/bin/hostname")
    };
    let Ok(request) = SpawnRequest::new(program, Vec::new(), Vec::new(), &env.cwd) else {
        return String::new();
    };
    let Ok(child) = host.spawn_process().spawn_piped(&request).await else {
        return String::new();
    };
    let output = hofund_plugin_sdk::timeout(PROCESS_WAIT, async move {
        child.close_stdin().await.ok()?;
        let mut stdout = child.stdout().ok()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = stdout.next_chunk().await {
            bytes.extend_from_slice(&chunk.ok()?);
            if bytes.len() > 4096 {
                return None;
            }
        }
        (child.exit_code().await.ok()? == 0).then_some(bytes)
    })
    .await;
    output
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

fn node_arch(arch: &str) -> &str {
    match arch {
        "x86" => "ia32",
        other => other,
    }
}

fn node_os(os: &str) -> &str {
    match os {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn machine_os(arch: &str, platform: &str) -> String {
    let arch = match arch {
        "x64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    };
    format!("{arch}_{platform}")
}

fn machine_hostname(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        return String::new();
    }
    if hostname_safe(name) {
        return cap_hostname(name);
    }
    if let Ok(hofund_plugin_sdk::url::Host::Domain(ascii)) =
        hofund_plugin_sdk::url::Host::parse(name)
        && hostname_safe(&ascii)
    {
        return cap_hostname(&ascii);
    }
    let mut safe = String::new();
    let mut dash = false;
    for character in name.chars() {
        if ('\x21'..='\x7e').contains(&character) {
            safe.push(character);
            dash = false;
        } else if !dash {
            safe.push('-');
            dash = true;
        }
    }
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        cap_hostname(&format!("unknown-{}", hostname_hash(name)))
    } else {
        cap_hostname(&format!("{safe}-{}", hostname_hash(name)))
    }
}

fn hostname_safe(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.iter().all(|byte| (0x20..=0x7e).contains(byte))
        && bytes[0] != b' '
        && bytes[bytes.len() - 1] != b' '
}

fn cap_hostname(value: &str) -> String {
    if value.len() <= 96 {
        return value.to_owned();
    }
    let head = value[..87]
        .trim_end_matches(|character: char| character == '-' || character.is_whitespace());
    if head.is_empty() {
        format!("unknown-{}", hostname_hash(value))
    } else {
        format!("{head}-{}", hostname_hash(value))
    }
}

fn hostname_hash(value: &str) -> String {
    digest::sha256(value.as_bytes())[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r54_machine_os_and_hostname_match_reference_fixtures() {
        assert_eq!(machine_os("x64", "win32"), "x86_64_win32");
        assert_eq!(machine_os("arm64", "darwin"), "aarch64_darwin");
        assert_eq!(machine_os("ia32", "win32"), "ia32_win32");
        assert_eq!(machine_hostname("  my-mac.local "), "my-mac.local");
        assert_eq!(machine_hostname("büro"), "xn--bro-hoa");
        assert_eq!(machine_hostname(""), "");
        let long = "a".repeat(120);
        let capped = machine_hostname(&long);
        assert_eq!(capped.len(), 96);
        assert!(capped.ends_with(&hostname_hash(&long)));
    }

    #[test]
    fn r54_native_identity_requires_two_safe_fields() {
        let parsed = parse_native(br#"{"machineToken":"fixture-machine-token","machineType":"7"}"#)
            .expect("identity");
        assert_eq!(secret_text(&parsed.token), "fixture-machine-token");
        assert_eq!(parsed.machine_type, "7");
        for invalid in [
            br#"{"machineToken":"","machineType":"7"}"#.as_slice(),
            br#"{"machineToken":"token","machineType":""}"#.as_slice(),
            br#"{"machineToken":"token\nvalue","machineType":"7"}"#.as_slice(),
        ] {
            assert!(parse_native(invalid).is_err());
        }
    }

    #[test]
    fn r54_cn_runtime_home_is_distinct_and_uses_userprofile_fallback() {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join("profile");
        let env = TestEnv::new(temp.path())
            .var("USERPROFILE", home.as_os_str())
            .into_runtime("fixture-host");
        assert_eq!(
            env.config_home(runtime_spec(Site::Global)),
            home.join(".qoder")
        );
        assert_eq!(
            env.config_home(runtime_spec(Site::China)),
            home.join(".qoder-cn")
        );
        let env = TestEnv::new(temp.path())
            .var("QODER_CONFIG_DIR", temp.path().join("global"))
            .var("QODERCN_CONFIG_DIR", temp.path().join("china"))
            .into_runtime("fixture-host");
        assert_eq!(
            env.config_home(runtime_spec(Site::China)),
            temp.path().join("china")
        );
    }

    #[test]
    fn r54_cn_runtime_discovers_only_cn_helper_cache() {
        let temp = tempfile::tempdir().expect("temp dir");
        let global = temp.path().join("global");
        let china = temp.path().join("china");
        for (root, name) in [
            (&global, "runtime-info-linux-x64-global"),
            (&china, "runtime-info-linux-x64-cn"),
        ] {
            std::fs::create_dir_all(root.join(".bin")).expect("cache");
            std::fs::write(root.join(".bin").join(name), "fixture").expect("helper");
        }
        let env = TestEnv::new(temp.path())
            .var("QODER_CONFIG_DIR", global.as_os_str())
            .var("QODERCN_CONFIG_DIR", china.as_os_str())
            .into_runtime("fixture-host");
        assert_eq!(
            runtime_path(&env, runtime_spec(Site::China)).expect("CN runtime"),
            china.join(".bin/runtime-info-linux-x64-cn")
        );
    }
}
