//! OS notifications for masked quota alerts from the embedded gateway.
//!
//! The Tauri notification plugin pulls extra `zbus` generations that
//! `cargo deny` rejects, and those versions cannot be unified with the
//! existing tree. The menu-bar app therefore calls the platform helpers
//! already present on a typical desktop (`notify-send`, `osascript`, or
//! Windows Runtime via PowerShell) and stays within the current crates.

use std::process::Command;

use hofund_daemon::QuotaAlertEvent;

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

/// Environment keys used so macOS and Windows scripts never interpolate the
/// title or body into a shell string.
const TITLE_ENV: &str = "HOFUND_NOTIFY_TITLE";
const BODY_ENV: &str = "HOFUND_NOTIFY_BODY";

/// Lazily initialized notifier backed by a platform helper command.
#[derive(Default)]
pub(crate) struct OsQuotaAlertSurface {
    refused: bool,
}

impl OsQuotaAlertSurface {
    pub(crate) fn new() -> Self {
        Self { refused: false }
    }
}

impl QuotaAlertSurface for OsQuotaAlertSurface {
    fn present(&mut self, title: &str, body: &str) -> PresentOutcome {
        if self.refused {
            return PresentOutcome::Suppressed;
        }
        match show_os_notification(title, body) {
            true => PresentOutcome::Shown,
            false => {
                self.refused = true;
                PresentOutcome::Suppressed
            }
        }
    }
}

fn show_os_notification(title: &str, body: &str) -> bool {
    let mut command = match notify_command(title, body) {
        Some(command) => command,
        None => return false,
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Platform helper used to show a notification.
pub(crate) fn notify_command(title: &str, body: &str) -> Option<Command> {
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("notify-send");
        command.args(["--", title, body]);
        Some(command)
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("osascript");
        command
            .env(TITLE_ENV, title)
            .env(BODY_ENV, body)
            .args([
                "-e",
                r#"display notification (system attribute "HOFUND_NOTIFY_BODY") with title (system attribute "HOFUND_NOTIFY_TITLE")"#,
            ]);
        Some(command)
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("powershell");
        command
            .env(TITLE_ENV, title)
            .env(BODY_ENV, body)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                r#"$t=$env:HOFUND_NOTIFY_TITLE; $b=$env:HOFUND_NOTIFY_BODY; try { [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; $xml = New-Object Windows.Data.Xml.Dom.XmlDocument; $xml.LoadXml("<toast><visual><binding template='ToastGeneric'><text>" + [System.Security.SecurityElement]::Escape($t) + "</text><text>" + [System.Security.SecurityElement]::Escape($b) + "</text></binding></visual></toast>"); $toast = [Windows.UI.Notifications.ToastNotification]::new($xml); [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier("Hofund").Show($toast) } catch { exit 1 }"#,
            ]);
        Some(command)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (title, body);
        None
    }
}

/// Program name for the current OS notifier, if any.
pub(crate) fn notify_program() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        Some("notify-send")
    }
    #[cfg(target_os = "macos")]
    {
        Some("osascript")
    }
    #[cfg(target_os = "windows")]
    {
        Some("powershell")
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use hofund_daemon::QuotaAlertEvent;

    use super::{
        PresentOutcome, QuotaAlertSurface, RecordingSurface, notify_command, notify_program,
        present_alert,
    };

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

    #[test]
    fn d310_notify_command_uses_the_platform_helper_and_keeps_text_out_of_the_shell() {
        let program = notify_program().expect("desktop CI covers linux/mac/windows");
        let title = r#"80% "weekly""#;
        let body = "se****et";
        let command = notify_command(title, body).expect("command");
        assert_eq!(command.get_program(), program);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let envs: Vec<(String, String)> = command
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                args,
                vec!["--".to_owned(), title.to_owned(), body.to_owned()]
            );
            assert!(envs.is_empty());
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            assert!(
                !args
                    .iter()
                    .any(|arg| arg.contains(title) || arg.contains(body)),
                "title/body must be passed via env, not argv: {args:?}"
            );
            assert!(
                envs.iter()
                    .any(|(k, v)| k == super::TITLE_ENV && v == title)
            );
            assert!(envs.iter().any(|(k, v)| k == super::BODY_ENV && v == body));
        }
    }
}
