use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_control_proto::API_VERSION;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{Client, ClientError, Endpoint};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Runtime roots used to locate control discovery and sockets.
#[derive(Clone, Debug)]
pub struct DiscoveryRuntime {
    /// The runtime directory nominated by the operating environment.
    pub xdg_runtime_dir: Option<PathBuf>,
    /// The operating system's temporary directory.
    pub temp_dir: PathBuf,
    /// The current user's local application-data directory.
    pub local_app_data: Option<PathBuf>,
}

impl DiscoveryRuntime {
    fn from_env() -> Self {
        Self {
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            temp_dir: std::env::temp_dir(),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        }
    }
}

/// Ordered control discovery settings.
#[derive(Clone, Debug)]
pub struct DiscoveryOptions {
    /// An explicit numeric-loopback TCP or Unix endpoint. It takes precedence
    /// over `HOFUND_CONTROL` and never falls through to another endpoint.
    pub explicit_endpoint: Option<String>,
    /// The final configured TCP candidate, defaulting to `127.0.0.1:3426`.
    pub control_listen: Option<SocketAddr>,
    /// Runtime roots used by file and socket discovery.
    pub runtime: DiscoveryRuntime,
    use_environment: bool,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            explicit_endpoint: None,
            control_listen: None,
            runtime: DiscoveryRuntime::from_env(),
            use_environment: true,
        }
    }
}

impl DiscoveryOptions {
    /// Constructs discovery settings that do not read process environment.
    #[doc(hidden)]
    #[must_use]
    pub fn isolated(xdg_runtime_dir: Option<PathBuf>, temp_dir: PathBuf) -> Self {
        Self {
            explicit_endpoint: None,
            control_listen: None,
            runtime: DiscoveryRuntime {
                xdg_runtime_dir,
                temp_dir,
                local_app_data: None,
            },
            use_environment: false,
        }
    }
}

#[derive(Debug)]
struct ConfigIdentity {
    resolved_path: PathBuf,
    id: String,
}

impl ConfigIdentity {
    fn resolve(path: &Path) -> Result<Self, ClientError> {
        let resolved_path = std::fs::canonicalize(path).map_err(|error| {
            ClientError::Discovery(format!(
                "cannot resolve config path {}: {error}",
                path.display()
            ))
        })?;
        let text = resolved_path.to_str().ok_or_else(|| {
            ClientError::Discovery(format!(
                "canonical config path is not valid UTF-8: {}",
                resolved_path.display()
            ))
        })?;
        #[cfg(any(target_os = "macos", windows))]
        let normalized = text.to_lowercase();
        #[cfg(not(any(target_os = "macos", windows)))]
        let normalized = text.to_owned();
        let id = config_id(&normalized);
        Ok(Self { resolved_path, id })
    }
}

fn config_id(normalized_path: &str) -> String {
    let digest = Sha256::digest(normalized_path.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Deserialize)]
struct DiscoveryFile {
    api_version: u32,
    #[serde(rename = "pid")]
    _pid: u32,
    boot_id: String,
    config_id: String,
    #[serde(default)]
    tcp_endpoints: Vec<SocketAddr>,
    #[serde(default)]
    socket_path: Option<PathBuf>,
}

pub(crate) async fn discover(
    config_path: &Path,
    token: String,
    options: DiscoveryOptions,
) -> Result<Client, ClientError> {
    let identity = ConfigIdentity::resolve(config_path)?;
    let explicit = match &options.explicit_endpoint {
        Some(endpoint) => Some(endpoint.clone()),
        None if options.use_environment => match std::env::var("HOFUND_CONTROL") {
            Ok(endpoint) => Some(endpoint),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(ClientError::Discovery(
                    "HOFUND_CONTROL is not valid Unicode".to_owned(),
                ));
            }
        },
        None => None,
    };
    if let Some(explicit) = explicit {
        let client = Client::from_endpoint(parse_explicit(&explicit)?, token);
        return probe(client).await;
    }

    #[cfg(unix)]
    let uid = current_uid(&options.runtime.temp_dir)?;
    let primary = identity
        .resolved_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".hofund")
        .join(format!("control-{}.json", identity.id));
    let mut paths = vec![primary];
    let runtime_dirs = runtime_dirs(&options.runtime)?;
    paths.extend(
        runtime_dirs
            .iter()
            .map(|dir| dir.join(format!("control-{}.json", identity.id))),
    );
    for path in paths {
        let Some(discovery) = read_discovery(
            &path,
            &identity.id,
            #[cfg(unix)]
            uid,
        )?
        else {
            continue;
        };
        for endpoint in discovery_endpoints(discovery, &path)? {
            match probe(Client::from_endpoint(endpoint, token.clone())).await {
                Err(error) if error.is_unavailable() => {}
                result => return result,
            }
        }
    }

    #[cfg(unix)]
    for dir in runtime_dirs {
        let endpoint = Endpoint::Unix(dir.join(format!("control-{}.sock", identity.id)));
        match probe(Client::from_endpoint(endpoint, token.clone())).await {
            Err(error) if error.is_unavailable() => {}
            result => return result,
        }
    }

    const DEFAULT_CONTROL_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3426);
    let addr = options.control_listen.unwrap_or(DEFAULT_CONTROL_ADDR);
    if !addr.ip().is_loopback() {
        return Err(ClientError::Discovery(
            "configured control TCP endpoint must be loopback".to_owned(),
        ));
    }
    probe(Client::tcp(addr, token)).await
}

/// A daemon lifetime lock that another process holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldLock {
    /// The lock file.
    pub path: PathBuf,
    /// The holder's PID from the discovery file beside the lock, when one is
    /// readable and names this config.
    pub pid: Option<u32>,
}

/// The canonical config id used in discovery, lock, and socket names.
///
/// # Errors
///
/// Returns [`ClientError::Discovery`] when the path cannot be resolved.
pub fn resolve_config_id(config_path: &Path) -> Result<String, ClientError> {
    ConfigIdentity::resolve(config_path).map(|identity| identity.id)
}

/// Probes the daemon lifetime lock at every lookup location for
/// `config_path` and returns the ones another process holds.
///
/// The probe opens existing lock files only. It never creates a lock file
/// or a directory, so it is safe before a one-shot run.
///
/// # Errors
///
/// Returns [`ClientError::Discovery`] when the config path cannot be
/// resolved, or a lock file exists but cannot be opened or probed.
pub fn held_daemon_locks(
    config_path: &Path,
    runtime: &DiscoveryRuntime,
) -> Result<Vec<HeldLock>, ClientError> {
    let identity = ConfigIdentity::resolve(config_path)?;
    let mut dirs = vec![
        identity
            .resolved_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".hofund"),
    ];
    dirs.extend(runtime_dirs(runtime)?);
    let mut held = Vec::new();
    for dir in dirs {
        let path = dir.join(format!("daemon-{}.lock", identity.id));
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ClientError::Discovery(format!(
                    "cannot open daemon lock {}: {error}",
                    path.display()
                )));
            }
        };
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let pid = holder_pid(&dir, &identity.id);
                held.push(HeldLock { path, pid });
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(ClientError::Discovery(format!(
                    "cannot probe daemon lock {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Ok(held)
}

fn holder_pid(dir: &Path, config_id: &str) -> Option<u32> {
    #[derive(Deserialize)]
    struct Holder {
        pid: u32,
        config_id: String,
    }
    let bytes = std::fs::read(dir.join(format!("control-{config_id}.json"))).ok()?;
    let holder: Holder = serde_json::from_slice(&bytes).ok()?;
    (holder.config_id == config_id).then_some(holder.pid)
}

fn discovery_endpoints(
    discovery: DiscoveryFile,
    discovery_path: &Path,
) -> Result<Vec<Endpoint>, ClientError> {
    let mut endpoints = Vec::new();
    #[cfg(unix)]
    if let Some(path) = discovery.socket_path {
        if !path.is_absolute() {
            return Err(ClientError::Discovery(format!(
                "discovery {} contains a relative socket path",
                discovery_path.display()
            )));
        }
        endpoints.push(Endpoint::Unix(path));
    }
    #[cfg(not(unix))]
    if discovery.socket_path.is_some() {
        return Err(ClientError::Discovery(format!(
            "discovery {} names unsupported local IPC",
            discovery_path.display()
        )));
    }
    for addr in discovery.tcp_endpoints {
        if !addr.ip().is_loopback() {
            return Err(ClientError::Discovery(format!(
                "discovery {} contains a non-loopback TCP endpoint",
                discovery_path.display()
            )));
        }
        endpoints.push(Endpoint::Tcp(addr));
    }
    if endpoints.is_empty() {
        return Err(ClientError::Discovery(format!(
            "discovery {} contains no control endpoint",
            discovery_path.display()
        )));
    }
    Ok(endpoints)
}

async fn probe(client: Client) -> Result<Client, ClientError> {
    let health = client.health().await?;
    if health.api_version != API_VERSION {
        return Err(ClientError::Discovery(format!(
            "control API version {} does not match {API_VERSION}",
            health.api_version
        )));
    }
    client.status().await?;
    Ok(client)
}

fn parse_explicit(value: &str) -> Result<Endpoint, ClientError> {
    #[cfg(unix)]
    if let Some(path) = value.strip_prefix("unix://") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(ClientError::Discovery(
                "explicit Unix control endpoint must be absolute".to_owned(),
            ));
        }
        return Ok(Endpoint::Unix(path));
    }
    let address = value.strip_prefix("tcp://").unwrap_or(value);
    let addr = address.parse::<SocketAddr>().map_err(|_| {
        ClientError::Discovery(format!(
            "explicit control endpoint must be numeric loopback TCP or local IPC: {value}"
        ))
    })?;
    if !addr.ip().is_loopback() {
        return Err(ClientError::Discovery(
            "explicit control TCP endpoint must be loopback".to_owned(),
        ));
    }
    Ok(Endpoint::Tcp(addr))
}

fn read_discovery(
    path: &Path,
    config_id: &str,
    #[cfg(unix)] uid: u32,
) -> Result<Option<DiscoveryFile>, ClientError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ClientError::Discovery(format!(
                "cannot inspect control discovery {}: {error}",
                path.display()
            )));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ClientError::Discovery(format!(
            "control discovery is not a regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != uid {
            return Err(ClientError::Discovery(format!(
                "control discovery is not owned by the current user: {}",
                path.display()
            )));
        }
    }
    let bytes = std::fs::read(path).map_err(|error| {
        ClientError::Discovery(format!(
            "cannot read control discovery {}: {error}",
            path.display()
        ))
    })?;
    let discovery: DiscoveryFile = serde_json::from_slice(&bytes).map_err(|error| {
        ClientError::Discovery(format!(
            "cannot decode control discovery {}: {error}",
            path.display()
        ))
    })?;
    if discovery.api_version != API_VERSION {
        return Err(ClientError::Discovery(format!(
            "control discovery {} has API version {}, expected {API_VERSION}",
            path.display(),
            discovery.api_version
        )));
    }
    if discovery.config_id != config_id {
        return Err(ClientError::Discovery(format!(
            "control discovery {} belongs to a different config",
            path.display()
        )));
    }
    if discovery.boot_id.is_empty() {
        return Err(ClientError::Discovery(format!(
            "control discovery {} has an empty boot id",
            path.display()
        )));
    }
    Ok(Some(discovery))
}

fn runtime_dirs(runtime: &DiscoveryRuntime) -> Result<Vec<PathBuf>, ClientError> {
    #[cfg(unix)]
    {
        let uid = current_uid(&runtime.temp_dir)?;
        let mut result = Vec::new();
        if let Some(base) = &runtime.xdg_runtime_dir
            && base.is_absolute()
            && valid_unix_base(base, uid)
        {
            let path = base.join("hofund");
            if valid_unix_dir(&path, uid) {
                result.push(path);
            }
        }
        if result.is_empty() {
            let base = std::fs::canonicalize(&runtime.temp_dir).map_err(|error| {
                ClientError::Discovery(format!(
                    "cannot resolve temporary runtime directory {}: {error}",
                    runtime.temp_dir.display()
                ))
            })?;
            let path = base.join(format!("hofund-{uid}"));
            if valid_unix_dir(&path, uid) {
                result.push(path);
            }
        }
        Ok(result)
    }
    #[cfg(windows)]
    {
        let path = runtime
            .local_app_data
            .as_ref()
            .map(|base| base.join("hofund").join("run"));
        Ok(path.into_iter().filter(|path| path.is_dir()).collect())
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(Vec::new())
    }
}

#[cfg(unix)]
fn valid_unix_base(base: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(base).is_ok_and(|metadata| {
        metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid() == uid
    })
}

#[cfg(unix)]
fn valid_unix_dir(path: &Path, uid: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == uid
            && metadata.permissions().mode() & 0o077 == 0
    })
}

#[cfg(unix)]
pub(crate) fn current_uid(temp_dir: &Path) -> Result<u32, ClientError> {
    use std::os::unix::fs::MetadataExt;
    for _ in 0..16 {
        let path = temp_dir.join(format!(
            ".hofund-client-uid-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                let uid = file
                    .metadata()
                    .map(|metadata| metadata.uid())
                    .map_err(|error| {
                        ClientError::Discovery(format!(
                            "cannot determine the current user id: {error}"
                        ))
                    });
                drop(file);
                let _ = std::fs::remove_file(path);
                return uid;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(ClientError::Discovery(format!(
                    "cannot determine the current user id in {}: {error}",
                    temp_dir.display()
                )));
            }
        }
    }
    Err(ClientError::Discovery(
        "cannot allocate a current-user probe".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::{DiscoveryFile, config_id, discovery_endpoints, read_discovery};
    use std::path::{Path, PathBuf};

    struct Scratch(tempfile::TempDir);

    impl Scratch {
        fn new(label: &str) -> Self {
            Self(
                tempfile::Builder::new()
                    .prefix(&format!("hofund-client-discovery-{label}-"))
                    .tempdir()
                    .expect("scratch directory"),
            )
        }

        fn join(&self, path: &str) -> PathBuf {
            self.0.path().join(path)
        }
    }

    fn write_discovery(path: &Path, config_id: &str) {
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "api_version": 1,
                "pid": 7,
                "boot_id": "boot",
                "config_id": config_id,
                "tcp_endpoints": ["127.0.0.1:3426"],
                "socket_path": null,
            }))
            .expect("discovery JSON"),
        )
        .expect("discovery file");
    }

    #[test]
    fn config_id_known_vector() {
        assert_eq!(config_id("/tmp/hofund.toml"), "1664910274138c0f");
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_wrong_owner() {
        let dir = Scratch::new("owner");
        let path = dir.join("control.json");
        write_discovery(&path, "expected");
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata(&path).expect("metadata").uid();
        let error = read_discovery(&path, "expected", uid.wrapping_add(1))
            .expect_err("wrong owner was accepted");
        assert!(matches!(error, crate::ClientError::Discovery(_)));
    }

    #[test]
    fn discovery_rejects_mismatched_config_id() {
        let dir = Scratch::new("config-id");
        let path = dir.join("control.json");
        write_discovery(&path, "other-config");
        #[cfg(unix)]
        let error = {
            use std::os::unix::fs::MetadataExt;
            let uid = std::fs::metadata(&path).expect("metadata").uid();
            read_discovery(&path, "expected", uid).expect_err("wrong config id")
        };
        #[cfg(not(unix))]
        let error = read_discovery(&path, "expected").expect_err("wrong config id");
        assert!(matches!(error, crate::ClientError::Discovery(_)));
    }

    #[test]
    fn discovery_rejects_non_loopback_tcp_endpoint() {
        let discovery = DiscoveryFile {
            api_version: 1,
            _pid: 7,
            boot_id: "boot".to_owned(),
            config_id: "expected".to_owned(),
            tcp_endpoints: vec!["192.0.2.1:3426".parse().expect("TCP address")],
            socket_path: None,
        };
        let error = discovery_endpoints(discovery, Path::new("control.json"))
            .expect_err("non-loopback endpoint");
        assert!(matches!(error, crate::ClientError::Discovery(_)));
    }
}
