//! A per-account token cache with single-flight refresh.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use hofund_plugin_api::{AccountId, PluginError, PluginErrorKind};

use super::Clock;

/// How long a failed refresh is remembered. Callers inside the window get
/// the same error without another request.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(5);

/// Cached tokens, one slot per account.
///
/// [`TokenCache::get`] returns the cached value while it is outside the
/// margin before expiry. Otherwise one caller refreshes while the others for
/// the same account wait, then share its result. A failed refresh is
/// remembered for [`NEGATIVE_TTL`].
pub struct TokenCache<T> {
    margin: Duration,
    slots: Mutex<HashMap<AccountId, Arc<tokio::sync::Mutex<Slot<T>>>>>,
}

struct Slot<T> {
    value: Option<(T, Option<SystemTime>)>,
    failure: Option<Failure>,
}

struct Failure {
    at: Duration,
    kind: PluginErrorKind,
    status: Option<u16>,
    retry_after: Option<Duration>,
    message: String,
}

impl Failure {
    fn new(at: Duration, error: &PluginError) -> Self {
        Self {
            at,
            kind: error.kind(),
            status: error.status(),
            retry_after: error.retry_after(),
            message: error.message().to_owned(),
        }
    }

    fn replay(&self) -> PluginError {
        let message = self.message.clone();
        let error = match (self.kind, self.status) {
            (PluginErrorKind::Auth, _) => PluginError::auth(message),
            (PluginErrorKind::Quota, Some(status)) => PluginError::quota_status(status, message),
            (PluginErrorKind::Quota, None) => PluginError::quota(message),
            (PluginErrorKind::Status, Some(status)) => PluginError::status_error(status, message),
            (PluginErrorKind::Connect, _) => PluginError::connect(message),
            (PluginErrorKind::Timeout, _) => PluginError::timeout(message),
            (PluginErrorKind::Transport, _) => PluginError::transport(message),
            (PluginErrorKind::Interrupted, _) => PluginError::interrupted(message),
            (PluginErrorKind::Decode, _) => PluginError::decode(message),
            (PluginErrorKind::Unsupported, _) => PluginError::unsupported(message),
            (PluginErrorKind::Config, _) => PluginError::config(message),
            (PluginErrorKind::Cancelled, _) => PluginError::cancelled(message),
            (PluginErrorKind::Io, _) => PluginError::io(message),
            _ => PluginError::internal(message),
        };
        match self.retry_after {
            Some(after) => error.with_retry_after(after),
            None => error,
        }
    }
}

impl<T: Clone> TokenCache<T> {
    /// An empty cache that refreshes `margin` before expiry.
    #[must_use]
    pub fn new(margin: Duration) -> Self {
        Self {
            margin,
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn slot(&self, account: &AccountId) -> Arc<tokio::sync::Mutex<Slot<T>>> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(slots.entry(account.clone()).or_insert_with(|| {
            Arc::new(tokio::sync::Mutex::new(Slot {
                value: None,
                failure: None,
            }))
        }))
    }

    /// The cached value for `account`, or the result of `refresh`.
    ///
    /// `refresh` returns the value and when it expires (`None` for never).
    /// It runs at most once at a time per account; callers that arrive while
    /// it runs get its result.
    ///
    /// # Errors
    ///
    /// Returns the error `refresh` returned, now or within the last
    /// [`NEGATIVE_TTL`].
    pub async fn get<C, F, Fut>(
        &self,
        clock: &C,
        account: &AccountId,
        refresh: F,
    ) -> Result<T, PluginError>
    where
        C: Clock + ?Sized,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(T, Option<SystemTime>), PluginError>>,
    {
        let slot = self.slot(account);
        let mut slot = slot.lock().await;
        if let Some((value, expires_at)) = &slot.value {
            let fresh = expires_at.is_none_or(|at| clock.now() + self.margin < at);
            if fresh {
                return Ok(value.clone());
            }
        }
        if let Some(failure) = &slot.failure
            && clock.monotonic().saturating_sub(failure.at) < NEGATIVE_TTL
        {
            return Err(failure.replay());
        }
        match refresh().await {
            Ok((value, expires_at)) => {
                slot.value = Some((value.clone(), expires_at));
                slot.failure = None;
                Ok(value)
            }
            Err(error) => {
                slot.failure = Some(Failure::new(clock.monotonic(), &error));
                Err(error)
            }
        }
    }

    /// Drops the cached value for `account` when `stale` says it is the one
    /// that failed.
    ///
    /// Call it after the upstream rejects a token, passing a check against
    /// the token that was sent. If another caller already refreshed, the new
    /// value does not match and stays. Returns whether a value was dropped.
    pub async fn invalidate_if(&self, account: &AccountId, stale: impl FnOnce(&T) -> bool) -> bool {
        let slot = self.slot(account);
        let mut slot = slot.lock().await;
        let matches = slot.value.as_ref().is_some_and(|(value, _)| stale(value));
        if matches {
            slot.value = None;
        }
        matches
    }

    /// Forgets `account`, for sign-out.
    pub fn remove(&self, account: &AccountId) {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(account);
    }
}

#[cfg(test)]
mod tests;
