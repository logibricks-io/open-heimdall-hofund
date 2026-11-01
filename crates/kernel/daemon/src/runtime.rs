use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use hofund_config::Config;
use hofund_control_proto::{EventPayload, MemberCooledEvent, MemberRecoveredEvent};
use hofund_plugin_api::MemberId;
use hofund_router::quota::QuotaStore;
use hofund_router::{CooldownPolicy, Cooldowns, FailoverPolicy, Snapshot};
use tokio::sync::watch;

use crate::control_map;
use crate::events::EventBus;
use crate::membership::{self, Table};
use crate::rests::RestStore;
use crate::subscriptions::{self, QuotaPolls, Subscriptions};
use crate::{
    Built, DaemonError, DaemonRegistry, Instance, ModelsHost, RouterParts, providers, route_plan,
    router,
};

/// Cooldown observer tick. One second keeps cooled/recovered events prompt
/// without busy-waiting on the book.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
/// How long a fresh quota report waits on the vendors before answering
/// what is kept.
pub(crate) const QUOTA_READ_TIMEOUT: Duration = Duration::from_secs(12);

/// Providers and routes built from a candidate configuration, not yet live.
pub(crate) struct Candidate {
    config: Arc<Config>,
    built: BTreeMap<String, membership::BuiltProvider>,
    instances: HashMap<String, Instance>,
    fingerprints: HashMap<MemberId, String>,
    routes: BTreeMap<String, Vec<membership::PlannedHop>>,
    policy: FailoverPolicy,
    subscriptions: Subscriptions,
}

/// The published providers and routes before subscription expansion, kept so
/// an account change republishes without rebuilding providers.
#[derive(Default)]
struct Plan {
    built: BTreeMap<String, membership::BuiltProvider>,
    routes: BTreeMap<String, Vec<membership::PlannedHop>>,
    policy: FailoverPolicy,
    subscriptions: Subscriptions,
}

pub(crate) struct Runtime {
    registry: Arc<dyn DaemonRegistry>,
    cooldowns: Arc<Cooldowns>,
    rests: Arc<Mutex<RestStore>>,
    fingerprints: Mutex<HashMap<MemberId, String>>,
    path: PathBuf,
    max_idle_per_host: Option<usize>,
    table: OnceLock<Arc<Table>>,
    /// Built instances by provider id; a reload reuses an unchanged one.
    instances: Mutex<HashMap<String, Instance>>,
    /// The list and probe host, attached once it is shared.
    models: OnceLock<Arc<ModelsHost>>,
    /// The published configuration, for tasks that follow reloads.
    config: OnceLock<watch::Sender<Arc<Config>>>,
    /// Optional control bus for cooldown membership events.
    events: OnceLock<Arc<EventBus>>,
    plan: Mutex<Plan>,
    polls: tokio::sync::Mutex<QuotaPolls>,
    quotas: Arc<QuotaStore>,
    affinity: hofund_router::affinity::AffinityState,
    /// Wakes the subscription task after a publication.
    wake: tokio::sync::Notify,
}

impl Candidate {
    pub(crate) fn config(&self) -> &Arc<Config> {
        &self.config
    }
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
            rests: Arc::new(Mutex::new(RestStore::new(path))),
            fingerprints: Mutex::new(HashMap::new()),
            path: path.to_path_buf(),
            max_idle_per_host,
            table: OnceLock::new(),
            instances: Mutex::new(HashMap::new()),
            models: OnceLock::new(),
            config: OnceLock::new(),
            events: OnceLock::new(),
            plan: Mutex::new(Plan::default()),
            polls: tokio::sync::Mutex::new(QuotaPolls::default()),
            quotas: Arc::new(QuotaStore::new(
                path.parent().unwrap_or_else(|| Path::new(".")),
            )),
            affinity: hofund_router::affinity::AffinityState::new(
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("affinity.json"),
            ),
            wake: tokio::sync::Notify::new(),
        })
    }

    pub(crate) fn initial_snapshot(
        &self,
        config: &Config,
        models_host: Option<&mut ModelsHost>,
    ) -> Result<Snapshot, DaemonError> {
        let subscriptions = subscriptions::build(
            config,
            &self.path,
            self.max_idle_per_host,
            &self.registry,
            None,
        )?;
        let RouterParts {
            snapshot,
            table,
            fingerprints,
            instances,
            built,
            routes,
            policy,
        } = router(
            config,
            &self.path,
            self.max_idle_per_host,
            models_host,
            &self.registry,
            &subscriptions,
            (
                Arc::clone(&self.cooldowns),
                self.affinity.clone(),
                Arc::clone(&self.quotas),
            ),
        )?;
        let configured = fingerprints.keys().cloned().collect::<HashSet<_>>();
        *self.plan.lock().expect("published plan") = Plan {
            built,
            routes,
            policy,
            subscriptions,
        };
        *self.fingerprints.lock().expect("provider fingerprints") = fingerprints;
        *self.instances.lock().expect("provider instances") = instances;
        self.rests
            .lock()
            .expect("credit rest store")
            .load(&self.cooldowns, &configured);
        self.config
            .set(watch::Sender::new(Arc::new(config.clone())))
            .map_err(|_| DaemonError::new("runtime published its first configuration twice"))?;
        self.table.set(Arc::clone(&table)).map_err(|_| {
            DaemonError::new("runtime initialized its membership table more than once")
        })?;
        let owner = table as Arc<dyn std::any::Any + Send + Sync>;
        Ok(snapshot.hold(owner))
    }

    /// Shares the list and probe host so reloads plan through it and
    /// replace its instances.
    pub(crate) fn attach_models(&self, host: Arc<ModelsHost>) {
        let _ = self.models.set(host);
    }

    /// Emits member cooldown events on the shared control bus when set.
    pub(crate) fn attach_events(&self, bus: Arc<EventBus>) {
        let _ = self.events.set(bus);
    }

    /// Follows the published configuration.
    pub(crate) fn config_updates(&self) -> watch::Receiver<Arc<Config>> {
        self.config.get().expect("initial snapshot").subscribe()
    }

    /// Builds the providers for `config` without publishing anything.
    ///
    /// A provider whose instance identity is unchanged reuses the running
    /// instance, so its connection pool and learned state carry over.
    pub(crate) fn prepare(&self, config: &Config) -> Result<Candidate, DaemonError> {
        let reuse = self.instances.lock().expect("provider instances").clone();
        let Built { built, instances } = providers(
            config,
            &self.path,
            self.max_idle_per_host,
            crate::Mode::Serve,
            self.models.get().map(AsRef::as_ref),
            Some(&reuse),
            Some(Arc::clone(&self.cooldowns)),
            &self.registry,
        )?;
        drop(reuse);
        let previous = self
            .plan
            .lock()
            .expect("published plan")
            .subscriptions
            .clone();
        let subscriptions = subscriptions::build(
            config,
            &self.path,
            self.max_idle_per_host,
            &self.registry,
            Some(&previous),
        )?;
        drop(previous);
        let mut fingerprints = HashMap::new();
        let mut owners: HashMap<MemberId, &str> = HashMap::new();
        for (id, provider) in &built {
            let member = membership::member_id(id);
            // Publication rejects two providers that share a member id; check
            // here so the candidate fails before anything is written.
            if let Some(previous) = owners.insert(member.clone(), id) {
                return Err(DaemonError::new(format!(
                    "member id `{member}` is shared by `{previous}` and `{id}`"
                )));
            }
            fingerprints.insert(member, provider.fingerprint.clone());
        }
        let (routes, policy) = route_plan(config, self.models.get().map(AsRef::as_ref));
        // Publication cannot fail after the file is written: every routed
        // provider must already be built.
        for (model, hops) in &routes {
            for hop in hops {
                match &hop.subscription {
                    Some(id) if !subscriptions.contains_key(id) => {
                        return Err(DaemonError::new(format!(
                            "route `{model}` names unknown subscription `{id}`"
                        )));
                    }
                    Some(_) => {}
                    None if !built.contains_key(&hop.provider) => {
                        return Err(DaemonError::new(format!(
                            "route `{model}` names unknown provider `{}`",
                            hop.provider
                        )));
                    }
                    None => {}
                }
            }
        }
        Ok(Candidate {
            config: Arc::new(config.clone()),
            built,
            instances,
            fingerprints,
            routes,
            policy,
            subscriptions,
        })
    }

    /// Publishes a prepared candidate and returns the ids of members that were
    /// added, removed, or whose fingerprint changed, sorted.
    pub(crate) fn publish(&self, candidate: Candidate) -> Result<Vec<String>, DaemonError> {
        let Candidate {
            config,
            built,
            instances,
            fingerprints,
            routes,
            policy,
            subscriptions,
        } = candidate;
        let plan = Plan {
            built,
            routes,
            policy,
            subscriptions,
        };
        let mut published = self.plan.lock().expect("published plan");
        self.apply(&plan)?;
        let subscription_members = subscriptions::member_ids(&plan.subscriptions);
        *published = plan;
        drop(published);
        if let Some(host) = self.models.get() {
            host.replace_instances(instances.iter().map(|(id, instance)| {
                (
                    id.clone(),
                    Arc::clone(&instance.upstream),
                    instance.base_url.clone(),
                )
            }));
        }
        // Retired instances drop here unless a request still holds them.
        *self.instances.lock().expect("provider instances") = instances;
        if let Some(sender) = self.config.get() {
            sender.send_replace(config);
        }

        // Published routes omit failed members; every configured candidate
        // member must retain its rest regardless of that transient flag.
        let mut members = fingerprints.keys().cloned().collect::<HashSet<_>>();
        members.extend(subscription_members);
        self.cooldowns.retain_members(&members);
        let mut previous = self.fingerprints.lock().expect("provider fingerprints");
        let mut changed = Vec::new();
        for (id, fingerprint) in &fingerprints {
            match previous.get(id) {
                Some(old) if old == fingerprint => {}
                Some(_) => {
                    self.cooldowns.remove(id);
                    changed.push(id.to_string());
                }
                None => changed.push(id.to_string()),
            }
        }
        changed.extend(
            previous
                .keys()
                .filter(|id| !fingerprints.contains_key(*id))
                .map(ToString::to_string),
        );
        changed.sort();
        *previous = fingerprints;
        drop(previous);
        self.wake.notify_one();
        Ok(changed)
    }

    /// Expands `plan` and replaces the membership table.
    fn apply(&self, plan: &Plan) -> Result<(), DaemonError> {
        let (built, routes) =
            subscriptions::expand(&plan.built, &plan.routes, &plan.subscriptions)?;
        let table = self.table.get().expect("initial snapshot").clone();
        table.replace(&built, routes, plan.policy.clone())
    }

    /// Rereads every subscription's accounts and republishes when a member
    /// list changed. Returns whether one did.
    pub(crate) async fn refresh_subscriptions(&self) -> bool {
        let current = self
            .plan
            .lock()
            .expect("published plan")
            .subscriptions
            .clone();
        let mut changed = false;
        for state in current.values() {
            changed |= state.resolve(&self.cooldowns).await;
        }
        if changed {
            let plan = self.plan.lock().expect("published plan");
            if let Err(error) = self.apply(&plan) {
                tracing::warn!(%error, "subscription members were not republished");
                return false;
            }
            let mut members = self
                .fingerprints
                .lock()
                .expect("provider fingerprints")
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            members.extend(subscriptions::member_ids(&plan.subscriptions));
            self.cooldowns.retain_members(&members);
        }
        changed
    }

    /// Polls quota for each subscription member that is due.
    pub(crate) async fn poll_quotas(&self, now: tokio::time::Instant) -> Vec<MemberId> {
        self.poll_quotas_with(now, false).await
    }

    async fn poll_quotas_with(&self, now: tokio::time::Instant, force: bool) -> Vec<MemberId> {
        let current = self
            .plan
            .lock()
            .expect("published plan")
            .subscriptions
            .clone();
        self.polls
            .lock()
            .await
            .poll(&current, &self.cooldowns, &self.quotas, now, force)
            .await
    }

    /// Returns the kept report, or reads every subscription first. A fresh
    /// read gives up after [`QUOTA_READ_TIMEOUT`] and answers what is kept.
    pub(crate) async fn quota_report(
        &self,
        kept: bool,
    ) -> (
        Vec<hofund_control_proto::Quota>,
        Vec<hofund_control_proto::QuotaStanding>,
    ) {
        if !kept {
            let _ = tokio::time::timeout(
                QUOTA_READ_TIMEOUT,
                self.poll_quotas_with(tokio::time::Instant::now(), true),
            )
            .await;
        }
        let order = self.quota_order();
        let now = self.quotas.now();
        (
            self.quotas.report(&order),
            self.quotas.standings(&order, now),
        )
    }

    /// Providers then subscriptions, as configured.
    fn quota_order(&self) -> Vec<String> {
        self.config
            .get()
            .map(|config| {
                let config = config.subscribe();
                let config = config.borrow();
                config
                    .providers
                    .iter()
                    .map(|provider| provider.id.clone())
                    .chain(config.subscriptions.iter().map(|entry| entry.id.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// The quota store's wall clock.
    pub(crate) fn quota_now(&self) -> std::time::SystemTime {
        self.quotas.now()
    }

    /// Returns sampled history from the shared store.
    pub(crate) fn quota_histories(
        &self,
        days: Option<&str>,
        provider: Option<&str>,
        user: Option<&str>,
    ) -> Vec<hofund_control_proto::QuotaHistory> {
        self.quotas
            .histories(days, provider, user, self.quotas.now())
    }

    /// Rereads accounts every tick and after each publication, then polls
    /// quota for members that are due.
    pub(crate) fn spawn_subscriptions(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let runtime = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(subscriptions::ACCOUNT_TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    () = runtime.wake.notified() => {}
                }
                runtime.refresh_subscriptions().await;
                runtime.poll_quotas(tokio::time::Instant::now()).await;
            }
        })
    }

    pub(crate) fn affinity(&self) -> hofund_router::affinity::AffinityState {
        self.affinity.clone()
    }

    #[cfg(test)]
    pub(crate) fn cooldown_book(&self) -> &Arc<Cooldowns> {
        &self.cooldowns
    }

    #[cfg(test)]
    pub(crate) fn publish_candidate(&self, config: &Config) -> Result<Vec<String>, DaemonError> {
        let candidate = self.prepare(config)?;
        self.publish(candidate)
    }

    pub(crate) fn spawn_maintenance(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let cooldowns = Arc::clone(&self.cooldowns);
        let rests = Arc::clone(&self.rests);
        let events = self.events.get().cloned();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Last observed cooling revision per member/model; a rise means cooled.
            let mut last_revisions: HashMap<(MemberId, Option<String>), u64> = HashMap::new();
            loop {
                interval.tick().await;
                cooldowns.maintain();
                rests.lock().expect("credit rest store").save(&cooldowns);
                let recoveries = cooldowns.drain_scoped_recoveries();
                if let Some(bus) = &events {
                    for (id, model, _cause) in &recoveries {
                        let mut event = MemberRecoveredEvent::default();
                        event.member_id = id.to_string();
                        event.model.clone_from(model);
                        event.ready = true;
                        bus.emit(EventPayload::MemberRecovered(event));
                        last_revisions.remove(&(id.clone(), model.clone()));
                    }
                    let now = std::time::Instant::now();
                    for (id, model, cooling) in cooldowns.scoped_entries() {
                        let revision = model.as_deref().map_or_else(
                            || cooldowns.revision(&id),
                            |model| cooldowns.revision_for_model(&id, model),
                        );
                        let previous = last_revisions.insert((id.clone(), model.clone()), revision);
                        if previous == Some(revision) {
                            continue;
                        }
                        let remaining =
                            cooling.until().checked_duration_since(now).map(|duration| {
                                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
                            });
                        let mut event = MemberCooledEvent::default();
                        event.member_id = id.to_string();
                        event.model = model;
                        event.cause = control_map::cooldown_reason(cooling.cause());
                        event.remaining_ms = remaining;
                        event.retry_after_ms = remaining;
                        event.failures = cooling.failures();
                        event.status = cooling.status();
                        event.error = cooling.error().to_owned();
                        bus.emit(EventPayload::MemberCooled(event));
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hofund_config::Config;
    use hofund_plugin_api::PluginError;

    use super::Runtime;
    use crate::RegistryAdapter;
    use crate::membership::member_id;

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

    fn runtime(config: &Config) -> (Arc<Runtime>, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("runtime directory");
        let runtime = Runtime::new(
            Arc::new(RegistryAdapter),
            &root.path().join("hofund.toml"),
            Some(0),
        );
        runtime.initial_snapshot(config, None).expect("snapshot");
        (runtime, root)
    }

    #[test]
    fn prepare_rejects_a_route_to_a_provider_it_did_not_build() {
        let config = config(&["first", "second"]);
        let (runtime, _root) = runtime(&config);
        runtime.prepare(&config).expect("valid candidate");
        let mut orphaned = config.clone();
        orphaned
            .providers
            .retain(|provider| provider.id != "second");
        let error = runtime.prepare(&orphaned).err().expect("orphaned route");
        assert!(
            error.to_string().contains("unknown provider `second`"),
            "{error}"
        );
    }

    fn fingerprint(config: &Config, index: usize) -> String {
        let provider = &config.providers[index];
        let planned = crate::plan(provider).expect("plan");
        let endpoint = crate::endpoint_for(provider, &planned).expect("endpoint");
        crate::provider_fingerprint(provider, &endpoint).expect("fingerprint")
    }

    fn cool(runtime: &Runtime, id: &str) {
        runtime
            .cooldowns
            .record_failure(&member_id(id), &PluginError::connect("down"));
    }

    #[test]
    fn cooldown_survives_repeated_publication() {
        let config = config(&["member"]);
        let (runtime, _root) = runtime(&config);
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
        let (runtime, _root) = runtime(&config);
        cool(&runtime, "member");
        runtime.publish_candidate(&config).expect("candidate");
        assert!(runtime.cooldowns.cooling(&member_id("member")).is_some());
    }

    #[test]
    fn removed_member_is_pruned_on_reload() {
        let original = config(&["kept", "removed"]);
        let (runtime, _root) = runtime(&original);
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
        let (runtime, _root) = runtime(&original);
        cool(&runtime, "member");
        let mut rejected = original.clone();
        rejected.routes[0].upstreams[0].target =
            hofund_config::UpstreamTarget::Provider("missing".to_owned());
        runtime
            .publish_candidate(&rejected)
            .expect_err("unknown route provider");
        assert!(runtime.cooldowns.cooling(&member_id("member")).is_some());
    }

    fn aws_config() -> Config {
        let providers = ["first", "second"]
            .iter()
            .map(|id| {
                format!(
                    "[[providers]]\nid = \"{id}\"\nprotocol = \"bedrock-converse\"\nbase_url = \"https://bedrock-runtime.us-east-1.amazonaws.com\"\n[providers.options]\nregion = \"us-east-1\"\naccess_key_id = \"access-{id}\"\nsecret_access_key = \"secret-{id}\"\nsession_token = \"session-{id}\"\n"
                )
            })
            .collect::<String>();
        hofund_config::load(&format!(
            "{providers}[[routes]]\nmodel = \"demo\"\nupstreams = [\"first\", \"second\"]\n"
        ))
        .expect("AWS config")
    }

    fn assert_secret_free(digest: &str, secrets: &[&str]) {
        for secret in secrets {
            let hexadecimal = secret
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            for encoded in [(*secret).to_owned(), base64(secret.as_bytes()), hexadecimal] {
                assert!(!digest.contains(&encoded));
            }
        }
    }

    #[test]
    fn provider_fingerprint_is_secret_free_and_changes_for_key_and_base_url() {
        let original = config(&["member"]);
        let digest = fingerprint(&original, 0);
        assert_eq!(digest, fingerprint(&original, 0));
        let mut changed = original.clone();
        changed.providers[0].api_key = "another-key".to_owned();
        assert_ne!(digest, fingerprint(&changed, 0));
        let mut changed = original.clone();
        changed.providers[0].base_url = "http://127.0.0.1:10".to_owned();
        assert_ne!(digest, fingerprint(&changed, 0));
        let mut changed = original;
        changed.providers[0]
            .headers
            .insert("x-auth-token".to_owned(), "header-secret".to_owned());
        assert_ne!(digest, fingerprint(&changed, 0));
        assert_secret_free(&digest, &["first-key"]);
    }

    #[test]
    fn provider_fingerprint_covers_sigv4_secrets_and_clears_only_changed_member() {
        for option in ["secret_access_key", "session_token"] {
            let original = aws_config();
            let digest = fingerprint(&original, 0);
            assert_secret_free(&digest, &["access-first", "secret-first", "session-first"]);
            let mut changed = original.clone();
            changed.providers[0].options.insert(
                option.to_owned(),
                hofund_config::Value::String("rotated-secret".to_owned()),
            );
            let changed_digest = fingerprint(&changed, 0);
            assert_ne!(digest, changed_digest);
            assert_secret_free(&changed_digest, &["rotated-secret"]);

            let (runtime, _root) = runtime(&original);
            cool(&runtime, "first");
            cool(&runtime, "second");
            runtime.publish_candidate(&changed).expect("changed");
            assert_eq!(runtime.cooldowns.cooling(&member_id("first")), None);
            assert!(runtime.cooldowns.cooling(&member_id("second")).is_some());
        }
    }

    #[test]
    fn provider_fingerprint_change_clears_only_that_cooldown() {
        let original = config(&["first", "second"]);
        let (runtime, _root) = runtime(&original);
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

    fn adapter(runtime: &Runtime, id: &str) -> Arc<dyn hofund_provider::Provider> {
        Arc::clone(&runtime.instances.lock().expect("instances")[id].adapter)
    }

    fn hosted(config: &Config) -> (Arc<Runtime>, Arc<crate::ModelsHost>, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("runtime directory");
        let runtime = Runtime::new(
            Arc::new(RegistryAdapter),
            &root.path().join("hofund.toml"),
            Some(0),
        );
        let mut host = crate::ModelsHost::new(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        runtime
            .initial_snapshot(config, Some(&mut host))
            .expect("snapshot");
        let host = Arc::new(host);
        runtime.attach_models(Arc::clone(&host));
        (runtime, host, root)
    }

    #[test]
    fn unchanged_fingerprint_reuses_instance_and_preserves_learned_routes() {
        let config = config(&["first", "second"]);
        let (runtime, host, _root) = hosted(&config);
        let first = adapter(&runtime, "first");
        let upstream = host.instance("first").expect("registered");
        cool(&runtime, "first");
        let changed = runtime.publish_candidate(&config).expect("reload");
        assert!(changed.is_empty(), "{changed:?}");
        assert!(Arc::ptr_eq(&first, &adapter(&runtime, "first")));
        assert!(Arc::ptr_eq(
            &upstream,
            &host.instance("first").expect("still registered")
        ));
        assert!(runtime.cooldowns.cooling(&member_id("first")).is_some());
    }

    #[test]
    fn changed_fingerprint_rebuilds_instance() {
        let original = config(&["first", "second"]);
        let (runtime, host, _root) = hosted(&original);
        let first = adapter(&runtime, "first");
        let second = adapter(&runtime, "second");
        let upstream = host.instance("first").expect("registered");

        let mut changed = original.clone();
        changed.providers[0].api_key = "changed-key".to_owned();
        let ids = runtime.publish_candidate(&changed).expect("reload");
        assert_eq!(ids, [member_id("first").to_string()]);
        assert!(!Arc::ptr_eq(&first, &adapter(&runtime, "first")));
        assert!(Arc::ptr_eq(&second, &adapter(&runtime, "second")));
        assert!(!Arc::ptr_eq(
            &upstream,
            &host.instance("first").expect("re-registered")
        ));

        // A setting outside the fingerprint still rebuilds the instance, but
        // keeps the member's identity and its cooldown.
        let rebuilt = adapter(&runtime, "first");
        cool(&runtime, "first");
        let mut capped = changed;
        capped.providers[0].max_output_tokens = Some(64);
        assert!(
            runtime
                .publish_candidate(&capped)
                .expect("reload")
                .is_empty()
        );
        assert!(!Arc::ptr_eq(&rebuilt, &adapter(&runtime, "first")));
        assert!(runtime.cooldowns.cooling(&member_id("first")).is_some());

        let mut slower = capped;
        slower.read_timeout += std::time::Duration::from_secs(1);
        let before = adapter(&runtime, "second");
        runtime.publish_candidate(&slower).expect("reload");
        assert!(!Arc::ptr_eq(&before, &adapter(&runtime, "second")));
    }

    #[test]
    fn long_stream_finishes_before_retired_instance_shutdown() {
        let original = config(&["member"]);
        let (runtime, host, _root) = hosted(&original);
        // A request in flight holds the adapter it was routed to.
        let in_flight = adapter(&runtime, "member");
        let retired = Arc::downgrade(&in_flight);
        let mut changed = original;
        changed.providers[0].api_key = "changed-key".to_owned();
        runtime.publish_candidate(&changed).expect("reload");
        assert!(!Arc::ptr_eq(&in_flight, &adapter(&runtime, "member")));
        assert!(host.instance("member").is_some());
        assert!(
            retired.upgrade().is_some(),
            "the retired instance must outlive its in-flight request"
        );
        drop(in_flight);
        assert!(
            retired.upgrade().is_none(),
            "the retired instance must drop with its last request"
        );
    }

    #[tokio::test]
    async fn published_configuration_reaches_followers() {
        let original = config(&["member"]);
        let (runtime, _root) = runtime(&original);
        let mut updates = runtime.config_updates();
        assert_eq!(updates.borrow_and_update().providers.len(), 1);
        let next = config(&["member", "added"]);
        runtime.publish_candidate(&next).expect("reload");
        assert!(updates.has_changed().expect("sender alive"));
        assert_eq!(updates.borrow_and_update().providers.len(), 2);
        // A candidate that is only prepared is not published.
        runtime.prepare(&original).expect("candidate");
        assert!(!updates.has_changed().expect("sender alive"));
    }

    #[tokio::test(start_paused = true)]
    async fn maintenance_task_recovers_expired_rest_without_reads() {
        let config = config(&["member"]);
        let (runtime, _root) = runtime(&config);
        cool(&runtime, "member");
        let task = runtime.spawn_maintenance();
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        // The maintenance task drains recoveries itself so the book is clear.
        assert_eq!(runtime.cooldowns.cooling(&member_id("member")), None);
        assert!(runtime.cooldowns.drain_recoveries().is_empty());
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn maintenance_emits_member_cooled_and_recovered() {
        use crate::events::EventBus;
        use hofund_control_proto::{EventEnvelope, EventPayload, decode_response};

        let config = config(&["member"]);
        let (runtime, _root) = runtime(&config);
        let bus = Arc::new(EventBus::with_boot_id(
            "aabbccddeeff00112233445566778899".into(),
        ));
        runtime.attach_events(Arc::clone(&bus));
        cool(&runtime, "member");
        let task = runtime.spawn_maintenance();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        let cooled = bus
            .control_frames()
            .into_iter()
            .map(|frame| {
                decode_response::<EventEnvelope>(
                    hofund_control_proto::API_VERSION,
                    frame.as_bytes(),
                )
                .expect("env")
                .payload
            })
            .find(|payload| matches!(payload, EventPayload::MemberCooled(_)))
            .expect("member_cooled");
        match cooled {
            EventPayload::MemberCooled(event) => {
                assert_eq!(event.member_id, member_id("member").to_string());
                assert!(event.failures >= 1);
            }
            _ => unreachable!(),
        }
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        let recovered = bus
            .control_frames()
            .into_iter()
            .map(|frame| {
                decode_response::<EventEnvelope>(
                    hofund_control_proto::API_VERSION,
                    frame.as_bytes(),
                )
                .expect("env")
                .payload
            })
            .any(|payload| matches!(payload, EventPayload::MemberRecovered(_)));
        assert!(recovered, "expected member_recovered after expiry");
        assert_eq!(runtime.cooldowns.cooling(&member_id("member")), None);
        task.abort();
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
