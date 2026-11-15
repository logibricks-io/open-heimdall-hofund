//! OS notifications for masked quota alerts from the embedded gateway.
//!
//! The Tauri notification plugin pulls extra `zbus` generations that
//! `cargo deny` rejects, and those versions cannot be unified with the
//! existing tree. The menu-bar app therefore calls the platform helpers
//! already present on a typical desktop (`notify-send`, `swift`, or
//! Windows Runtime via PowerShell) and stays within the current crates.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use hofund_daemon::QuotaAlertEvent;

/// Freedesktop action key for a click on the notification body (Linux).
pub(crate) const LINUX_CLICK_ACTION: &str = "default";

/// Notification category whose default action comes back as [`LINUX_CLICK_ACTION`].
pub(crate) const CLICK_CATEGORY_ID: &str = "hofund-click";

/// Default action identifier for macOS and Windows responses (matches the reference).
pub(crate) const DEFAULT_ACTION: &str = "default";

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

/// Whether activating a quota-alert notification should focus Usage.
///
/// On Linux only the click category's `default` action opens Usage; dismissal
/// must not. macOS and Windows treat the default activation as Usage, matching
/// the reference's alert behavior.
pub(crate) fn quota_alert_click_focuses_usage(host_os: &str, action: &str) -> bool {
    match host_os {
        "linux" => action == LINUX_CLICK_ACTION,
        "darwin" | "windows" => action == DEFAULT_ACTION,
        _ => false,
    }
}

#[cfg(any(test, target_os = "linux", target_os = "macos", target_os = "windows"))]
pub(crate) fn host_os_for_notify() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "other"
    }
}

/// Recording surface for unit tests.
#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct RecordingSurface {
    pub calls: Vec<(String, String)>,
    pub denied: bool,
}

#[cfg(test)]
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
#[cfg(any(target_os = "macos", target_os = "windows"))]
const TITLE_ENV: &str = "HOFUND_NOTIFY_TITLE";
#[cfg(any(target_os = "macos", target_os = "windows"))]
const BODY_ENV: &str = "HOFUND_NOTIFY_BODY";

#[cfg(target_os = "macos")]
const MACOS_CLICK_SCRIPT: &str = r#"import Foundation
import UserNotifications
final class Delegate: NSObject, UNUserNotificationCenterDelegate {
    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse, withCompletionHandler completionHandler: @escaping () -> Void) {
        if response.actionIdentifier == UNNotificationDefaultActionIdentifier {
            fputs("default\n", stdout)
        }
        completionHandler()
        CFRunLoopStop(CFRunLoopGetMain())
    }
}
guard let title = ProcessInfo.processInfo.environment["HOFUND_NOTIFY_TITLE"],
      let body = ProcessInfo.processInfo.environment["HOFUND_NOTIFY_BODY"] else { exit(1) }
let delegate = Delegate()
UNUserNotificationCenter.current().delegate = delegate
let auth = DispatchSemaphore(value: 0)
UNUserNotificationCenter.current().requestAuthorization(options: [.alert]) { _, _ in auth.signal() }
auth.wait()
let content = UNMutableNotificationContent()
content.title = title
content.body = body
let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
UNUserNotificationCenter.current().add(request)
RunLoop.main.run(until: Date(timeIntervalSinceNow: 3600))
"#;

/// Lazily initialized notifier backed by a platform helper command.
#[derive(Default)]
pub(crate) struct OsQuotaAlertSurface {
    refused: bool,
}

impl OsQuotaAlertSurface {
    pub(crate) fn new() -> Self {
        Self { refused: false }
    }

    /// Shows a quota alert and focuses Usage when the user activates it.
    pub(crate) fn present_with_focus(
        &mut self,
        title: &str,
        body: &str,
        focus_usage: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> PresentOutcome {
        if self.refused {
            return PresentOutcome::Suppressed;
        }
        let Some(command) = notify_command(title, body) else {
            self.refused = true;
            return PresentOutcome::Suppressed;
        };
        spawn_notify_wait(command, focus_usage);
        PresentOutcome::Shown
    }
}

impl QuotaAlertSurface for OsQuotaAlertSurface {
    fn present(&mut self, title: &str, body: &str) -> PresentOutcome {
        self.present_with_focus(title, body, Arc::new(|| {}))
    }
}

fn spawn_notify_wait(mut command: Command, focus_usage: Arc<dyn Fn() + Send + Sync + 'static>) {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(target_os = "macos")]
    {
        command.stdin(Stdio::piped());
    }
    #[cfg(not(target_os = "macos"))]
    {
        command.stdin(Stdio::null());
    }
    thread::spawn(move || {
        #[cfg(target_os = "macos")]
        let output = (|| {
            let mut child = command.spawn().ok()?;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(MACOS_CLICK_SCRIPT.as_bytes());
            }
            child.wait_with_output().ok()
        })();
        #[cfg(not(target_os = "macos"))]
        let output = command.output().ok();
        let action = output
            .as_ref()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        if quota_alert_click_focuses_usage(host_os_for_notify(), &action) {
            focus_usage();
        }
    });
}

/// Platform helper used to show a notification and wait for activation.
pub(crate) fn notify_command(title: &str, body: &str) -> Option<Command> {
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("notify-send");
        command.args([
            "--app-name=Hofund",
            &format!("--category={CLICK_CATEGORY_ID}"),
            &format!("--action={LINUX_CLICK_ACTION}=Open"),
            "--",
            title,
            body,
        ]);
        Some(command)
    }
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("swift");
        command.env(TITLE_ENV, title).env(BODY_ENV, body).arg("-");
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
                r#"$t=$env:HOFUND_NOTIFY_TITLE; $b=$env:HOFUND_NOTIFY_BODY; try { [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; $xml = New-Object Windows.Data.Xml.Dom.XmlDocument; $xml.LoadXml("<toast activationType='foreground' launch='usage'><visual><binding template='ToastGeneric'><text>" + [System.Security.SecurityElement]::Escape($t) + "</text><text>" + [System.Security.SecurityElement]::Escape($b) + "</text></binding></visual></toast>"); $toast = [Windows.UI.Notifications.ToastNotification]::new($xml); $done = New-Object System.Threading.ManualResetEventSlim($false); $handler = [Windows.Foundation.TypedEventHandler[Windows.UI.Notifications.ToastNotification, Windows.UI.Notifications.ToastActivatedEventArgs]]{ param($s,$e) [Console]::Out.WriteLine('default'); $done.Set() }; $toast.add_Activated($handler); [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Hofund').Show($toast); $null = $done.Wait(3600000) } catch { exit 1 }"#,
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
#[cfg(test)]
pub(crate) fn notify_program() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        Some("notify-send")
    }
    #[cfg(target_os = "macos")]
    {
        Some("swift")
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
        CLICK_CATEGORY_ID, DEFAULT_ACTION, LINUX_CLICK_ACTION, PresentOutcome, RecordingSurface,
        notify_command, notify_program, present_alert, quota_alert_click_focuses_usage,
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
    fn d310_dismissed_quota_alert_opens_nothing_on_linux() {
        for (host_os, action, want) in [
            ("linux", "", false),
            ("linux", DEFAULT_ACTION, true),
            ("darwin", DEFAULT_ACTION, true),
            ("windows", DEFAULT_ACTION, true),
            ("linux", "reply", false),
        ] {
            assert_eq!(
                quota_alert_click_focuses_usage(host_os, action),
                want,
                "os={host_os} action={action:?}"
            );
        }
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn d310_non_linux_notify_has_no_freedesktop_click_category() {
        let args: Vec<_> = notify_command("t", "b")
            .expect("command")
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args.iter().any(|arg| arg.contains(CLICK_CATEGORY_ID)),
            "args: {args:?}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn d310_linux_alert_sent_under_click_category() {
        let command = notify_command("t", "b").expect("linux CI");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .any(|arg| arg == &format!("--category={CLICK_CATEGORY_ID}")),
            "args: {args:?}"
        );
        assert!(
            args.iter()
                .any(|arg| { arg == &format!("--action={LINUX_CLICK_ACTION}=Open") }),
            "args: {args:?}"
        );
        assert_eq!(LINUX_CLICK_ACTION, "default");
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
            assert!(args.ends_with(&["--".to_owned(), title.to_owned(), body.to_owned()]));
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
