//! Client identity headers the reference sends as the Cline desktop app.

/// Desktop client type Cline's API keys free-model lists on.
pub(crate) const CLIENT_TYPE: &str = "cline-desktop";

/// Pinned Cline desktop release (move when the reference bumps ClineVersion).
pub(crate) const CLIENT_VERSION: &str = "3.5.54";

/// Headers every Cline API call carries, optionally with a task id.
#[must_use]
pub(crate) fn headers(task_id: Option<&str>) -> Vec<(String, String)> {
    let mut out = vec![
        ("HTTP-Referer".to_owned(), "https://cline.bot".to_owned()),
        ("X-Title".to_owned(), "Cline".to_owned()),
        ("X-IS-MULTIROOT".to_owned(), "false".to_owned()),
        ("X-CLIENT-TYPE".to_owned(), CLIENT_TYPE.to_owned()),
        ("X-CLIENT-VERSION".to_owned(), CLIENT_VERSION.to_owned()),
        ("User-Agent".to_owned(), format!("Cline/{CLIENT_VERSION}")),
    ];
    if let Some(task) = task_id.filter(|id| !id.is_empty()) {
        out.push(("X-Task-ID".to_owned(), task.to_owned()));
    }
    out
}
