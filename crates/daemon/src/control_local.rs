use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hofund_control_proto::API_VERSION;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Runtime roots used for local control state.
#[derive(Clone, Debug)]
pub struct ControlRuntime {
    /// The runtime directory nominated by the operating environment.
    pub xdg_runtime_dir: Option<PathBuf>,
    /// The operating system's temporary directory.
    pub temp_dir: PathBuf,
    /// The current user's local application-data directory.
    pub local_app_data: Option<PathBuf>,
}

impl ControlRuntime {
    /// Reads runtime roots from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            temp_dir: std::env::temp_dir(),
            local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        }
    }

    /// Constructs isolated runtime roots for tests.
    #[doc(hidden)]
    #[must_use]
    pub fn new(xdg_runtime_dir: Option<PathBuf>, temp_dir: PathBuf) -> Self {
        Self {
            xdg_runtime_dir,
            temp_dir,
            local_app_data: None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigIdentity {
    pub(crate) resolved_path: PathBuf,
    pub(crate) id: String,
}

impl ConfigIdentity {
    pub(crate) fn resolve(path: &Path) -> Result<Self, String> {
        let resolved_path = std::fs::canonicalize(path)
            .map_err(|error| format!("cannot resolve config path {}: {error}", path.display()))?;
        let normalized = normalized_path(&resolved_path)?;
        let id = config_id(&normalized);
        Ok(Self { resolved_path, id })
    }
}

fn normalized_path(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or_else(|| {
        format!(
            "canonical config path is not valid UTF-8: {}",
            path.display()
        )
    })?;
    #[cfg(any(target_os = "macos", windows))]
    {
        Ok(text.to_lowercase())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Ok(text.to_owned())
    }
}

fn config_id(normalized_path: &str) -> String {
    let digest = Sha256::digest(normalized_path.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) struct ControlSession {
    identity: ConfigIdentity,
    discovery_path: PathBuf,
    boot_id: String,
    _lock: File,
    published: bool,
}

impl ControlSession {
    pub(crate) fn acquire(
        identity: ConfigIdentity,
        runtime: &ControlRuntime,
    ) -> Result<Self, String> {
        let primary = identity
            .resolved_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(".hofund");
        let state_dir = match prepare_primary_dir(&primary) {
            Ok(()) => match acquire_lock(&primary, &identity.id) {
                Ok(lock) => {
                    return Ok(Self::new(identity, primary, lock));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Err(already_served(&identity, runtime));
                }
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    prepare_runtime_dir(runtime)?
                }
                Err(error) => {
                    return Err(format!(
                        "cannot open daemon lock in {}: {error}",
                        primary.display()
                    ));
                }
            },
            Err(_) => prepare_runtime_dir(runtime)?,
        };
        let lock = acquire_lock(&state_dir, &identity.id).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                already_served(&identity, runtime)
            } else {
                format!(
                    "cannot open daemon lock in {}: {error}",
                    state_dir.display()
                )
            }
        })?;
        Ok(Self::new(identity, state_dir, lock))
    }

    fn new(identity: ConfigIdentity, state_dir: PathBuf, lock: File) -> Self {
        let discovery_path = state_dir.join(format!("control-{}.json", identity.id));
        let boot_id = boot_id(&identity.id);
        Self {
            identity,
            discovery_path,
            boot_id,
            _lock: lock,
            published: false,
        }
    }

    /// Configuration sidecar lock used when the directory beside the file
    /// cannot hold one (read-only or not writable).
    pub(crate) fn config_lock_fallback(&self, runtime: &ControlRuntime) -> Option<PathBuf> {
        config_lock_fallback(&self.identity, runtime)
    }

    #[cfg(unix)]
    pub(crate) fn socket_path(&self, runtime: &ControlRuntime) -> Result<PathBuf, String> {
        Ok(prepare_runtime_dir(runtime)?.join(format!("control-{}.sock", self.identity.id)))
    }

    #[cfg(unix)]
    pub(crate) fn prepare_socket(&self, path: &Path) -> Result<(), String> {
        use std::os::unix::fs::FileTypeExt;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "cannot inspect control socket {}: {error}",
                    path.display()
                ));
            }
        };
        if !metadata.file_type().is_socket() {
            return Err(format!(
                "refusing to replace non-socket control path {}",
                path.display()
            ));
        }
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => Err(format!(
                "control socket is already serving connections: {}",
                path.display()
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                std::fs::remove_file(path).map_err(|remove| {
                    format!(
                        "cannot remove stale control socket {}: {remove}",
                        path.display()
                    )
                })
            }
            Err(error) => Err(format!(
                "cannot probe control socket {}: {error}",
                path.display()
            )),
        }
    }

    pub(crate) fn publish(
        &mut self,
        tcp_endpoints: Vec<SocketAddr>,
        socket_path: Option<PathBuf>,
    ) -> Result<(), String> {
        let discovery = DiscoveryFile {
            api_version: API_VERSION,
            pid: std::process::id(),
            boot_id: self.boot_id.clone(),
            config_id: self.identity.id.clone(),
            tcp_endpoints,
            socket_path,
        };
        write_discovery(&self.discovery_path, &discovery)?;
        self.published = true;
        Ok(())
    }
}

impl Drop for ControlSession {
    fn drop(&mut self) {
        if self.published {
            let _ = std::fs::remove_file(&self.discovery_path);
        }
    }
}

/// Configuration sidecar lock in the first runtime directory, used when the
/// directory beside the config cannot hold one. One-shot operations use it
/// without a daemon session.
pub(crate) fn config_lock_fallback(
    identity: &ConfigIdentity,
    runtime: &ControlRuntime,
) -> Option<PathBuf> {
    prepare_runtime_dir(runtime)
        .ok()
        .map(|directory| directory.join(format!("config-{}.lock", identity.id)))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryFile {
    api_version: u32,
    pid: u32,
    boot_id: String,
    config_id: String,
    tcp_endpoints: Vec<SocketAddr>,
    socket_path: Option<PathBuf>,
}

fn acquire_lock(directory: &Path, id: &str) -> std::io::Result<File> {
    let path = directory.join(format!("daemon-{id}.lock"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.try_lock()?;
    Ok(lock)
}

fn prepare_primary_dir(path: &Path) -> std::io::Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(path)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                Ok(())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "control state path is not a directory",
                ))
            }
        }
        Err(error) => Err(error),
    }
}

fn already_served(identity: &ConfigIdentity, runtime: &ControlRuntime) -> String {
    let primary = identity
        .resolved_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".hofund")
        .join(format!("control-{}.json", identity.id));
    let mut paths = vec![primary];
    if let Ok(runtime_paths) = runtime_dirs(runtime, false) {
        paths.extend(
            runtime_paths
                .into_iter()
                .map(|path| path.join(format!("control-{}.json", identity.id))),
        );
    }
    for path in paths {
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(discovery) = serde_json::from_slice::<DiscoveryFile>(&bytes)
            && discovery.config_id == identity.id
        {
            return format!("already served by PID {}", discovery.pid);
        }
    }
    "already served by PID unknown".to_owned()
}

fn write_discovery(path: &Path, discovery: &DiscoveryFile) -> Result<(), String> {
    let bytes = serde_json::to_vec(discovery)
        .map_err(|error| format!("cannot encode control discovery: {error}"))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for _ in 0..16 {
        let temporary = parent.join(format!(
            ".control-{}-{}.tmp",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(mut file) => {
                let result = (|| -> std::io::Result<()> {
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    std::fs::rename(&temporary, path)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    let _ = std::fs::remove_file(&temporary);
                    return Err(format!(
                        "cannot write control discovery {}: {error}",
                        path.display()
                    ));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "cannot create control discovery in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    Err(format!(
        "cannot allocate temporary control discovery in {}",
        parent.display()
    ))
}

fn boot_id(config_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"hofund-control-boot-v1\0");
    hash.update(config_id.as_bytes());
    hash.update(std::process::id().to_be_bytes());
    hash.update(super::control::unix_milliseconds().to_be_bytes());
    hash.update(NEXT_ID.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    hash.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn prepare_runtime_dir(runtime: &ControlRuntime) -> Result<PathBuf, String> {
    runtime_dirs(runtime, true)?
        .into_iter()
        .next()
        .ok_or_else(|| "no per-user runtime directory is available for local control".to_owned())
}

fn runtime_dirs(runtime: &ControlRuntime, create: bool) -> Result<Vec<PathBuf>, String> {
    #[cfg(unix)]
    {
        let uid = current_uid(&runtime.temp_dir)?;
        let mut result = Vec::new();
        if let Some(base) = &runtime.xdg_runtime_dir
            && base.is_absolute()
            && valid_unix_base(base, uid)
        {
            let path = base.join("hofund");
            if prepare_unix_dir(&path, uid, create).is_ok() {
                result.push(path);
            }
        }
        if result.is_empty() {
            let base = std::fs::canonicalize(&runtime.temp_dir).map_err(|error| {
                format!(
                    "cannot resolve temporary runtime directory {}: {error}",
                    runtime.temp_dir.display()
                )
            })?;
            let path = base.join(format!("hofund-{uid}"));
            prepare_unix_dir(&path, uid, create).map_err(|error| {
                format!(
                    "cannot prepare control runtime directory {}: {error}",
                    path.display()
                )
            })?;
            result.push(path);
        }
        Ok(result)
    }
    #[cfg(windows)]
    {
        let base = runtime
            .local_app_data
            .as_ref()
            .ok_or_else(|| "LOCALAPPDATA is unavailable for local control state".to_owned())?;
        let path = base.join("hofund").join("run");
        if create {
            std::fs::create_dir_all(&path).map_err(|error| {
                format!(
                    "cannot prepare control runtime directory {}: {error}",
                    path.display()
                )
            })?;
        }
        Ok(vec![path])
    }
    #[cfg(not(any(unix, windows)))]
    {
        let path = runtime.temp_dir.join("hofund");
        if create {
            std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
        }
        Ok(vec![path])
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
fn prepare_unix_dir(path: &Path, uid: u32, create: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if create {
        match std::fs::create_dir(path) {
            Ok(()) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "runtime directory must be current-user-owned and inaccessible to group and others",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent)?;
    if !std::fs::metadata(parent)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "runtime parent is not a directory",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn current_uid(temp_dir: &Path) -> Result<u32, String> {
    use std::os::unix::fs::MetadataExt;
    for _ in 0..16 {
        let path = temp_dir.join(format!(
            ".hofund-uid-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                let uid = file
                    .metadata()
                    .map(|metadata| metadata.uid())
                    .map_err(|error| format!("cannot determine the current user id: {error}"));
                drop(file);
                let _ = std::fs::remove_file(path);
                return uid;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "cannot determine the current user id in {}: {error}",
                    temp_dir.display()
                ));
            }
        }
    }
    Err("cannot allocate a current-user probe".to_owned())
}

#[cfg(test)]
mod tests {
    use super::ConfigIdentity;
    #[cfg(windows)]
    use super::{ControlRuntime, ControlSession};

    fn scratch(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("hofund-control-local-{label}-"))
            .tempdir()
            .expect("scratch directory")
    }

    #[test]
    fn config_id_known_vector() {
        assert_eq!(super::config_id("/tmp/hofund.toml"), "1664910274138c0f");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_directory_rejects_group_and_world_access() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir_guard = scratch("runtime-mode");
        let dir = dir_guard.path();
        let runtime = dir.join("hofund");
        std::fs::create_dir(&runtime).expect("runtime directory");
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
            .expect("runtime mode");
        let uid = std::fs::metadata(&runtime).expect("metadata").uid();
        let error =
            super::prepare_unix_dir(&runtime, uid, false).expect_err("insecure runtime directory");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn canonical_config_id_resolves_symlinks_and_normalizes_case() {
        let dir_guard = scratch("identity");
        let dir = dir_guard.path();
        let target = dir.join("Config.toml");
        std::fs::write(&target, "listen = \"127.0.0.1:0\"\n").expect("config");
        let direct = ConfigIdentity::resolve(&target).expect("direct identity");
        #[cfg(unix)]
        {
            let link = dir.join("linked.toml");
            std::os::unix::fs::symlink(&target, &link).expect("symlink");
            let linked = ConfigIdentity::resolve(&link).expect("linked identity");
            assert_eq!(direct.id, linked.id);
            assert_eq!(direct.resolved_path, linked.resolved_path);
        }
        #[cfg(any(target_os = "macos", windows))]
        {
            let alternate = dir.join("CONFIG.TOML");
            let other = ConfigIdentity::resolve(&alternate).expect("case-insensitive identity");
            assert_eq!(direct.id, other.id);
        }
        assert_eq!(direct.id.len(), 16);
        assert!(direct.id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_single_instance_lock_uses_lockfileex() {
        let dir_guard = scratch("windows-lock");
        let dir = dir_guard.path();
        let config = dir.join("hofund.toml");
        std::fs::write(&config, "listen = \"127.0.0.1:0\"\n").expect("config");
        let identity = ConfigIdentity::resolve(&config).expect("identity");
        let runtime = ControlRuntime::new(None, dir.to_path_buf());
        let first = ControlSession::acquire(identity.clone(), &runtime).expect("first lock");
        let Err(error) = ControlSession::acquire(identity, &runtime) else {
            panic!("second lock was acquired");
        };
        assert!(error.contains("already served by PID"), "{error}");
        drop(first);
    }
}
