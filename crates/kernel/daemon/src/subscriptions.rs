//! Subscription accounts as route members.
//!
//! Each `[[subscriptions]]` entry builds one plugin instance. Its signed-in
//! accounts become route members with ids from
//! [`MemberId::from_parts`]. A route hop that names the subscription expands to
//! one hop per account, ordered by label and then id. A periodic tick rereads
//! the account list and republishes when it changes, and polls quota so an
//! exhausted blocking window cools its member before a request fails.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hofund_config::{Accounts, Config};
use hofund_control_proto::{Quota, QuotaKind};
use hofund_plugin_api::{
    AccountId, AccountStatus, HostContext, MemberId, PluginError, PluginErrorKind, PluginId,
    PluginKind, QuotaObservation, QuotaWindow, RawTable, Subscription, UpstreamProvider,
};
use hofund_plugin_host::HttpLimits;
use hofund_router::quota::{QuotaStore, WindowInfo};
use hofund_router::Cooldowns;
use tokio::time::Instant;

use crate::membership::{self, BuiltProvider, PlannedHop};
use crate::{DaemonError, DaemonRegistry, adapt, beside, raw_value, show};

/// How often the account list is reread.
pub(crate) const ACCOUNT_TICK: Duration = Duration::from_secs(5);

/// Base quota poll interval per member, before jitter.
pub(crate) const QUOTA_POLL: Duration = Duration::from_secs(5 * 60);

/// Per-call deadline for subscription members, the provider default.
const MEMBER_TIMEOUT: Duration = Duration::from_secs(60);

/// Protocol label on subscription member spans.
const MEMBER_PROTOCOL: &str = "subscription";

/// The built map with subscription members added, and the expanded routes.
pub(crate) type Expanded = (
    BTreeMap<String, BuiltProvider>,
    BTreeMap<String, Vec<PlannedHop>>,
);

/// Built subscriptions keyed by config id.
pub(crate) type Subscriptions = BTreeMap<String, Arc<SubscriptionState>>;

/// One configured subscription and its current members.
pub(crate) struct SubscriptionState {
    config: hofund_config::Subscription,
    plugin: PluginId,
    instance: Arc<dyn Subscription>,
    host: HostContext,
    root: PathBuf,
    limits: HttpLimits,
    timeouts: (Duration, Duration),
    members: Mutex<Vec<SubscriptionMember>>,
}

/// One account serving as a route member.
#[derive(Clone)]
pub(crate) struct SubscriptionMember {
    pub(crate) account: AccountId,
    pub(crate) id: MemberId,
    pub(crate) built: BuiltProvider,
    /// The plugin upstream, kept for tests that call it directly.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) upstream: Arc<dyn UpstreamProvider>,
}

impl SubscriptionMember {
    /// Built-map key and route id. It reads as the member id.
    pub(crate) fn key(&self) -> String {
        self.id.to_string()
    }
}

/// Builds each configured subscription. A subscription whose config is
/// unchanged from `previous` keeps its instance and members.
///
/// # Errors
///
/// Returns [`DaemonError`] when no linked plugin has the subscription id or
/// the plugin rejects its options.
pub(crate) fn build(
    config: &Config,
    path: &Path,
    max_idle_per_host: Option<usize>,
    registry: &Arc<dyn DaemonRegistry>,
    previous: Option<&Subscriptions>,
) -> Result<Subscriptions, DaemonError> {
    let mut limits = HttpLimits::new(config.connect_timeout, config.read_timeout);
    if let Some(max_idle_per_host) = max_idle_per_host {
        limits = limits.with_max_idle_per_host(max_idle_per_host);
    }
    let timeouts = (config.connect_timeout, config.read_timeout);
    let root = beside(path, ".hofund");
    let mut built = BTreeMap::new();
    for subscription in &config.subscriptions {
        if let Some(state) = previous.and_then(|map| map.get(&subscription.id))
            && state.config == *subscription
            && state.root == root
            && state.timeouts == timeouts
        {
            built.insert(subscription.id.clone(), Arc::clone(state));
            continue;
        }
        let factory = registry.subscription(&subscription.id).ok_or_else(|| {
            DaemonError::new(format!(
                "no subscription plugin `{}` is linked",
                subscription.id
            ))
        })?;
        let plugin = PluginId::try_from(subscription.id.as_str()).map_err(show)?;
        let mut raw = RawTable::new();
        for (key, value) in &subscription.options {
            raw.insert(
                key.clone(),
                raw_value(
                    value,
                    Some(("subscription", subscription.id.as_str(), key.as_str())),
                )?,
            );
        }
        let host = hofund_plugin_host::runtime_host_for(
            PluginKind::Subscription,
            plugin.clone(),
            String::new(),
            root.clone(),
            limits,
        )
        .map_err(show)?;
        let instance = factory.validate_and_build(&raw, &host).map_err(|error| {
            DaemonError::new(format!("subscription `{}`: {error}", subscription.id))
        })?;
        built.insert(
            subscription.id.clone(),
            Arc::new(SubscriptionState {
                config: subscription.clone(),
                plugin,
                instance,
                host,
                root: root.clone(),
                limits,
                timeouts,
                members: Mutex::new(Vec::new()),
            }),
        );
    }
    Ok(built)
}

impl SubscriptionState {
    /// The built plugin instance.
    pub(crate) fn instance(&self) -> &Arc<dyn Subscription> {
        &self.instance
    }

    /// The host the instance was built with.
    pub(crate) fn host(&self) -> &HostContext {
        &self.host
    }

    /// The current members, in route order.
    pub(crate) fn members(&self) -> Vec<SubscriptionMember> {
        self.members.lock().expect("subscription members").clone()
    }

    /// Rereads the accounts and rebuilds the member list. Returns whether
    /// the ordered list of account ids changed.
    pub(crate) async fn resolve(&self, cooldowns: &Arc<Cooldowns>) -> bool {
        let accounts = match self.host.accounts().list().await {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::warn!(subscription = %self.config.id, %error, "account list failed");
                return false;
            }
        };
        let mut ready = Vec::new();
        for account in accounts {
            if let Accounts::One(id) = &self.config.accounts
                && account.id().as_str() != id
            {
                continue;
            }
            match self.instance.account_status(account.id(), &self.host).await {
                Ok(AccountStatus::Ready) => ready.push(account),
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    subscription = %self.config.id,
                    account = %account.id(),
                    %error,
                    "account status failed; leaving the account out"
                ),
            }
        }
        ready.sort_by(|left, right| {
            (left.label(), left.id().as_str()).cmp(&(right.label(), right.id().as_str()))
        });
        let previous = self.members();
        let unchanged = previous.len() == ready.len()
            && previous
                .iter()
                .zip(&ready)
                .all(|(member, account)| member.account == *account.id());
        if unchanged {
            return false;
        }
        let mut members = Vec::with_capacity(ready.len());
        for account in &ready {
            if let Some(member) = previous
                .iter()
                .find(|member| member.account == *account.id())
            {
                members.push(member.clone());
                continue;
            }
            match self.member(account.id(), cooldowns) {
                Ok(member) => members.push(member),
                Err(error) => tracing::warn!(
                    subscription = %self.config.id,
                    account = %account.id(),
                    %error,
                    "subscription member build failed"
                ),
            }
        }
        *self.members.lock().expect("subscription members") = members;
        true
    }

    fn member(
        &self,
        account: &AccountId,
        cooldowns: &Arc<Cooldowns>,
    ) -> Result<SubscriptionMember, DaemonError> {
        let id = MemberId::from_parts(self.plugin.clone(), account.clone());
        if credit_windowed(&self.plugin) {
            cooldowns.configure_credit_window(&id);
        }
        let host = hofund_plugin_host::runtime_host_with_observe_for(
            PluginKind::Subscription,
            self.plugin.clone(),
            String::new(),
            self.root.clone(),
            self.limits,
            membership::observe_backend_for(id.clone(), Arc::clone(cooldowns)),
        )
        .map_err(show)?;
        let upstream = self.instance.upstream(account, &host).map_err(show)?;
        let adapter = adapt::ProviderAdapter::new(id.to_string(), Arc::clone(&upstream));
        Ok(SubscriptionMember {
            account: account.clone(),
            built: BuiltProvider {
                adapter: Arc::new(adapter),
                protocol: MEMBER_PROTOCOL,
                timeout: MEMBER_TIMEOUT,
                plugin_id: self.plugin.as_str().to_owned(),
                preset_id: String::new(),
                fingerprint: String::new(),
            },
            id,
            upstream,
        })
    }

    async fn quota(&self, account: &AccountId) -> Result<Vec<QuotaWindow>, PluginError> {
        self.instance.quota(account, &self.host).await.map_err(|error| {
            if error.kind() != PluginErrorKind::Unsupported {
                tracing::warn!(
                    subscription = %self.config.id,
                    account = %account,
                    %error,
                    "quota poll failed"
                );
            }
            error
        })
    }

    fn quota_card(&self, account: &AccountId) -> Quota {
        Quota {
            provider: self.config.id.clone(),
            name: self.config.id.clone(),
            kind: QuotaKind::Subscription,
            user: account.to_string(),
            ..Quota::default()
        }
    }
}

/// Expands subscription hops into one hop per member and returns the routes
/// with the built map extended by the members.
///
/// # Errors
///
/// Returns [`DaemonError`] when a hop names a subscription that was not built
/// or a member key collides with a provider id.
pub(crate) fn expand(
    providers: &BTreeMap<String, BuiltProvider>,
    routes: &BTreeMap<String, Vec<PlannedHop>>,
    subscriptions: &Subscriptions,
) -> Result<Expanded, DaemonError> {
    let mut built = providers.clone();
    let mut expanded = BTreeMap::new();
    for (model, hops) in routes {
        let mut list = Vec::with_capacity(hops.len());
        for hop in hops {
            let Some(subscription) = &hop.subscription else {
                list.push(hop.clone());
                continue;
            };
            let state = subscriptions.get(subscription).ok_or_else(|| {
                DaemonError::new(format!(
                    "route `{model}` names unknown subscription `{subscription}`"
                ))
            })?;
            for member in state.members() {
                let key = member.key();
                if providers.contains_key(&key) {
                    return Err(DaemonError::new(format!(
                        "provider id `{key}` collides with a subscription member"
                    )));
                }
                list.push(PlannedHop {
                    provider: key.clone(),
                    subscription: None,
                    member_id: Some(member.id.clone()),
                    wire_model: hop.wire_model.clone(),
                    max_output_tokens: hop.max_output_tokens,
                    routing: hop.routing.clone(),
                    weight: state.config.account_weight(member.account.as_str()),
                    free: hop.free,
                });
                built.entry(key).or_insert(member.built);
            }
        }
        expanded.insert(model.clone(), list);
    }
    Ok((built, expanded))
}

/// Member ids of every subscription member.
pub(crate) fn member_ids(subscriptions: &Subscriptions) -> HashSet<MemberId> {
    subscriptions
        .values()
        .flat_map(|state| state.members().into_iter().map(|member| member.id))
        .collect()
}

/// Whether `plugin`'s accounts rest out of credit until their quota window
/// renews rather than on the flat subscription rest.
fn credit_windowed(plugin: &PluginId) -> bool {
    plugin.as_str() == "codex"
}

/// Turns polled windows into a cooldown. Only a blocking, model-wide window
/// with a known limit that is spent cools the member. Returns whether one did.
pub(crate) fn apply_windows(
    cooldowns: &Cooldowns,
    member: &MemberId,
    plugin: &PluginId,
    windows: &[QuotaWindow],
) -> bool {
    let now = SystemTime::now();
    let full = windows
        .iter()
        .filter(|window| {
            window.blocking()
                && window.scope_model().is_none()
                && window.limit().is_some_and(|limit| window.used() >= limit)
        })
        .collect::<Vec<_>>();
    let full_reset = full
        .iter()
        .filter_map(|window| {
            window
                .resets_at()
                .and_then(|reset| reset.duration_since(now).ok())
        })
        .max();
    let full_unknown = full.iter().any(|window| window.resets_at().is_none());
    cooldowns.observe_credit_window(member, full_reset, full_unknown);
    let mut cooled = false;
    for window in windows {
        if !window.blocking() || window.scope_model().is_some() {
            continue;
        }
        let Some(limit) = window.limit() else {
            continue;
        };
        if window.used() < limit {
            continue;
        }
        let mut observation = QuotaObservation::from_stored(plugin.clone())
            .with_remaining(0)
            .with_limit(limit);
        if let Some(after) = window
            .resets_at()
            .and_then(|reset| reset.duration_since(now).ok())
        {
            observation = observation.with_window_reset(after);
        }
        cooldowns.observe(member, &observation);
        cooled = true;
    }
    cooled
}

/// `base` scaled into the range 80% to 120% by a hash of `member` and `round`.
pub(crate) fn jittered(
    base: Duration,
    member: &MemberId,
    round: u64,
    state: &impl BuildHasher,
) -> Duration {
    let mut hasher = state.build_hasher();
    member.hash(&mut hasher);
    round.hash(&mut hasher);
    let permille = 800 + hasher.finish() % 401;
    let millis = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(millis / 1000 * permille + millis % 1000 * permille / 1000)
}

/// Per-member quota poll schedule.
#[derive(Default)]
pub(crate) struct QuotaPolls {
    next: HashMap<MemberId, Instant>,
    unsupported: HashSet<MemberId>,
    round: u64,
    hasher: std::collections::hash_map::RandomState,
}

impl QuotaPolls {
    /// Polls each member that is due and returns the members that cooled.
    pub(crate) async fn poll(
        &mut self,
        subscriptions: &Subscriptions,
        cooldowns: &Cooldowns,
        quotas: &QuotaStore,
        now: Instant,
        force: bool,
    ) -> Vec<MemberId> {
        let mut live = HashSet::new();
        let mut cooled = Vec::new();
        for state in subscriptions.values() {
            for member in state.members() {
                live.insert(member.id.clone());
                let card = state.quota_card(&member.account);
                quotas.register(card.clone());
                if self.unsupported.contains(&member.id) {
                    quotas.record_deferred(card);
                    continue;
                }
                if !force && self.next.get(&member.id).is_some_and(|due| *due > now) {
                    continue;
                }
                self.round = self.round.wrapping_add(1);
                let delay = jittered(QUOTA_POLL, &member.id, self.round, &self.hasher);
                self.next.insert(member.id.clone(), now + delay);
                match state.quota(&member.account).await {
                    Ok(windows) => {
                        if apply_windows(cooldowns, &member.id, &state.plugin, &windows) {
                            cooled.push(member.id.clone());
                        }
                        quotas.record_success(
                            card,
                            windows.iter().map(WindowInfo::from_plugin).collect(),
                            quotas.now(),
                        );
                    }
                    Err(error) if error.kind() == PluginErrorKind::Unsupported => {
                        self.unsupported.insert(member.id.clone());
                        self.next.remove(&member.id);
                        quotas.record_deferred(card);
                    }
                    Err(error) => {
                        let transient = matches!(
                            error.kind(),
                            PluginErrorKind::Connect
                                | PluginErrorKind::Timeout
                                | PluginErrorKind::Transport
                                | PluginErrorKind::Interrupted
                                | PluginErrorKind::Io
                                | PluginErrorKind::Internal
                                | PluginErrorKind::Quota
                        ) || error.status().is_some_and(|status| status == 429 || status >= 500);
                        let mut failed = card;
                        failed.error = error.to_string();
                        quotas.record_failure(failed, transient);
                    }
                }
            }
        }
        self.next.retain(|id, _| live.contains(id));
        self.unsupported.retain(|id| live.contains(id));
        cooled
    }

    #[cfg(test)]
    pub(crate) fn is_unsupported(&self, member: &MemberId) -> bool {
        self.unsupported.contains(member)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use hofund_config::Config;
    use hofund_plugin_api::{
        AccountId, MemberId, PluginId, ProtocolId, Request, SignInFlow, SignInFlowKind,
    };
    use hofund_router::{CooldownPolicy, Cooldowns};

    use super::{QuotaPolls, Subscriptions, build};
    use crate::runtime::Runtime;
    use crate::{DaemonRegistry, RegistryAdapter};

    const PLUGIN: &str = "fixture-sub";

    /// Writes account metadata rows. Listing reads no secret, so the test
    /// never touches an OS credential store.
    fn write_accounts(dir: &Path, accounts: &[(&str, &str)]) {
        let rows = accounts
            .iter()
            .map(|(id, label)| {
                format!(
                    "{{\"kind\":\"subscription\",\"plugin\":\"{PLUGIN}\",\"account\":\"{id}\",\"label\":\"{label}\",\"secret_ref\":\"encrypted-file:v2\"}}"
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let state = dir.join(".hofund");
        std::fs::create_dir_all(&state).expect("state directory");
        let staged = state.join("accounts.json.staged");
        std::fs::write(&staged, format!("[{rows}]")).expect("accounts");
        std::fs::rename(&staged, state.join("accounts.json")).expect("swap accounts");
    }

    fn config(options: &str) -> Config {
        hofund_config::load(&format!(
            "[[providers]]\nid = \"plain\"\nprotocol = \"openai-chat\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"key\"\n\
             [[subscriptions]]\nid = \"{PLUGIN}\"\naccounts = \"*\"\noptions = {{ {options} }}\n\
             [[routes]]\nmodel = \"demo\"\nupstreams = [\"plain\", {{ subscription = \"{PLUGIN}\", model = \"wire\" }}]\n"
        ))
        .expect("config")
    }

    fn member(account: &str) -> MemberId {
        MemberId::from_parts(
            PluginId::from_static(PLUGIN),
            AccountId::try_from(account).expect("account id"),
        )
    }

    fn plain() -> MemberId {
        crate::membership::member_id("plain")
    }

    fn runtime(dir: &Path, config: &Config) -> (Arc<Runtime>, Arc<hofund_router::Snapshot>) {
        let runtime = Runtime::new(Arc::new(RegistryAdapter), &dir.join("hofund.toml"), Some(0));
        let snapshot = runtime.initial_snapshot(config, None).expect("snapshot");
        (runtime, Arc::new(snapshot))
    }

    fn routed(snapshot: &hofund_router::Snapshot) -> Vec<MemberId> {
        snapshot.load().member_ids()
    }

    fn states(dir: &Path, config: &Config) -> Subscriptions {
        let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
        build(config, &dir.join("hofund.toml"), Some(0), &registry, None).expect("subscriptions")
    }

    fn cooldowns() -> Arc<Cooldowns> {
        Arc::new(Cooldowns::new(CooldownPolicy::default()))
    }

    #[test]
    fn spec_kind_matches_instance_flow() {
        let dir = tempfile::tempdir().expect("dir");
        let states = states(dir.path(), &config(""));
        let state = states.get(PLUGIN).expect("fixture state");
        let registry = RegistryAdapter;
        let spec = registry.subscription(PLUGIN).expect("factory").spec();
        assert_eq!(state.instance.flow().kind(), spec.flow());
        assert_eq!(spec.flow(), SignInFlowKind::Device);
    }

    #[test]
    fn device_host_comes_from_validated_options() {
        let dir = tempfile::tempdir().expect("dir");
        let states = states(
            dir.path(),
            &config("host = \"https://Tenant.Example.COM/\""),
        );
        let state = states.get(PLUGIN).expect("fixture state");
        let SignInFlow::Device(flow) = state.instance.flow() else {
            panic!("device flow expected");
        };
        assert_eq!(flow.host(), "tenant.example.com");
        assert_eq!(flow.verification_url(), "https://tenant.example.com/device");
    }

    #[test]
    fn invalid_device_host_fails_at_build() {
        let dir = tempfile::tempdir().expect("dir");
        let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
        let config = config("host = \"http://tenant.example.com\"");
        let error = build(
            &config,
            &dir.path().join("hofund.toml"),
            Some(0),
            &registry,
            None,
        )
        .err()
        .expect("http host rejected");
        assert!(error.to_string().contains("https"), "{error}");
    }

    #[test]
    fn instance_flow_kind_mismatch_fails_at_build() {
        let dir = tempfile::tempdir().expect("dir");
        let registry: Arc<dyn DaemonRegistry> = Arc::new(RegistryAdapter);
        let config = config("flow = \"token-file\"");
        let error = build(
            &config,
            &dir.path().join("hofund.toml"),
            Some(0),
            &registry,
            None,
        )
        .err()
        .expect("kind mismatch rejected");
        assert!(
            error
                .to_string()
                .contains("built a TokenFile sign-in flow but its spec declares Device"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn subscription_accounts_become_members_with_from_parts_ids() {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("work", "Work")]);
        let config = config("");
        let (runtime, snapshot) = runtime(dir.path(), &config);
        assert_eq!(routed(&snapshot), vec![plain()]);
        assert!(runtime.refresh_subscriptions().await);
        assert_eq!(routed(&snapshot), vec![plain(), member("work")]);
        assert_eq!(member("work").to_string(), "account:fixture-sub:work");
    }

    #[tokio::test]
    async fn route_subscription_key_expands_in_account_order() {
        let dir = tempfile::tempdir().expect("dir");
        // Label first, then id: "Alpha" < "Same", and `y` < `z` on a tie.
        write_accounts(
            dir.path(),
            &[("z", "Same"), ("a", "Zed"), ("y", "Same"), ("x", "Alpha")],
        );
        let config = config("");
        let (runtime, snapshot) = runtime(dir.path(), &config);
        runtime.refresh_subscriptions().await;
        assert_eq!(
            routed(&snapshot),
            vec![plain(), member("x"), member("y"), member("z"), member("a")]
        );
    }

    #[tokio::test]
    async fn account_weights_expand_with_stable_member_ids() {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("work", "Work"), ("other", "Other")]);
        let mut config = config("");
        config.routes[0].routing = "weight".to_owned();
        config.subscriptions[0]
            .account_weights
            .insert("work".to_owned(), 3);
        let subscriptions = states(dir.path(), &config);
        subscriptions
            .get(PLUGIN)
            .expect("subscription")
            .resolve(&cooldowns())
            .await;
        let (routes, _) = crate::route_plan(&config, None);
        let (_, expanded) =
            super::expand(&BTreeMap::new(), &routes, &subscriptions).expect("expanded routes");

        let work = expanded["demo"]
            .iter()
            .find(|hop| hop.member_id.as_ref() == Some(&member("work")))
            .expect("work hop");
        let other = expanded["demo"]
            .iter()
            .find(|hop| hop.member_id.as_ref() == Some(&member("other")))
            .expect("other hop");
        assert_eq!(work.weight, 3);
        assert_eq!(other.weight, 1);
        assert_eq!(work.routing, "weight");
    }

    #[tokio::test]
    async fn member_ids_stable_across_snapshot_swap() {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("one", "One"), ("two", "Two")]);
        let config = config("");
        let (runtime, snapshot) = runtime(dir.path(), &config);
        runtime.refresh_subscriptions().await;
        let before = routed(&snapshot);
        runtime.publish_candidate(&config).expect("reload");
        assert_eq!(routed(&snapshot), before);
        assert!(!runtime.refresh_subscriptions().await, "nothing changed");
        assert_eq!(routed(&snapshot), before);
    }

    #[tokio::test]
    async fn needs_sign_in_account_left_out() {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("ready", "Ready"), ("stale", "Stale")]);
        let config = config("needs_sign_in = \"stale\"");
        let (runtime, snapshot) = runtime(dir.path(), &config);
        runtime.refresh_subscriptions().await;
        assert_eq!(routed(&snapshot), vec![plain(), member("ready")]);
    }

    #[tokio::test]
    async fn new_account_republishes_within_tick() {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("one", "One")]);
        let config = config("");
        let (runtime, snapshot) = runtime(dir.path(), &config);
        runtime.refresh_subscriptions().await;
        let task = runtime.spawn_subscriptions();
        // Let the immediate first pass finish so only the tick can see the
        // new account.
        tokio::time::sleep(Duration::from_millis(300)).await;
        write_accounts(dir.path(), &[("one", "One"), ("two", "Two")]);
        // One 5 s tick plus slack; a fixed bound so a slower tick fails.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while !routed(&snapshot).contains(&member("two")) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "new account did not join within one tick"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        task.abort();
        assert_eq!(
            routed(&snapshot),
            vec![plain(), member("one"), member("two")]
        );
    }

    #[tokio::test]
    async fn removed_account_drops_member() {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("one", "One"), ("two", "Two")]);
        let config = config("");
        let (runtime, snapshot) = runtime(dir.path(), &config);
        runtime.refresh_subscriptions().await;
        runtime.cooldown_book().record_failure(
            &member("two"),
            &hofund_plugin_api::PluginError::connect("down"),
        );
        write_accounts(dir.path(), &[("one", "One")]);
        assert!(runtime.refresh_subscriptions().await);
        assert_eq!(routed(&snapshot), vec![plain(), member("one")]);
        assert_eq!(runtime.cooldown_book().revision(&member("two")), 0);
        // Same count, different account.
        write_accounts(dir.path(), &[("other", "Other")]);
        assert!(runtime.refresh_subscriptions().await);
        assert_eq!(routed(&snapshot), vec![plain(), member("other")]);
    }

    async fn polled(mode: &str) -> (Arc<Cooldowns>, QuotaPolls, Vec<MemberId>, Subscriptions) {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("one", "One"), ("two", "Two")]);
        let states = states(dir.path(), &config(&format!("quota = \"{mode}\"")));
        let cooldowns = cooldowns();
        for state in states.values() {
            assert!(state.resolve(&cooldowns).await);
        }
        let mut polls = QuotaPolls::default();
        let cooled = polls
            .poll(&states, &cooldowns, tokio::time::Instant::now())
            .await;
        (cooldowns, polls, cooled, states)
    }

    #[tokio::test]
    async fn exhausted_polled_window_cools_member() {
        let (cooldowns, mut polls, cooled, states) = polled("exhausted").await;
        assert_eq!(cooled, vec![member("one"), member("two")]);
        let rest = cooldowns.cooling(&member("one")).expect("cooled");
        let left = rest
            .until()
            .saturating_duration_since(std::time::Instant::now());
        assert!(
            left > Duration::from_secs(30) && left <= Duration::from_secs(61),
            "{left:?}"
        );
        let again = polls
            .poll(&states, &cooldowns, tokio::time::Instant::now())
            .await;
        assert!(again.is_empty(), "polled again before the interval");
        let later = tokio::time::Instant::now() + super::QUOTA_POLL * 2;
        assert_eq!(polls.poll(&states, &cooldowns, later).await.len(), 2);
    }

    #[tokio::test]
    async fn scoped_or_nonblocking_window_does_not_cool() {
        for mode in ["scoped", "nonblocking", "ok"] {
            let (cooldowns, _, cooled, _) = polled(mode).await;
            assert!(cooled.is_empty(), "{mode}");
            assert!(cooldowns.cooling(&member("one")).is_none(), "{mode}");
        }
    }

    #[tokio::test]
    async fn unsupported_quota_stops_polling_the_member() {
        let (cooldowns, polls, cooled, _) = polled("unsupported").await;
        assert!(cooled.is_empty());
        assert!(cooldowns.cooling(&member("one")).is_none());
        assert!(polls.is_unsupported(&member("one")));
        assert!(polls.is_unsupported(&member("two")));
    }

    #[tokio::test]
    async fn member_observe_bound_to_member() {
        let dir = tempfile::tempdir().expect("dir");
        write_accounts(dir.path(), &[("one", "One"), ("two", "Two")]);
        let states = states(dir.path(), &config(""));
        let cooldowns = cooldowns();
        let state = states.get(PLUGIN).expect("state");
        state.resolve(&cooldowns).await;
        let members = state.members();
        let first = members
            .iter()
            .find(|member| member.account.as_str() == "one")
            .expect("member");
        let _ = first
            .upstream
            .execute(Request::new("wire"), ProtocolId::from_static("openai-chat"))
            .await;
        assert!(cooldowns.cooling(&member("one")).is_some());
        assert!(cooldowns.cooling(&member("two")).is_none());
    }

    fn full_window(name: &str, reset: Option<Duration>) -> hofund_plugin_api::QuotaWindow {
        hofund_plugin_api::QuotaWindow::new(
            name,
            10,
            Some(10),
            reset.map(|after| std::time::SystemTime::now() + after),
        )
        .expect("window")
    }

    fn credit_left(cooldowns: &Cooldowns, id: &MemberId) -> Duration {
        cooldowns
            .cooling(id)
            .expect("cooled")
            .until()
            .saturating_duration_since(std::time::Instant::now())
    }

    #[test]
    fn credit_rest_uses_latest_full_window_and_lifts_when_it_renews_sooner() {
        const HOUR: Duration = Duration::from_secs(3600);
        let cooldowns = cooldowns();
        let id = member("one");
        cooldowns.configure_credit_window(&id);
        let plugin = PluginId::from_static(PLUGIN);
        let both = [
            full_window("five-hour", Some(HOUR)),
            full_window("weekly", Some(3 * HOUR)),
        ];
        assert!(super::apply_windows(&cooldowns, &id, &plugin, &both));
        cooldowns.record_failure(
            &id,
            &hofund_plugin_api::PluginError::credit_status(402, "out"),
        );
        let left = credit_left(&cooldowns, &id);
        assert!(left > 3 * HOUR - Duration::from_secs(60), "{left:?}");
        // The same reading again is not a renewal.
        super::apply_windows(&cooldowns, &id, &plugin, &both);
        assert!(credit_left(&cooldowns, &id) > 3 * HOUR - Duration::from_secs(60));
        // Only the five-hour window is still full: the latest full reset
        // moved sooner, so the credit rest lifts and only the quota rest stays.
        super::apply_windows(&cooldowns, &id, &plugin, &both[..1]);
        let left = credit_left(&cooldowns, &id);
        assert!(left <= HOUR + Duration::from_secs(1), "{left:?}");
    }

    #[test]
    fn credit_rest_waits_for_a_full_window_with_no_reset_to_renew() {
        const HOUR: Duration = Duration::from_secs(3600);
        let cooldowns = cooldowns();
        let id = member("one");
        cooldowns.configure_credit_window(&id);
        let plugin = PluginId::from_static(PLUGIN);
        super::apply_windows(&cooldowns, &id, &plugin, &[full_window("weekly", None)]);
        cooldowns.record_failure(
            &id,
            &hofund_plugin_api::PluginError::credit_status(402, "out"),
        );
        assert!(credit_left(&cooldowns, &id) > 5 * HOUR - Duration::from_secs(60));
        super::apply_windows(
            &cooldowns,
            &id,
            &plugin,
            &[full_window("weekly", Some(HOUR))],
        );
        assert!(cooldowns.cooling(&id).is_none_or(|rest| {
            rest.until()
                .saturating_duration_since(std::time::Instant::now())
                <= HOUR + Duration::from_secs(1)
        }));
    }

    #[test]
    fn credit_rest_ignores_a_window_that_is_not_full() {
        const HOUR: Duration = Duration::from_secs(3600);
        let cooldowns = cooldowns();
        let id = member("one");
        cooldowns.configure_credit_window(&id);
        let plugin = PluginId::from_static(PLUGIN);
        let open = hofund_plugin_api::QuotaWindow::new(
            "weekly",
            3,
            Some(10),
            Some(std::time::SystemTime::now() + 3 * HOUR),
        )
        .expect("window");
        assert!(!super::apply_windows(&cooldowns, &id, &plugin, &[open]));
        cooldowns.record_failure(
            &id,
            &hofund_plugin_api::PluginError::credit_status(402, "out"),
        );
        let left = credit_left(&cooldowns, &id);
        assert!(left > 5 * HOUR - Duration::from_secs(60), "{left:?}");
    }

    #[test]
    fn only_codex_accounts_rest_on_their_window() {
        assert!(super::credit_windowed(&PluginId::from_static("codex")));
        assert!(!super::credit_windowed(&PluginId::from_static(PLUGIN)));
        assert!(!super::credit_windowed(&PluginId::from_static("claude")));
    }

    #[test]
    fn unknown_subscription_fails_at_factory_resolution() {
        let dir = tempfile::tempdir().expect("dir");
        let config = hofund_config::load(
            "[[subscriptions]]\nid = \"no-such-sub\"\naccounts = \"*\"\n\
             [[routes]]\nmodel = \"demo\"\nupstreams = [{ subscription = \"no-such-sub\" }]\n",
        )
        .expect("config");
        let runtime = Runtime::new(
            Arc::new(RegistryAdapter),
            &dir.path().join("hofund.toml"),
            Some(0),
        );
        let error = runtime
            .initial_snapshot(&config, None)
            .err()
            .expect("unlinked subscription");
        assert!(
            error
                .to_string()
                .contains("no subscription plugin `no-such-sub` is linked"),
            "{error}"
        );
    }
}
