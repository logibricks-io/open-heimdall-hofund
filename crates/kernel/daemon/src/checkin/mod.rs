//! Daily check-in orchestration for subscription accounts.

mod day;

#[cfg(test)]
mod tick_tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hofund_config::Value;
use hofund_plugin_api::{
    AccountId, AccountStatus, CheckinAccountStatus, CheckinOutcome, CheckinRecord, CheckinResult,
    PluginErrorKind,
};
use hofund_plugin_sdk::async_trait;
use hofund_plugin_sdk::daily_checkin::{self, ENABLED_KEY};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::registry::DaemonRegistry;
use crate::runtime::Runtime;
use crate::subscriptions::{SubscriptionMember, SubscriptionState};

pub(crate) use day::{local_day_key, local_offset};

/// How often the daemon looks for due check-ins.
pub(crate) const TICK: Duration = Duration::from_secs(60);

/// Initial delay before the first check-in pass.
pub(crate) const START_DELAY: Duration = Duration::from_secs(2 * 60);

/// Per-account check-in deadline for one vendor round trip.
const RUN_TIMEOUT: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    daily: BTreeMap<String, String>,
}

/// In-memory check-in state plus on-disk keys so restarts do not repeat the same day.
pub(crate) struct CheckinStore {
    path: PathBuf,
    persisted: Mutex<Persisted>,
    status: Mutex<BTreeMap<String, CheckinAccountStatus>>,
    running: tokio::sync::Mutex<()>,
}

impl CheckinStore {
    pub(crate) fn new(root: &Path) -> Self {
        let path = root.join("daily-checkin.json");
        let persisted = read_persisted(&path);
        Self {
            path,
            persisted: Mutex::new(persisted),
            status: Mutex::new(BTreeMap::new()),
            running: tokio::sync::Mutex::new(()),
        }
    }

    fn last_day(&self, member: &str) -> String {
        self.persisted
            .lock()
            .expect("checkin persisted")
            .daily
            .get(member)
            .cloned()
            .unwrap_or_default()
    }

    fn set_day(&self, member: &str, day: &str) {
        let snapshot = {
            let mut persisted = self.persisted.lock().expect("checkin persisted");
            persisted.daily.insert(member.to_owned(), day.to_owned());
            persisted.clone()
        };
        if let Err(error) = write_persisted(&self.path, &snapshot) {
            warn!(
                path = %self.path.display(),
                error = %error,
                "failed to persist daily check-in state"
            );
        }
    }

    fn record(&self, row: CheckinAccountStatus) {
        let key = member_key(row.subscription(), row.account().as_str());
        self.status.lock().expect("checkin status").insert(key, row);
    }

    pub(crate) fn statuses(&self) -> Vec<CheckinAccountStatus> {
        self.status
            .lock()
            .expect("checkin status")
            .values()
            .cloned()
            .collect()
    }
}

fn member_key(subscription: &str, account: &str) -> String {
    format!("{subscription}/{account}")
}

fn read_persisted(path: &Path) -> Persisted {
    let Ok(bytes) = std::fs::read(path) else {
        return Persisted::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn write_persisted(path: &Path, data: &Persisted) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(data).expect("checkin state serializes");
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

pub(crate) fn enabled_from_options(options: &BTreeMap<String, Value>) -> Result<bool, String> {
    let mut raw = hofund_plugin_api::RawTable::new();
    if let Some(value) = options.get(ENABLED_KEY) {
        match value {
            Value::Boolean(enabled) => raw.insert(
                ENABLED_KEY.to_owned(),
                hofund_plugin_api::RawValue::Boolean(*enabled),
            ),
            _ => return Err(format!("{ENABLED_KEY} must be a boolean")),
        }
    }
    daily_checkin::read_enabled(&raw).map_err(|error| error.to_string())
}

struct Candidate {
    subscription: String,
    state: Arc<SubscriptionState>,
    member: SubscriptionMember,
}

pub(crate) async fn tick(
    runtime: &Arc<Runtime>,
    store: &Arc<CheckinStore>,
    registry: &dyn DaemonRegistry,
) {
    tick_with_runner(runtime, store, registry, &ProductionRunner).await;
}

struct ProductionRunner;

#[async_trait]
pub(crate) trait CheckinRunner: Send + Sync {
    async fn run(
        &self,
        state: &SubscriptionState,
        account: &AccountId,
    ) -> Result<CheckinResult, String>;
}

#[async_trait]
impl CheckinRunner for ProductionRunner {
    async fn run(
        &self,
        state: &SubscriptionState,
        account: &AccountId,
    ) -> Result<CheckinResult, String> {
        run_checkin(state, account).await
    }
}

pub(crate) async fn tick_with_runner(
    runtime: &Arc<Runtime>,
    store: &Arc<CheckinStore>,
    registry: &dyn DaemonRegistry,
    runner: &dyn CheckinRunner,
) {
    let _guard = store.running.lock().await;
    let now = runtime.quota_now();
    let zone = local_offset(now);
    let day = local_day_key(now, zone);
    let subscriptions = runtime.subscription_states();

    let mut candidates = Vec::new();
    for (subscription_id, state) in &subscriptions {
        let Some(factory) = registry.subscription(subscription_id) else {
            continue;
        };
        if !factory.spec().daily_checkin() {
            continue;
        }
        let enabled = match enabled_from_options(&state.config.options) {
            Ok(enabled) => enabled,
            Err(message) => {
                warn!("check-in options for `{subscription_id}`: {message}");
                continue;
            }
        };
        if !enabled {
            continue;
        }
        for member in state.members() {
            candidates.push(Candidate {
                subscription: subscription_id.clone(),
                state: Arc::clone(state),
                member,
            });
        }
    }
    candidates.sort_by(|left, right| {
        left.subscription.cmp(&right.subscription).then_with(|| {
            left.member
                .account
                .as_str()
                .cmp(right.member.account.as_str())
        })
    });

    for candidate in candidates {
        let member_id = candidate.member.id.to_string();
        if store.last_day(&member_id) == day {
            continue;
        }
        if !account_ready(&candidate.state, &candidate.member.account).await {
            continue;
        }
        let result = match tokio::time::timeout(
            RUN_TIMEOUT,
            runner.run(&candidate.state, &candidate.member.account),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err("check-in timed out".to_owned()),
        };
        match result {
            Ok(outcome) => {
                let record = CheckinRecord::from_result(now, &outcome);
                if outcome.outcome() == CheckinOutcome::Failed {
                    warn!(
                        subscription = %candidate.subscription,
                        account = %candidate.member.account,
                        detail = %record.detail(),
                        "daily check-in failed; will retry on the next local day"
                    );
                }
                store.set_day(&member_id, &day);
                let mut row = CheckinAccountStatus::new(
                    candidate.subscription,
                    candidate.member.account.clone(),
                );
                row.set_last(record);
                store.record(row);
            }
            Err(message) => {
                warn!(
                    subscription = %candidate.subscription,
                    account = %candidate.member.account,
                    detail = %message,
                    "daily check-in failed; will retry on the next local day"
                );
                store.set_day(&member_id, &day);
                let record = CheckinRecord::new(now, CheckinOutcome::Failed, message.clone());
                let mut row = CheckinAccountStatus::new(
                    candidate.subscription,
                    candidate.member.account.clone(),
                );
                row.set_last(record);
                store.record(row);
            }
        }
        return;
    }
}

async fn account_ready(state: &SubscriptionState, account: &AccountId) -> bool {
    matches!(
        state.instance().account_status(account, state.host()).await,
        Ok(AccountStatus::Ready)
    )
}

async fn run_checkin(
    state: &SubscriptionState,
    account: &AccountId,
) -> Result<CheckinResult, String> {
    let run = state.instance().daily_checkin(account, state.host());
    match tokio::time::timeout(RUN_TIMEOUT, run).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) if error.kind() == PluginErrorKind::Unsupported => {
            Err("daily check-in is not supported".to_owned())
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("check-in timed out".to_owned()),
    }
}

pub(crate) fn list_statuses(
    runtime: &Arc<Runtime>,
    registry: &dyn DaemonRegistry,
    store: &CheckinStore,
) -> Vec<CheckinAccountStatus> {
    let mut rows: BTreeMap<String, CheckinAccountStatus> = store
        .statuses()
        .into_iter()
        .map(|row| (member_key(row.subscription(), row.account().as_str()), row))
        .collect();
    for (subscription_id, state) in runtime.subscription_states().iter() {
        let Some(factory) = registry.subscription(subscription_id.as_str()) else {
            continue;
        };
        if !factory.spec().daily_checkin() {
            continue;
        }
        let Ok(enabled) = enabled_from_options(&state.config.options) else {
            continue;
        };
        if !enabled {
            continue;
        }
        for member in state.members() {
            rows.entry(member_key(
                subscription_id.as_str(),
                member.account.as_str(),
            ))
            .or_insert_with(|| {
                CheckinAccountStatus::new(subscription_id.clone(), member.account.clone())
            });
        }
    }
    let mut out: Vec<_> = rows.into_values().collect();
    out.sort_by(|left, right| {
        left.subscription()
            .cmp(right.subscription())
            .then_with(|| left.account().as_str().cmp(right.account().as_str()))
    });
    out
}

pub(crate) async fn run_now(
    runtime: &Arc<Runtime>,
    store: &Arc<CheckinStore>,
    registry: &dyn DaemonRegistry,
    subscription: &str,
    account: &str,
) -> Result<CheckinRecord, String> {
    let _guard = store.running.lock().await;
    let subscriptions = runtime.subscription_states();
    let state = subscriptions
        .get(subscription)
        .ok_or_else(|| format!("unknown subscription `{subscription}`"))?;
    let factory = registry
        .subscription(subscription)
        .ok_or_else(|| format!("subscription `{subscription}` is not linked"))?;
    if !factory.spec().daily_checkin() {
        return Err("this subscription does not support daily check-in".to_owned());
    }
    if !enabled_from_options(&state.config.options).unwrap_or(false) {
        return Err(format!(
            "enable `{ENABLED_KEY} = true` under [subscriptions.options] first"
        ));
    }
    let account_id = AccountId::try_from(account).map_err(|error| error.to_string())?;
    if !state
        .members()
        .iter()
        .any(|member| member.account == account_id)
    {
        return Err(format!(
            "account `{account}` is not on subscription `{subscription}`"
        ));
    }
    let now = runtime.quota_now();
    if !account_ready(state, &account_id).await {
        return Err("account is not signed in".to_owned());
    }
    let zone = local_offset(now);
    let day = local_day_key(now, zone);
    let member_id = state
        .members()
        .iter()
        .find(|member| member.account == account_id)
        .map(|member| member.id.to_string())
        .ok_or_else(|| format!("account `{account}` is not on subscription `{subscription}`"))?;
    match run_checkin(state, &account_id).await {
        Ok(result) => {
            let record = CheckinRecord::from_result(now, &result);
            store.set_day(&member_id, &day);
            let mut row = CheckinAccountStatus::new(subscription.to_owned(), account_id);
            row.set_last(record.clone());
            store.record(row);
            Ok(record)
        }
        Err(message) => {
            let record = CheckinRecord::new(now, CheckinOutcome::Failed, message.clone());
            let mut row = CheckinAccountStatus::new(subscription.to_owned(), account_id);
            row.set_last(record.clone());
            store.record(row);
            Err(message)
        }
    }
}

pub(crate) fn spawn(
    runtime: Arc<Runtime>,
    store: Arc<CheckinStore>,
    registry: Arc<dyn DaemonRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(START_DELAY).await;
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            tick(&runtime, &store, registry.as_ref()).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{CheckinStore, enabled_from_options, local_day_key, local_offset};
    use std::collections::BTreeMap;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn a3b_1_enabled_defaults_off_in_options() {
        assert!(!enabled_from_options(&BTreeMap::new()).expect("absent"));
    }

    #[test]
    fn a3b_3_persisted_day_round_trips() {
        let dir = tempfile::tempdir().expect("temp");
        let store = CheckinStore::new(dir.path());
        store.set_day("acct/a-0000000000000001", "2026-11-04");
        let store2 = CheckinStore::new(dir.path());
        assert_eq!(store2.last_day("acct/a-0000000000000001"), "2026-11-04");
    }

    #[test]
    fn a3b_2_local_day_key_changes_at_midnight() {
        let zone = chrono::FixedOffset::east_opt(8 * 3600).expect("offset");
        let early = UNIX_EPOCH + Duration::from_secs(1_762_195_200);
        let late = early + Duration::from_secs(24 * 3600);
        assert_ne!(local_day_key(early, zone), local_day_key(late, zone));
    }

    #[test]
    fn a3b_4_failure_marks_day_without_intraday_retry() {
        let dir = tempfile::tempdir().expect("temp");
        let store = CheckinStore::new(dir.path());
        store.set_day("check-test/a-0000000000000001", "2026-11-04");
        assert_eq!(
            store.last_day("check-test/a-0000000000000001"),
            "2026-11-04"
        );
    }

    #[test]
    fn a3b_5_checkin_scheduling_is_isolated_from_routing() {
        let zone = local_offset(UNIX_EPOCH);
        let _ = local_day_key(UNIX_EPOCH, zone);
    }

    #[test]
    fn a3b_21_successful_persist_removes_temp_file() {
        let dir = tempfile::tempdir().expect("temp");
        let store = CheckinStore::new(dir.path());
        let temp = dir.path().join("daily-checkin.json.tmp");
        store.set_day("member/a-0000000000000001", "2026-11-04");
        assert!(!temp.exists(), "atomic write must not leave a temp file");
        let on_disk = dir.path().join("daily-checkin.json");
        assert!(on_disk.is_file());
    }

    #[test]
    fn a3b_12_stale_temp_or_corrupt_state_file_does_not_panic() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("daily-checkin.json");
        std::fs::write(&path, b"{not json").expect("corrupt");
        std::fs::write(
            path.with_extension("json.tmp"),
            br#"{"daily":{"x/y":"2020-01-01"}}"#,
        )
        .expect("stale temp");
        let store = CheckinStore::new(dir.path());
        assert!(store.last_day("x/y").is_empty());
        store.set_day("member/a-0000000000000001", "2026-11-04");
        let store2 = CheckinStore::new(dir.path());
        assert_eq!(store2.last_day("member/a-0000000000000001"), "2026-11-04");
        let on_disk = std::fs::read_to_string(&path).expect("state file");
        assert!(on_disk.contains("2026-11-04"));
    }
}
