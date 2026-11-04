//! Daemon-side quota alert settings and periodic checks.

use std::sync::Arc;

use hofund_control_proto::{
    EventPayload, QuotaAlertEvent, QuotaAlertsResponse, QuotaAlertsSetRequest,
    QuotaAlertsSetResponse,
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
            if !store.any_enabled() {
                continue;
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
    })
}
