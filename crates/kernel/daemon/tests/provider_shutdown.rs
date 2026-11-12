//! Daemon exit calls [`UpstreamProvider::shutdown`] on every constructed
//! upstream so parked children are not orphaned.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use hofund_daemon::{DaemonRegistry, start_with_registry};
use hofund_plugin_api::{
    AgentDescription, BuildError, Completion, ConfigError, CredentialStyle, DynUpstreamFactory,
    HostContext, ListKind, PluginError, PluginId, ProtocolId, RawTable, RawValue, Request, Schema,
    UpstreamFactory, UpstreamProvider, UpstreamSpec, ValidConfig,
};

static SPEC: UpstreamSpec = UpstreamSpec::from_static(
    PluginId::from_static("openai-compat"),
    &[ProtocolId::OPENAI_CHAT],
    CredentialStyle::Bearer,
    ListKind::Unsupported,
);

#[derive(Default)]
struct ShutdownLog {
    calls: Mutex<Vec<Duration>>,
}

impl ShutdownLog {
    fn calls(&self) -> Vec<Duration> {
        self.calls.lock().expect("shutdown log").clone()
    }
}

struct RecordingConfig {
    fail: bool,
}

impl ValidConfig for RecordingConfig {}

struct RecordingFactory {
    log: Arc<ShutdownLog>,
}

impl UpstreamFactory for RecordingFactory {
    type Config = RecordingConfig;

    fn spec(&self) -> &'static UpstreamSpec {
        &SPEC
    }

    fn config_schema(&self) -> Schema {
        Schema::empty()
    }

    fn validate(&self, raw: &RawTable) -> Result<Self::Config, ConfigError> {
        let fail = raw.get("base_url").is_some_and(|value| match value {
            RawValue::String(base_url) => base_url.contains("fail"),
            _ => false,
        });
        Ok(RecordingConfig { fail })
    }

    fn build(
        &self,
        cfg: &Self::Config,
        _host: &HostContext,
    ) -> Result<Arc<dyn UpstreamProvider>, BuildError> {
        Ok(Arc::new(RecordingUpstream {
            log: Arc::clone(&self.log),
            fail: cfg.fail,
        }))
    }
}

struct RecordingUpstream {
    log: Arc<ShutdownLog>,
    fail: bool,
}

#[async_trait]
impl UpstreamProvider for RecordingUpstream {
    async fn execute(
        &self,
        _request: Request,
        _inbound: ProtocolId,
    ) -> Result<Completion, PluginError> {
        Err(PluginError::unsupported("recording provider"))
    }

    async fn shutdown(&self, deadline: Duration) -> Result<(), PluginError> {
        self.log.calls.lock().expect("shutdown log").push(deadline);
        if self.fail {
            Err(PluginError::internal("recording shutdown failed"))
        } else {
            Ok(())
        }
    }
}

struct RecordingRegistry {
    factory: Arc<RecordingFactory>,
}

impl DaemonRegistry for RecordingRegistry {
    fn factory(&self, id: &str) -> Option<Arc<dyn DynUpstreamFactory>> {
        (id == "openai-compat").then(|| Arc::clone(&self.factory) as Arc<dyn DynUpstreamFactory>)
    }

    fn is_kernel_factory(&self, id: &str) -> bool {
        id == "openai-compat"
    }

    fn builtin_factory_ids(&self) -> &'static [&'static str] {
        &["openai-compat"]
    }

    fn builtin_protocols(&self) -> Vec<ProtocolId> {
        vec![ProtocolId::OPENAI_CHAT]
    }

    fn find_preset(
        &self,
        _id: &str,
    ) -> Option<(&'static hofund_daemon::CompiledPreset, Option<&'static str>)> {
        None
    }

    fn agents(&self) -> &'static [&'static AgentDescription] {
        &[]
    }

    fn surfaces(&self) -> Vec<Arc<dyn hofund_plugin_api::InboundSurface>> {
        Vec::new()
    }
}

fn config_with(providers: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("directory");
    let path = dir.path().join("hofund.toml");
    let mut text = String::from("listen = \"127.0.0.1:0\"\n");
    let mut ids = Vec::new();
    for (id, base_url) in providers {
        text.push_str(&format!(
            "[[providers]]\nid = \"{id}\"\nprotocol = \"openai-chat\"\nbase_url = \"{base_url}\"\n"
        ));
        ids.push((*id).to_owned());
    }
    text.push_str("[[routes]]\nmodel = \"demo\"\nupstreams = [");
    text.push_str(
        &ids.iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(", "),
    );
    text.push_str("]\n");
    std::fs::write(&path, text).expect("config");
    (dir, path)
}

fn registry(log: &Arc<ShutdownLog>) -> Arc<RecordingRegistry> {
    Arc::new(RecordingRegistry {
        factory: Arc::new(RecordingFactory {
            log: Arc::clone(log),
        }),
    })
}

#[tokio::test]
async fn shutdown_is_called_on_every_constructed_upstream() {
    let log = Arc::new(ShutdownLog::default());
    let (_dir, path) = config_with(&[("one", "http://127.0.0.1:1"), ("two", "http://127.0.0.1:2")]);
    let running = start_with_registry(&path, registry(&log))
        .await
        .expect("start");
    assert!(
        log.calls().is_empty(),
        "shutdown must not run while the daemon is serving"
    );
    support::shutdown(running).await;
    let calls = log.calls();
    assert_eq!(
        calls.len(),
        2,
        "each constructed upstream must be shut down"
    );
    assert!(
        calls
            .iter()
            .all(|deadline| *deadline == Duration::from_secs(4)),
        "shutdown must receive the provider deadline, got {calls:?}"
    );
}

#[tokio::test]
async fn shutdown_still_finishes_when_one_provider_fails() {
    let log = Arc::new(ShutdownLog::default());
    let (_dir, path) = config_with(&[
        ("ok", "http://127.0.0.1:1"),
        ("bad", "http://127.0.0.1:2/fail"),
    ]);
    let running = start_with_registry(&path, registry(&log))
        .await
        .expect("start");
    support::shutdown(running).await;
    assert_eq!(
        log.calls().len(),
        2,
        "a failing provider must not skip the other constructed upstreams"
    );
}
