//! Control event application (section 2 "State").

use hofund_control_client::hofund_control_proto::{
    ConfigRejectedEvent, ConfigReloadedEvent, EventPayload, ModelsRefreshedEvent, QuotaAlertEvent,
    SignInProgressEvent, SnapshotEvent,
};

use crate::app::App;
use crate::state::{Flash, PageId};

/// Applies a control event without refetching when the payload is enough.
pub fn apply_control_event(app: &mut App, payload: &EventPayload) {
    match payload {
        EventPayload::Snapshot(event) => apply_snapshot(app, event),
        EventPayload::ConfigReloaded(_) | EventPayload::ConfigRejected(_) => {
            let ok = matches!(payload, EventPayload::ConfigReloaded(_));
            let message = match payload {
                EventPayload::ConfigReloaded(ConfigReloadedEvent { .. }) => {
                    "configuration reloaded".to_owned()
                }
                EventPayload::ConfigRejected(ConfigRejectedEvent { issues, .. }) => {
                    let detail = issues
                        .first()
                        .map(|issue| issue.message.clone())
                        .unwrap_or_else(|| "configuration rejected".to_owned());
                    format!("configuration rejected: {detail}")
                }
                _ => unreachable!(),
            };
            app.set_flash(Flash::new(message, ok));
            let _ = app.reload_open_page();
        }
        EventPayload::ModelsRefreshed(ModelsRefreshedEvent { .. }) => {
            if app.page == PageId::Providers {
                let _ = app.reload_open_page();
                app.refresh_open_picker();
            }
        }
        EventPayload::AgentChanged(_) => {
            if app.page == PageId::Agents {
                let _ = app.reload_open_page();
            }
        }
        EventPayload::SignInProgress(SignInProgressEvent { .. }) => {
            // Accounts sign-in polling is wired in R-15.
        }
        EventPayload::QuotaAlert(QuotaAlertEvent { title, body, .. }) => {
            let text = if body.is_empty() {
                title.clone()
            } else {
                format!("{title}: {body}")
            };
            app.set_flash(Flash::new(text, false));
        }
        EventPayload::TokenRotated(_) => {
            app.request_token_reread();
        }
        EventPayload::ResyncRequired(_) => {
            app.request_event_resync();
        }
        EventPayload::RequestStarted(_) | EventPayload::RequestFinished(_) => {
            // Logs page only in later work; Usage G-11 refetch is R-15.
        }
        EventPayload::ListenerRelistenFailed(_)
        | EventPayload::MemberCooled(_)
        | EventPayload::MemberRecovered(_)
        | EventPayload::SnapshotPublished(_)
        | EventPayload::ShutdownStarted(_)
        | EventPayload::Unknown => {}
        _ => {}
    }
}

fn apply_snapshot(app: &mut App, event: &SnapshotEvent) {
    app.disk_revision = event.disk_revision.clone();
    app.in_flight = event.in_flight;
    app.effective_listen = event.effective_listen.clone();
}

#[cfg(test)]
mod tests {
    use hofund_control_client::hofund_control_proto::{
        ConfigRejectedEvent, EventPayload, FieldIssue, QuotaAlertEvent, SnapshotEvent,
    };

    use crate::app::App;
    use crate::events::apply_control_event;
    use crate::state::PageId;

    #[test]
    fn snapshot_updates_revision_and_in_flight() {
        let mut app = App::for_test(PageId::Agents);
        let mut event = SnapshotEvent::default();
        event.disk_revision = "abc".to_owned();
        event.in_flight = 3;
        event.effective_listen = "127.0.0.1:3425".to_owned();
        apply_control_event(&mut app, &EventPayload::Snapshot(event));
        assert_eq!(app.disk_revision, "abc");
        assert_eq!(app.in_flight, 3);
        assert_eq!(app.effective_listen, "127.0.0.1:3425");
    }

    #[test]
    fn config_rejected_toasts_and_schedules_reload() {
        let mut app = App::for_test(PageId::Providers);
        app.reloads = 0;
        let mut event = ConfigRejectedEvent::default();
        event.issues = vec![FieldIssue::new("listen", "invalid", "bad port")];
        apply_control_event(&mut app, &EventPayload::ConfigRejected(event));
        assert!(app.flash.as_ref().is_some_and(|f| !f.ok));
        assert_eq!(app.reloads, 1);
    }

    #[test]
    fn quota_alert_replaces_flash() {
        let mut app = App::for_test(PageId::Usage);
        app.set_flash(crate::state::Flash::new("old", true));
        let mut alert = QuotaAlertEvent::default();
        alert.title = "limit".to_owned();
        alert.body = "90%".to_owned();
        apply_control_event(&mut app, &EventPayload::QuotaAlert(alert));
        assert_eq!(
            app.flash.as_ref().map(|f| f.text.as_str()),
            Some("limit: 90%")
        );
    }
}
