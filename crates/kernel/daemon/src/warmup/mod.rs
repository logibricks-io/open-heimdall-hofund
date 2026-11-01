//! Daily warm-up orchestration for subscription accounts.

mod daily;

#[cfg(test)]
mod tick_tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hofund_config::Value;
use hofund_plugin_api::{
    AccountId, AccountStatus, PluginErrorKind, WarmupAccountStatus, WarmupOutcome, WarmupRecord,
};
use hofund_plugin_sdk::async_trait;
use hofund_plugin_sdk::daily_warmup::{self, TIMES_KEY};
use hofund_router::quota::Allowance;
use serde::{Deserialize, Serialize};

use crate::registry::DaemonRegistry;
use crate::runtime::Runtime;
use crate::subscriptions::{SubscriptionMember, SubscriptionState};

pub(crate) use daily::{daily_due, local_offset, short_window_idle};

/// How often the daemon checks the wall clock for daily warm-up.
pub(crate) const TICK: Duration = Duration::from_secs(60);

/// Initial delay before the first warm-up pass, matching the reference loop.
pub(crate) const START_DELAY: Duration = Duration::from_secs(2 * 60);

/// Per-account warm-up deadline for one vendor request.
const RUN_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// After a failed daily attempt, wait this long before trying the same member again.
pub(crate) const FAILED_RETRY: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    daily: BTreeMap<String, String>,
}

/// In-memory warm-up state plus on-disk keys so restarts do not double-fire.
pub(crate) struct WarmupStore {
    path: PathBuf,
    persisted: Mutex<Persisted>,
    status: Mutex<BTreeMap<String, WarmupAccountStatus>>,
    failed_retry: Mutex<BTreeMap<String, SystemTime>>,
    running: tokio::sync::Mutex<()>,
}

impl WarmupStore {
    pub(crate) fn new(root: &Path) -> Self {
        let path = root.join("daily-warmup.json");
        let persisted = read_persisted(&path);
        Self {
            path,
            persisted: Mutex::new(persisted),
            status: Mutex::new(BTreeMap::new()),
            failed_retry: Mutex::new(BTreeMap::new()),
            running: tokio::sync::Mutex::new(()),
        }
    }

    fn last_daily_key(&self, member: &str) -> String {
        self.persisted
            .lock()
            .expect("warmup persisted")
            .daily
            .get(member)
            .cloned()
            .unwrap_or_default()
    }

    fn set_daily_key(&self, member: &str, key: &str) {
        self.persisted
            .lock()
            .expect("warmup persisted")
            .daily
            .insert(member.to_owned(), key.to_owned());
        let _ = write_persisted(
            &self.path,
            &self.persisted.lock().expect("warmup persisted"),
        );
    }

    fn record(&self, row: WarmupAccountStatus) {
        let key = member_key(row.subscription(), row.account().as_str());
        self.status.lock().expect("warmup status").insert(key, row);
    }

    pub(crate) fn statuses(&self) -> Vec<WarmupAccountStatus> {
        self.status
            .lock()
            .expect("warmup status")
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn failed_cooling(&self, member: &str, now: SystemTime) -> bool {
        self.failed_retry
            .lock()
            .expect("warmup retry")
            .get(member)
            .is_some_and(|until| now < *until)
    }

    pub(crate) fn record_failed_retry(&self, member: &str, now: SystemTime) {
        self.failed_retry
            .lock()
            .expect("warmup retry")
            .insert(member.to_owned(), now + FAILED_RETRY);
    }

    pub(crate) fn clear_failed_retry(&self, member: &str) {
        self.failed_retry
            .lock()
            .expect("warmup retry")
            .remove(member);
    }

    #[cfg(test)]
    pub(crate) fn failed_retry_until(&self, member: &str) -> Option<SystemTime> {
        self.failed_retry
            .lock()
            .expect("warmup retry")
            .get(member)
            .copied()
    }
}

fn member_key(subscription: &str, account: &str) -> String {
    format!("{subscription}/{account}")
}

fn ensure_configured_row(
    rows: &mut BTreeMap<String, WarmupAccountStatus>,
    subscription: &str,
    account: &AccountId,
) {
    rows.entry(member_key(subscription, account.as_str()))
        .or_insert_with(|| WarmupAccountStatus::new(subscription.to_owned(), account.clone()));
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
    let bytes = serde_json::to_vec(data).expect("warmup state serializes");
    std::fs::write(path, bytes)
}

pub(crate) fn times_from_options(options: &BTreeMap<String, Value>) -> Result<Vec<String>, String> {
    let Some(value) = options.get(TIMES_KEY) else {
        return Ok(Vec::new());
    };
    match value {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let Value::String(text) = item else {
                    return Err(format!("{TIMES_KEY} entries must be strings"));
                };
                let normalized = daily_warmup::normalize_time(text)
                    .map_err(|message| format!("{TIMES_KEY}: {message}"))?;
                if !out.contains(&normalized) {
                    out.push(normalized);
                }
            }
            out.sort();
            Ok(out)
        }
        Value::String(text) if text.trim().is_empty() => Ok(Vec::new()),
        Value::String(text) => {
            let mut raw = hofund_plugin_api::RawTable::new();
            raw.insert(
                TIMES_KEY.to_owned(),
                hofund_plugin_api::RawValue::String(text.clone()),
            );
            daily_warmup::read_times(&raw).map_err(|error| error.to_string())
        }
        _ => Err(format!("{TIMES_KEY} must be a string or array of times")),
    }
}

fn short_window_running(allowance: Option<&Allowance>, now: SystemTime) -> bool {
    let Some(allowance) = allowance else {
        return false;
    };
    allowance.windows.iter().any(|window| {
        window
            .window_span
            .is_some_and(|span| span < Duration::from_secs(24 * 3600))
            && !short_window_idle(window.used, window.resets_at, window.window_span, now)
    })
}

struct Candidate {
    subscription: String,
    state: Arc<SubscriptionState>,
    member: SubscriptionMember,
    times: Vec<String>,
}

/// Runs one scheduled pass. At most one account receives a warm-up at a time.
pub(crate) async fn tick(
    runtime: &Arc<Runtime>,
    store: &Arc<WarmupStore>,
    registry: &dyn DaemonRegistry,
) {
    tick_with_runner(runtime, store, registry, &ProductionRunner).await;
}

struct ProductionRunner;

#[async_trait]
pub(crate) trait WarmupRunner: Send + Sync {
    async fn run(&self, state: &SubscriptionState, account: &AccountId) -> Result<(), String>;
}

#[async_trait]
impl WarmupRunner for ProductionRunner {
    async fn run(&self, state: &SubscriptionState, account: &AccountId) -> Result<(), String> {
        run_warmup(state, account).await
    }
}

pub(crate) async fn tick_with_runner(
    runtime: &Arc<Runtime>,
    store: &Arc<WarmupStore>,
    registry: &dyn DaemonRegistry,
    runner: &dyn WarmupRunner,
) {
    let _guard = store.running.lock().await;
    let now = runtime.quota_now();
    let zone = local_offset(now);
    let subscriptions = runtime.subscription_states();

    let allowances: BTreeMap<String, Allowance> = runtime
        .quota_allowances()
        .into_iter()
        .map(|row| (row.member.clone(), row))
        .collect();

    let mut candidates = Vec::new();
    for (subscription_id, state) in &subscriptions {
        let Some(factory) = registry.subscription(subscription_id) else {
            continue;
        };
        if !factory.spec().daily_warmup() {
            continue;
        }
        let times = match times_from_options(&state.config.options) {
            Ok(times) => times,
            Err(_) => continue,
        };
        if times.is_empty() {
            continue;
        }
        for member in state.members() {
            candidates.push(Candidate {
                subscription: subscription_id.clone(),
                state: Arc::clone(state),
                member,
                times: times.clone(),
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
        if runtime.cooldowns().cooling(&candidate.member.id).is_some() {
            note_skipped(
                store,
                &candidate.subscription,
                &candidate.member.account,
                now,
                "account is resting",
            );
            continue;
        }
        let allowance = allowances.get(&member_id);
        if short_window_running(allowance, now) {
            note_skipped(
                store,
                &candidate.subscription,
                &candidate.member.account,
                now,
                "short window is already running",
            );
            continue;
        }
        if store.failed_cooling(&member_id, now) {
            continue;
        }
        let last = store.last_daily_key(&member_id);
        let Some(key) = daily_due(&candidate.times, &last, false, now, zone) else {
            continue;
        };
        if !account_ready(&candidate.state, &candidate.member.account).await {
            note_skipped(
                store,
                &candidate.subscription,
                &candidate.member.account,
                now,
                "account is not signed in",
            );
            continue;
        }
        let result = runner
            .run(&candidate.state, &candidate.member.account)
            .await;
        match result {
            Ok(()) => {
                store.clear_failed_retry(&member_id);
                store.set_daily_key(&member_id, &key);
                let mut row = WarmupAccountStatus::new(
                    candidate.subscription,
                    candidate.member.account.clone(),
                );
                row.set_last(WarmupRecord::new(now, WarmupOutcome::Ok, ""));
                store.record(row);
            }
            Err(message) => {
                store.record_failed_retry(&member_id, now);
                let mut row = WarmupAccountStatus::new(
                    candidate.subscription,
                    candidate.member.account.clone(),
                );
                row.set_last(WarmupRecord::new(now, WarmupOutcome::Failed, message));
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

async fn run_warmup(state: &SubscriptionState, account: &AccountId) -> Result<(), String> {
    let run = state.instance().daily_warmup(account, state.host());
    match tokio::time::timeout(RUN_TIMEOUT, run).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if error.kind() == PluginErrorKind::Unsupported => {
            Err("daily warm-up is not supported".to_owned())
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("warm-up timed out".to_owned()),
    }
}

fn note_skipped(
    store: &WarmupStore,
    subscription: &str,
    account: &AccountId,
    at: SystemTime,
    detail: &str,
) {
    let mut row = WarmupAccountStatus::new(subscription, account.clone());
    row.set_last(WarmupRecord::new(at, WarmupOutcome::Skipped, detail));
    store.record(row);
}

/// Status rows for the control API: every configured warm-up account, merged
/// with last-attempt records when present.
pub(crate) fn list_statuses(
    runtime: &Arc<Runtime>,
    registry: &dyn DaemonRegistry,
    store: &WarmupStore,
) -> Vec<WarmupAccountStatus> {
    let mut rows: BTreeMap<String, WarmupAccountStatus> = store
        .statuses()
        .into_iter()
        .map(|row| (member_key(row.subscription(), row.account().as_str()), row))
        .collect();
    for (subscription_id, state) in runtime.subscription_states().iter() {
        let Some(factory) = registry.subscription(subscription_id.as_str()) else {
            continue;
        };
        if !factory.spec().daily_warmup() {
            continue;
        }
        let Ok(times) = times_from_options(&state.config.options) else {
            continue;
        };
        if times.is_empty() {
            continue;
        }
        for member in state.members() {
            ensure_configured_row(&mut rows, subscription_id.as_str(), &member.account);
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

/// Runs warm-up for one account immediately, ignoring the schedule.
pub(crate) async fn run_now(
    runtime: &Arc<Runtime>,
    store: &Arc<WarmupStore>,
    registry: &dyn DaemonRegistry,
    subscription: &str,
    account: &str,
) -> Result<WarmupRecord, String> {
    let _guard = store.running.lock().await;
    let subscriptions = runtime.subscription_states();
    let state = subscriptions
        .get(subscription)
        .ok_or_else(|| format!("unknown subscription `{subscription}`"))?;
    let factory = registry
        .subscription(subscription)
        .ok_or_else(|| format!("subscription `{subscription}` is not linked"))?;
    if !factory.spec().daily_warmup() {
        return Err("this subscription does not support daily warm-up".to_owned());
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
    match run_warmup(state, &account_id).await {
        Ok(()) => {
            let record = WarmupRecord::new(now, WarmupOutcome::Ok, "");
            let mut row = WarmupAccountStatus::new(subscription.to_owned(), account_id.clone());
            row.set_last(record.clone());
            store.record(row);
            Ok(record)
        }
        Err(message) => {
            let record = WarmupRecord::new(now, WarmupOutcome::Failed, message.clone());
            let mut row = WarmupAccountStatus::new(subscription.to_owned(), account_id);
            row.set_last(record.clone());
            store.record(row);
            Err(message)
        }
    }
}

pub(crate) fn spawn(
    runtime: Arc<Runtime>,
    store: Arc<WarmupStore>,
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
    use super::{FAILED_RETRY, WarmupStore, note_skipped, times_from_options};
    use hofund_config::Value;
    use hofund_plugin_api::{AccountId, WarmupOutcome};
    use std::collections::BTreeMap;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn a3a_6_skipped_resting_records_detail() {
        let dir = tempfile::tempdir().expect("temp");
        let store = WarmupStore::new(dir.path());
        let account = AccountId::from_static("a-0000000000000001");
        note_skipped(&store, "acct", &account, UNIX_EPOCH, "account is resting");
        let row = store
            .statuses()
            .into_iter()
            .find(|row| row.account() == &account)
            .expect("row");
        let last = row.last().expect("last");
        assert_eq!(last.outcome(), WarmupOutcome::Skipped);
        assert_eq!(last.detail(), "account is resting");
    }

    #[test]
    fn a3a_7_skipped_signed_out_records_detail() {
        let dir = tempfile::tempdir().expect("temp");
        let store = WarmupStore::new(dir.path());
        let account = AccountId::from_static("a-0000000000000002");
        note_skipped(
            &store,
            "fixture-sub",
            &account,
            UNIX_EPOCH,
            "account is not signed in",
        );
        let row = store
            .statuses()
            .into_iter()
            .find(|row| row.account() == &account)
            .expect("row");
        let last = row.last().expect("last");
        assert_eq!(last.outcome(), WarmupOutcome::Skipped);
        assert_eq!(last.detail(), "account is not signed in");
    }

    #[test]
    fn a3a_15_options_times_parse_from_array() {
        let mut options = BTreeMap::new();
        options.insert(
            "daily-warmup-at-times".to_owned(),
            Value::Array(vec![
                Value::String("21:30".to_owned()),
                Value::String("6:00".to_owned()),
            ]),
        );
        let times = times_from_options(&options).expect("parse");
        assert_eq!(times, ["06:00", "21:30"]);
    }

    #[test]
    fn a3a_5_persisted_state_round_trips() {
        let dir = tempfile::tempdir().expect("temp");
        let store = WarmupStore::new(dir.path());
        store.set_daily_key("acct/a@x.io", "2026-09-28 06:00");
        let store2 = WarmupStore::new(dir.path());
        assert_eq!(store2.last_daily_key("acct/a@x.io"), "2026-09-28 06:00");
    }

    #[test]
    fn a3a_21_failed_retry_skips_account_until_backoff_expires() {
        let dir = tempfile::tempdir().expect("temp");
        let store = WarmupStore::new(dir.path());
        let now = UNIX_EPOCH;
        store.record_failed_retry("acct/a-0000000000000001", now);
        assert!(store.failed_cooling("acct/a-0000000000000001", now));
        assert!(!store.failed_cooling(
            "acct/a-0000000000000001",
            now + FAILED_RETRY + Duration::from_secs(1)
        ));
        assert!(!store.failed_cooling("acct/a-0000000000000002", now));
        let order = ["acct/a-0000000000000001", "acct/a-0000000000000002"];
        let next = order
            .iter()
            .find(|id| !store.failed_cooling(id, now))
            .expect("peer runs");
        assert_eq!(*next, "acct/a-0000000000000002");
    }
}
