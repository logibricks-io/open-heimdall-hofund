//! Work that finishes even when its caller goes away.

use std::future::Future;

use hofund_plugin_api::{PluginError, SchedulerHandle};

/// Runs `work` as a one-shot job and returns its result.
///
/// The job belongs to the kernel, so dropping the returned future does not
/// cancel `work`. Use it for a token refresh: a rotated refresh token must
/// be stored even when the request that asked for it was cancelled.
///
/// # Errors
///
/// Returns the error `work` returned, or [`PluginError`] when the scheduler
/// refused the job or cancelled it before it finished.
pub async fn detached<T, F>(
    scheduler: &SchedulerHandle,
    name: &str,
    work: F,
) -> Result<T, PluginError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, PluginError>> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    scheduler.run_once(
        name,
        Box::pin(async move {
            // The caller may be gone; the work's side effects are what matter.
            let _ = sender.send(work.await);
        }),
    )?;
    receiver
        .await
        .map_err(|_| PluginError::cancelled("background job stopped before it finished"))?
}
