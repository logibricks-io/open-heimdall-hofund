//! OS notifications for masked quota alerts from the embedded gateway.

use std::sync::OnceLock;

use hofund_daemon::QuotaAlertEvent;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

/// Result of attempting to show one alert.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PresentOutcome {
    /// The alert was handed to the OS notifier.
    Shown,
    /// Permission was denied or the notifier is unavailable.
    Suppressed,
}

/// Shows masked quota alerts with lazy OS notifier initialization.
pub(crate) trait QuotaAlertSurface: Send {
    /// Delivers `title` and `body` when allowed.
    fn present(&mut self, title: &str, body: &str) -> PresentOutcome;
}

/// Forwards one control `quota_alert` event to a surface.
pub(crate) fn present_alert<S: QuotaAlertSurface>(
    surface: &mut S,
    alert: &QuotaAlertEvent,
) -> PresentOutcome {
    surface.present(&alert.title, &alert.body)
}

/// Recording surface for unit tests.
#[derive(Clone, Debug, Default)]
pub(crate) struct RecordingSurface {
    pub calls: Vec<(String, String)>,
    pub denied: bool,
}

impl QuotaAlertSurface for RecordingSurface {
    fn present(&mut self, title: &str, body: &str) -> PresentOutcome {
        if self.denied {
            return PresentOutcome::Suppressed;
        }
        self.calls.push((title.to_owned(), body.to_owned()));
        PresentOutcome::Shown
    }
}

/// Lazily initialized notifier backed by the Tauri notification plugin.
pub(crate) struct OsQuotaAlertSurface {
    handle: AppHandle,
    ready: OnceLock<bool>,
    refused: bool,
}

impl OsQuotaAlertSurface {
    pub(crate) fn new(handle: AppHandle) -> Self {
        Self {
            handle,
            ready: OnceLock::new(),
            refused: false,
        }
    }
}

impl QuotaAlertSurface for OsQuotaAlertSurface {
    fn present(&mut self, title: &str, body: &str) -> PresentOutcome {
        if self.refused {
            return PresentOutcome::Suppressed;
        }
        if self.ready.get().is_none() {
            let allowed = self
                .handle
                .notification()
                .permission_state()
                .ok()
                .is_some_and(|state| state == tauri_plugin_notification::PermissionState::Granted)
                || self
                    .handle
                    .notification()
                    .request_permission()
                    .ok()
                    .is_some_and(|state| {
                        state == tauri_plugin_notification::PermissionState::Granted
                    });
            let _ = self.ready.set(allowed);
            if !allowed {
                self.refused = true;
                return PresentOutcome::Suppressed;
            }
        }
        if !self.ready.get().copied().unwrap_or(false) {
            return PresentOutcome::Suppressed;
        }
        match self
            .handle
            .notification()
            .builder()
            .title(title)
            .body(body)
            .show()
        {
            Ok(()) => PresentOutcome::Shown,
            Err(_) => PresentOutcome::Suppressed,
        }
    }
}

#[cfg(test)]
mod tests {
    use hofund_daemon::QuotaAlertEvent;

    use super::{PresentOutcome, QuotaAlertSurface, RecordingSurface, present_alert};

    #[test]
    fn d310_present_forwards_masked_title_and_body() {
        let mut surface = RecordingSurface::default();
        let mut alert = QuotaAlertEvent::default();
        alert.title = "se****et".to_owned();
        alert.body = "80% weekly".to_owned();
        let outcome = present_alert(&mut surface, &alert);
        assert_eq!(outcome, PresentOutcome::Shown);
        assert_eq!(surface.calls, vec![(alert.title, alert.body)]);
    }

    #[test]
    fn d310_denied_surface_suppresses_without_panic() {
        let mut surface = RecordingSurface {
            denied: true,
            ..RecordingSurface::default()
        };
        let mut alert = QuotaAlertEvent::default();
        alert.title = "t".to_owned();
        alert.body = "b".to_owned();
        assert_eq!(
            present_alert(&mut surface, &alert),
            PresentOutcome::Suppressed
        );
        assert!(surface.calls.is_empty());
    }
}
