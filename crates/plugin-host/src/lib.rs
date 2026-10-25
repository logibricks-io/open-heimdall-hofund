//! The only supported constructors for [`HostContext`] and [`SurfaceHost`].
//!
//! Plugins do not depend on this crate. A plugin reaches a [`HostContext`]
//! through its kit's `test_host` in tests, and through `build` at runtime.
//!
//! The account backend shipped here is [`FileAccountStore`]: a mode `0600`
//! file and an in-memory cache dropped on write. The OS keychain is a later
//! backend behind [`AccountBackend`]. The scheduler records jobs and does not
//! spawn tasks. PR D's kernel scheduler is what runs them. The daemon does
//! not call any of this yet, so gateway behavior is unchanged.

#![deny(missing_docs)]

mod accounts;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use hofund_plugin_api::kernel::{
    AccountBackend, ClockBackend, DirBackend, HttpBackend, HttpRequest, HttpResponse,
    ObserveBackend, ProcessBackend, SchedulerBackend, SecretsBackend,
};
use hofund_plugin_api::{
    AuthResult, Body, BodyStream, HostContext, MemberId, PluginError, PluginId, ProtocolId,
    QuotaObservation, SecretString, SpawnRequest, SurfaceHost,
};

pub use accounts::{FileAccountStore, StoreError};

/// Builds a [`HostContext`] from kernel backends.
///
/// This is the constructor. [`HostContext::from_backends`](HostContext::from_backends)
/// exists so this crate can fill the private handles. Plugins do not call either one.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn host_context(
    http: Arc<dyn HttpBackend>,
    secrets: Arc<dyn SecretsBackend>,
    accounts: Arc<dyn AccountBackend>,
    clock: Arc<dyn ClockBackend>,
    scheduler: Arc<dyn SchedulerBackend>,
    process: Arc<dyn ProcessBackend>,
    dirs: Arc<dyn DirBackend>,
    observe: Arc<dyn ObserveBackend>,
) -> HostContext {
    HostContext::from_backends(
        http, secrets, accounts, clock, scheduler, process, dirs, observe,
    )
}

/// Builds a [`SurfaceHost`]. The gateway mount in PR D is what fills the live snapshot.
#[must_use]
pub fn surface_host(
    members: Vec<MemberId>,
    protocols: Vec<ProtocolId>,
    auth: AuthResult,
) -> SurfaceHost {
    SurfaceHost::from_parts(members, protocols, auth)
}

/// A [`HostContext`] whose backends record calls and do not touch the network.
///
/// `root` holds the account file and the per-plugin directories. Kits re-export
/// this function. A failure to create the account file panics: this function is
/// for tests, and a missing temp directory is a test bug.
///
/// # Panics
///
/// Panics when the account file cannot be created.
#[must_use]
pub fn test_host(root: impl Into<PathBuf>) -> HostContext {
    let root = root.into();
    let accounts =
        FileAccountStore::open(root.join("accounts.json")).expect("test host account file");
    host_context(
        Arc::new(RecordingHttp::default()),
        Arc::new(RecordingSecrets::default()),
        Arc::new(accounts),
        Arc::new(FixedClock {
            now: SystemTime::UNIX_EPOCH,
        }),
        Arc::new(RecordingScheduler::default()),
        Arc::new(RecordingProcess::default()),
        Arc::new(RootDirs { root }),
        Arc::new(RecordingObserve::default()),
    )
}

/// A clock that returns one instant. Tests use this. Production passes a system clock.
#[derive(Clone, Debug)]
pub struct FixedClock {
    now: SystemTime,
}

impl FixedClock {
    /// A clock stuck at `now`.
    #[must_use]
    pub fn new(now: SystemTime) -> Self {
        Self { now }
    }
}

impl ClockBackend for FixedClock {
    fn now(&self) -> SystemTime {
        self.now
    }
}

/// The process clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl ClockBackend for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Spawns with [`Command::new`]. It never invokes a shell.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdProcess;

impl StdProcess {
    /// The command a spawn would run. Tests check the program and the args.
    #[must_use]
    pub fn command(request: &SpawnRequest) -> Command {
        let mut command = Command::new(request.program());
        command.args(request.args());
        command.current_dir(request.cwd());
        command.env_clear();
        command.envs(request.env().iter().map(|(key, value)| (key, value)));
        command
    }
}

#[async_trait]
impl ProcessBackend for StdProcess {
    async fn spawn(
        &self,
        program: PathBuf,
        args: Vec<OsString>,
        env: Vec<(OsString, OsString)>,
        cwd: PathBuf,
    ) -> Result<u32, PluginError> {
        let request = SpawnRequest::new(program, args, env, cwd)?;
        let child = Self::command(&request)
            .spawn()
            .map_err(|error| PluginError::config(error.to_string()))?;
        Ok(child.id())
    }
}

#[derive(Default)]
struct RecordingHttp {
    hits: Mutex<u32>,
}

#[async_trait]
impl HttpBackend for RecordingHttp {
    async fn send(
        &self,
        _request: HttpRequest<Body>,
    ) -> Result<HttpResponse<BodyStream>, PluginError> {
        *self.hits.lock().expect("http lock") += 1;
        Ok(HttpResponse::new(BodyStream::from_bytes(&b"ok"[..])))
    }
}

#[derive(Default)]
struct RecordingSecrets {
    values: Mutex<BTreeMap<String, String>>,
}

#[async_trait]
impl SecretsBackend for RecordingSecrets {
    async fn get(&self, id: &str) -> Result<Option<SecretString>, PluginError> {
        Ok(self
            .values
            .lock()
            .expect("secrets lock")
            .get(id)
            .cloned()
            .map(SecretString::from))
    }
}

#[derive(Default)]
struct RecordingScheduler {
    jobs: Mutex<Vec<String>>,
}

impl SchedulerBackend for RecordingScheduler {
    fn register(
        &self,
        name: &str,
        _every_ms: u64,
        _job: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<u64, PluginError> {
        let mut jobs = self.jobs.lock().expect("scheduler lock");
        jobs.push(name.to_owned());
        Ok(u64::try_from(jobs.len()).expect("job id"))
    }

    fn cancel(&self, id: u64) {
        let mut jobs = self.jobs.lock().expect("scheduler lock");
        let index = usize::try_from(id.saturating_sub(1)).unwrap_or(0);
        if index < jobs.len() {
            jobs.remove(index);
        }
    }

    fn shutdown(&self) {
        self.jobs.lock().expect("scheduler lock").clear();
    }
}

#[derive(Default)]
struct RecordingProcess {
    programs: Mutex<Vec<PathBuf>>,
}

#[async_trait]
impl ProcessBackend for RecordingProcess {
    async fn spawn(
        &self,
        program: PathBuf,
        _args: Vec<OsString>,
        _env: Vec<(OsString, OsString)>,
        _cwd: PathBuf,
    ) -> Result<u32, PluginError> {
        self.programs.lock().expect("process lock").push(program);
        Ok(1)
    }
}

struct RootDirs {
    root: PathBuf,
}

impl DirBackend for RootDirs {
    fn cache_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError> {
        mkdir(&self.root.join("cache").join(plugin.as_str()))
    }

    fn state_dir(&self, plugin: &PluginId) -> Result<PathBuf, PluginError> {
        mkdir(&self.root.join("state").join(plugin.as_str()))
    }
}

fn mkdir(path: &Path) -> Result<PathBuf, PluginError> {
    std::fs::create_dir_all(path).map_err(|error| PluginError::config(error.to_string()))?;
    Ok(path.to_path_buf())
}

#[derive(Default)]
struct RecordingObserve {
    seen: Mutex<Vec<QuotaObservation>>,
}

impl ObserveBackend for RecordingObserve {
    fn observe(&self, observation: QuotaObservation) -> Result<(), PluginError> {
        self.seen.lock().expect("observe lock").push(observation);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use hofund_plugin_api::kernel::{self, AccountBackend};
    use hofund_plugin_api::{Account, AccountId, JobSpec, PluginId, SpawnRequest};

    use super::{FileAccountStore, StdProcess, surface_host, test_host};

    fn root() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hofund-host-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("root");
        path
    }

    #[tokio::test]
    async fn file_store_cache_drops_on_write_and_file_is_private() {
        let path = root().join("accounts.json");
        let store = FileAccountStore::open(&path).expect("open");
        let account = Account::new(
            PluginId::from_static("codex"),
            AccountId::from_static("work"),
            "Work",
        )
        .expect("account");
        store
            .upsert(account, hofund_plugin_api::SecretString::from("secret-a"))
            .await
            .expect("upsert");
        let first = store
            .secret(
                &PluginId::from_static("codex"),
                &AccountId::from_static("work"),
            )
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&first), "secret-a");
        store
            .upsert(
                Account::new(
                    PluginId::from_static("codex"),
                    AccountId::from_static("work"),
                    "Work",
                )
                .expect("account"),
                hofund_plugin_api::SecretString::from("secret-b"),
            )
            .await
            .expect("replace");
        let second = store
            .secret(
                &PluginId::from_static("codex"),
                &AccountId::from_static("work"),
            )
            .await
            .expect("secret")
            .expect("present");
        assert_eq!(kernel::expose_secret(&second), "secret-b");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn std_process_does_not_use_a_shell() {
        let program = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe")
        } else {
            std::path::PathBuf::from("/bin/true")
        };
        let cwd = if cfg!(windows) {
            std::path::PathBuf::from(r"C:\Windows")
        } else {
            std::path::PathBuf::from("/tmp")
        };
        let request = SpawnRequest::new(program.clone(), vec!["--version".into()], Vec::new(), cwd)
            .expect("spawn");
        let command = StdProcess::command(&request);
        assert_eq!(command.get_program(), program.as_os_str());
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["--version"]);
    }

    #[tokio::test]
    async fn test_host_observe_and_scheduler_do_not_spawn() {
        let host = test_host(root());
        host.observe()
            .observe(hofund_plugin_api::QuotaObservation::new(
                PluginId::from_static("codex"),
            ))
            .expect("observe");
        let spec = JobSpec::new("refresh", Duration::from_secs(60)).expect("job");
        let id = host
            .scheduler()
            .register(&spec, std::sync::Arc::new(|| {}))
            .expect("register");
        host.scheduler().cancel(id);
        host.scheduler().shutdown();
        let _ = surface_host(
            Vec::new(),
            vec![hofund_plugin_api::ProtocolId::OPENAI_CHAT],
            hofund_plugin_api::AuthResult::new(true),
        );
    }
}
