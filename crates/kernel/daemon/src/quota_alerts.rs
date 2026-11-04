//! Daemon-side quota alert settings and periodic checks.

use std::sync::Arc;

use hofund_control_proto::{
    EventPayload, QuotaAlertEvent, QuotaAlertsResponse, QuotaAlertsSetRequest,
    QuotaAlertsSetResponse, Request,
};
use hofund_router::quota_alerts::{QuotaAlertStore, alert_text};
use tokio::sync::Notify;

use crate::runtime::Runtime;
use hofund_gateway::control::ControlFailure;

const ALERT_POLL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub(crate) fn list(store: &QuotaAlertStore) -> QuotaAlertsResponse {
    QuotaAlertsResponse {
        usage_alert: store.usage_alert(),
        balance_alert: store.balance_alert(),
        ..QuotaAlertsResponse::default()
    }
}

pub(crate) fn set(
    store: &QuotaAlertStore,
    wake: &Notify,
    request: QuotaAlertsSetRequest,
) -> Result<QuotaAlertsSetResponse, ControlFailure> {
    request.validate().map_err(ControlFailure::from)?;
    if let Some(pct) = request.usage_alert {
        store.set_usage_alert(pct);
    }
    if let Some(bal) = request.balance_alert {
        store.set_balance_alert(bal);
    }
    wake.notify_one();
    Ok(QuotaAlertsSetResponse {
        usage_alert: store.usage_alert(),
        balance_alert: store.balance_alert(),
        ..QuotaAlertsSetResponse::default()
    })
}

/// Refreshes quota cards when needed, evaluates thresholds, and emits events.
pub(crate) async fn check_once(runtime: &Runtime, store: &QuotaAlertStore) {
    if !store.any_enabled() {
        return;
    }
    let _ = runtime.quota_report(false).await;
    let now = runtime.quota_now();
    let cards = runtime.quotas().alert_cards();
    let balance_threshold = store.balance_alert();
    for alert in store.check(&cards, now) {
        let (title, body) = alert_text(&alert, balance_threshold, now);
        let mut event = QuotaAlertEvent::default();
        event.title = title;
        event.body = body;
        event.provider = alert.provider;
        event.window = alert.window;
        if let Some(bus) = runtime.events() {
            bus.emit(EventPayload::QuotaAlert(event));
        }
    }
}

pub(crate) fn spawn_watch(
    runtime: Arc<Runtime>,
    store: Arc<QuotaAlertStore>,
    wake: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(ALERT_POLL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                () = wake.notified() => {}
            }
            check_once(&runtime, &store).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use hofund_config::Config;
    use hofund_control_proto::{
        ControlErrorKind, EventEnvelope, EventPayload, Quota, QuotaAlertsSetRequest, QuotaKind,
        QuotaSpan, decode_response,
    };
    use hofund_router::quota::{WindowInfo, quota_time};

    use super::{set, spawn_watch};
    use crate::RegistryAdapter;
    use crate::events::EventBus;
    use crate::runtime::Runtime;

    fn config() -> Config {
        hofund_config::load(
            "[[providers]]\nid = \"relay\"\npreset = \"relay\"\nbase_url = \"http://127.0.0.1:9\"\napi_key = \"k\"\n\
             [[routes]]\nmodel = \"demo\"\nupstreams = [\"relay\"]\n",
        )
        .expect("config")
    }

    fn runtime(now: SystemTime) -> (Arc<Runtime>, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("runtime directory");
        let clock = Arc::new(move || now);
        let runtime = Runtime::new_with_quota_clock(
            Arc::new(RegistryAdapter),
            &root.path().join("hofund.toml"),
            Some(0),
            clock,
        );
        runtime.initial_snapshot(&config(), None).expect("snapshot");
        (runtime, root)
    }

    fn seed_subscription(runtime: &Runtime, user: &str, used: f64, now: SystemTime) {
        let reset = now + Duration::from_secs(3600);
        runtime.quotas().record_success(
            Quota {
                provider: "sub".to_owned(),
                name: "Sub".to_owned(),
                kind: QuotaKind::Subscription,
                user: user.to_owned(),
                ..Quota::default()
            },
            vec![
                WindowInfo::new(QuotaSpan {
                    name: "Weekly".to_owned(),
                    used,
                    remaining: (100.0 - used).max(0.0),
                    resets_at: Some(quota_time(reset)),
                    ..QuotaSpan::default()
                })
                .with_blocking(true),
            ],
            now,
        );
    }

    fn quota_alert_titles(bus: &EventBus) -> Vec<String> {
        bus.control_frames()
            .iter()
            .filter_map(|json| {
                decode_response::<EventEnvelope>(hofund_control_proto::API_VERSION, json.as_bytes())
                    .ok()
                    .and_then(|envelope| match envelope.payload {
                        EventPayload::QuotaAlert(event) => Some(event.title),
                        _ => None,
                    })
            })
            .collect()
    }

    #[test]
    fn al1_11_set_rejects_out_of_range_thresholds() {
        let store = hofund_router::quota_alerts::QuotaAlertStore::in_memory();
        let wake = tokio::sync::Notify::new();
        let over = set(
            &store,
            &wake,
            QuotaAlertsSetRequest {
                usage_alert: Some(101),
                ..QuotaAlertsSetRequest::new()
            },
        )
        .expect_err("usage over 100");
        assert_eq!(over.error.kind, ControlErrorKind::BadRequest);
        assert!(over.error.message.contains("usage alert"));

        let negative = set(
            &store,
            &wake,
            QuotaAlertsSetRequest {
                balance_alert: Some(-1.0),
                ..QuotaAlertsSetRequest::new()
            },
        )
        .expect_err("negative balance");
        assert_eq!(negative.error.kind, ControlErrorKind::BadRequest);

        let nan = set(
            &store,
            &wake,
            QuotaAlertsSetRequest {
                balance_alert: Some(f64::NAN),
                ..QuotaAlertsSetRequest::new()
            },
        )
        .expect_err("non-finite balance");
        assert_eq!(nan.error.kind, ControlErrorKind::BadRequest);
        assert_eq!(store.usage_alert(), 0);
        assert_eq!(store.balance_alert(), 0.0);
    }

    #[tokio::test(start_paused = true)]
    async fn al1_10_spawn_watch_emits_one_masked_quota_alert_per_window_run() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (runtime, _root) = runtime(now);
        seed_subscription(&runtime, "secret@example.test", 85.0, now);
        let bus = Arc::new(EventBus::with_boot_id(
            "00112233445566778899aabbccddeeff".into(),
        ));
        runtime.attach_events(Arc::clone(&bus));
        let store = Arc::clone(runtime.quota_alerts());
        let wake = Arc::clone(runtime.alert_wake());
        let task = spawn_watch(Arc::clone(&runtime), Arc::clone(&store), Arc::clone(&wake));

        let mut request = QuotaAlertsSetRequest::new();
        request.usage_alert = Some(80);
        assert!(set(&store, &wake, request).is_ok());
        tokio::task::yield_now().await;

        let titles = quota_alert_titles(&bus);
        assert_eq!(titles.len(), 1);
        assert!(titles[0].contains("se****et@example.test"));

        wake.notify_one();
        tokio::task::yield_now().await;
        assert_eq!(quota_alert_titles(&bus).len(), 1);

        task.abort();
    }
}
