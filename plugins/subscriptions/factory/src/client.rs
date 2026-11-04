//! Client identity headers Factory's CLI sends.

/// Factory CLI client type.
pub(crate) const CLIENT_TYPE: &str = "cline-cli";

/// Pinned Factory CLI release (move when Factory ships a new one).
pub(crate) const CLIENT_VERSION: &str = "3.0.68";

/// Platform label.
pub(crate) const PLATFORM: &str = "cli";

/// Core version Factory reports.
pub(crate) const CORE_VERSION: &str = "0.0.90";

/// Headers every Factory API call carries, optionally with a task id.
#[must_use]
pub(crate) fn headers(task_id: Option<&str>) -> Vec<(String, String)> {
    let mut out = vec![
        ("accept".to_owned(), "application/json".to_owned()),
        ("HTTP-Referer".to_owned(), "https://factory.ai".to_owned()),
        ("X-Title".to_owned(), "Factory".to_owned()),
        ("User-Agent".to_owned(), format!("Factory/{CLIENT_VERSION}")),
        ("X-IS-MULTIROOT".to_owned(), "false".to_owned()),
        ("X-CLIENT-TYPE".to_owned(), CLIENT_TYPE.to_owned()),
        ("X-CLIENT-VERSION".to_owned(), CLIENT_VERSION.to_owned()),
        ("X-PLATFORM".to_owned(), PLATFORM.to_owned()),
        ("X-PLATFORM-VERSION".to_owned(), CLIENT_VERSION.to_owned()),
        ("X-CORE-VERSION".to_owned(), CORE_VERSION.to_owned()),
    ];
    if let Some(task) = task_id.filter(|id| !id.is_empty()) {
        out.push(("X-Task-ID".to_owned(), task.to_owned()));
    }
    out
}
