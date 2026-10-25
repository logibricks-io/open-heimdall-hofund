//! Immutable router snapshot.
//!
//! In-flight requests keep the [`Arc`] they loaded. A later store does not
//! change that arc. Publishing a new snapshot on config reload is pull
//! request L. This type is the startup slot, and the slot a panic publishes
//! into when a member is removed.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::Router;

/// The router a request binds for its whole call.
#[derive(Clone)]
pub struct Snapshot {
    inner: Arc<ArcSwap<Router>>,
    /// Keeps the daemon membership table alive so a panic can publish the
    /// next snapshot. Absent for a snapshot built in a test.
    _owner: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl Snapshot {
    /// A snapshot holding `router`.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self {
            inner: Arc::new(ArcSwap::new(Arc::new(router))),
            _owner: None,
        }
    }

    /// A clone of this slot that also keeps `owner` alive.
    ///
    /// The daemon passes the membership table. Members hold a weak pointer
    /// back to it and publish here after a panic. The table's own snapshot
    /// handle does not own the table, so the arc is not a cycle.
    #[must_use]
    pub fn hold(&self, owner: Arc<dyn std::any::Any + Send + Sync>) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _owner: Some(owner),
        }
    }

    /// The router to use for a call that starts now.
    ///
    /// Hold the [`Arc`] until the call finishes. A store during the call
    /// leaves this value unchanged.
    #[must_use]
    pub fn load(&self) -> Arc<Router> {
        self.inner.load_full()
    }

    /// Replaces the router the next call will load.
    pub fn store(&self, router: Router) {
        self.inner.store(Arc::new(router));
    }
}
