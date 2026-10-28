use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hofund_config::Config;
use hofund_plugin_api::MemberId;
use hofund_router::{CooldownPolicy, Cooldowns, Snapshot};

use crate::membership::{self, Table};
use crate::{DaemonError, DaemonRegistry, ModelsHost, providers, route_plan, router};

const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct Runtime {
    registry: Arc<dyn DaemonRegistry>,
    cooldowns: Arc<Cooldowns>,
    fingerprints: Mutex<HashMap<MemberId, String>>,
    path: PathBuf,
    max_idle_per_host: Option<usize>,
    table: OnceLock<Arc<Table>>,
}

impl Runtime {
    pub(crate) fn new(
        registry: Arc<dyn DaemonRegistry>,
        path: &Path,
        max_idle_per_host: Option<usize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            registry,
            cooldowns: Arc::new(Cooldowns::with_clock(
                CooldownPolicy::default(),
                Arc::new(|| tokio::time::Instant::now().into_std()),
            )),
            fingerprints: Mutex::new(HashMap::new()),
            path: path.to_path_buf(),
            max_idle_per_host,
            table: OnceLock::new(),
        })
    }

    pub(crate) fn initial_snapshot(
        &self,
        config: &Config,
        models_host: Option<&mut ModelsHost>,
    ) -> Result<Snapshot, DaemonError> {
        let (snapshot, table, fingerprints) = router(
            config,
            &self.path,
            self.max_idle_per_host,
            models_host,
            Arc::clone(&self.cooldowns),
            &self.registry,
        )?;
        *self.fingerprints.lock().expect("provider fingerprints") = fingerprints;
        self.table.set(Arc::clone(&table)).map_err(|_| {
            DaemonError::new("runtime initialized its membership table more than once")
        })?;
        let owner = table as Arc<dyn std::any::Any + Send + Sync>;
        Ok(snapshot.hold(owner))
    }

    #[allow(dead_code)]
    pub(crate) fn publish_candidate(&self, config: &Config) -> Result<(), DaemonError> {
        let built = providers(
            config,
            &self.path,
            self.max_idle_per_host,
            crate::Mode::Serve,
            None,
            Some(Arc::clone(&self.cooldowns)),
            &self.registry,
        )?;
        let fingerprints = built
            .iter()
            .map(|(id, provider)| (membership::member_id(id), provider.fingerprint.clone()))
            .collect::<HashMap<_, _>>();
        let (routes, policy) = route_plan(config);
        let table = self.table.get().expect("initial snapshot").clone();
        table.replace(&built, routes, policy)?;

        let members = table.member_ids().into_iter().collect::<HashSet<_>>();
        self.cooldowns.retain_members(&members);
        let mut previous = self.fingerprints.lock().expect("provider fingerprints");
        for (id, fingerprint) in &fingerprints {
            if previous.get(id).is_some_and(|old| old != fingerprint) {
                self.cooldowns.remove(id);
            }
        }
        *previous = fingerprints;
        Ok(())
    }

    pub(crate) fn spawn_maintenance(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let cooldowns = Arc::clone(&self.cooldowns);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                cooldowns.maintain();
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hofund_config::Config;
    use hofund_plugin_api::PluginError;

    use super::Runtime;
    use crate::membership::member_id;
    use crate::{RegistryAdapter, provider_fingerprint};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn config(ids: &[&str]) -> Config {
        let providers = ids
            .iter()
            .map(|id| {
                format!(
                    "[[providers]]\nid = \"{id}\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"first-key\"\n"
                )
            })
            .collect::<String>();
        let upstreams = ids
            .iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(", ");
        hofund_config::load(&format!(
            "{providers}[[routes]]\nmodel = \"demo\"\nupstreams = [{upstreams}]\n"
        ))
        .expect("config")
    }

    fn runtime(config: &Config) -> Arc<Runtime> {
        let path = std::env::temp_dir().join(format!(
            "hofund-runtime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("runtime directory");
        let runtime = Runtime::new(
            Arc::new(RegistryAdapter),
            &path.join("hofund.toml"),
            Some(0),
        );
        runtime.initial_snapshot(config, None).expect("snapshot");
        runtime
    }

    fn cool(runtime: &Runtime, id: &str) {
        runtime
            .cooldowns
            .record_failure(&member_id(id), &PluginError::connect("down"));
    }

    #[test]
    fn cooldown_survives_repeated_publication() {
        let config = config(&["member"]);
        let runtime = runtime(&config);
        cool(&runtime, "member");
        for _ in 0..4 {
            runtime.table.get().expect("table").publish();
            assert!(
                runtime.cooldowns.cooling(&member_id("member")).is_some(),
                "membership publication replaced the cooldown book"
            );
        }
    }

    #[test]
    fn cooldown_survives_candidate_reload() {
        let config = config(&["member"]);
        let runtime = runtime(&config);
        cool(&runtime, "member");
        runtime.publish_candidate(&config).expect("candidate");
        assert!(runtime.cooldowns.cooling(&member_id("member")).is_some());
    }

    #[test]
    fn removed_member_is_pruned_on_reload() {
        let original = config(&["kept", "removed"]);
        let runtime = runtime(&original);
        cool(&runtime, "kept");
        cool(&runtime, "removed");
        runtime
            .publish_candidate(&config(&["kept"]))
            .expect("candidate");
        assert!(runtime.cooldowns.cooling(&member_id("kept")).is_some());
        assert_eq!(runtime.cooldowns.revision(&member_id("removed")), 0);
    }

    #[test]
    fn rejected_candidate_does_not_prune() {
        let original = config(&["member"]);
        let runtime = runtime(&original);
        cool(&runtime, "member");
        let mut rejected = original.clone();
        rejected.routes[0].upstreams[0].provider = "missing".to_owned();
        runtime
            .publish_candidate(&rejected)
            .expect_err("unknown route provider");
        assert!(runtime.cooldowns.cooling(&member_id("member")).is_some());
    }

    #[test]
    fn provider_fingerprint_is_secret_free_and_changes_for_key_and_base_url() {
        let key = "secret-key-value";
        let original = provider_fingerprint(key, "http://127.0.0.1:9");
        assert_eq!(original, provider_fingerprint(key, "http://127.0.0.1:9"));
        assert_ne!(
            original,
            provider_fingerprint("another-key", "http://127.0.0.1:9")
        );
        assert_ne!(original, provider_fingerprint(key, "http://127.0.0.1:10"));
        let hexadecimal = key
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        for encoded in [key.to_owned(), base64(key.as_bytes()), hexadecimal] {
            assert!(!original.contains(&encoded));
        }
    }

    #[test]
    fn provider_fingerprint_change_clears_only_that_cooldown() {
        let original = config(&["first", "second"]);
        let runtime = runtime(&original);
        cool(&runtime, "first");
        cool(&runtime, "second");
        runtime.publish_candidate(&original).expect("baseline");
        assert!(runtime.cooldowns.cooling(&member_id("first")).is_some());
        assert!(runtime.cooldowns.cooling(&member_id("second")).is_some());

        let mut changed = original;
        changed.providers[0].api_key = "changed-key".to_owned();
        runtime.publish_candidate(&changed).expect("changed");
        assert_eq!(runtime.cooldowns.cooling(&member_id("first")), None);
        assert!(runtime.cooldowns.cooling(&member_id("second")).is_some());
    }

    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let value = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for index in 0..4 {
                if index > chunk.len() {
                    out.push('=');
                } else {
                    let shift = 18 - index * 6;
                    out.push(ALPHABET[((value >> shift) & 63) as usize] as char);
                }
            }
        }
        out
    }
}
