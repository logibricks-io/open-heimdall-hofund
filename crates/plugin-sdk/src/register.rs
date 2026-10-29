//! What a plugin hands the registry.
//!
//! A plugin exports `contribute() -> Contribution`. The registry calls that
//! function. The plugin does not depend on the registry or the host.

use std::sync::Arc;

use hofund_plugin_api::{DynSubscriptionFactory, DynUpstreamFactory, InboundSurface};

/// Factories and surfaces one plugin contributes.
#[derive(Default)]
pub struct Contribution {
    upstreams: Vec<Arc<dyn DynUpstreamFactory>>,
    surfaces: Vec<Arc<dyn InboundSurface>>,
    subscriptions: Vec<Arc<dyn DynSubscriptionFactory>>,
}

impl Contribution {
    /// An empty contribution.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an upstream factory.
    #[must_use]
    pub fn upstream(mut self, factory: impl DynUpstreamFactory) -> Self {
        self.upstreams.push(Arc::new(factory));
        self
    }

    /// Adds an inbound surface.
    #[must_use]
    pub fn surface(mut self, surface: impl InboundSurface) -> Self {
        self.surfaces.push(Arc::new(surface));
        self
    }

    /// Adds a subscription factory.
    #[must_use]
    pub fn subscription(mut self, factory: impl DynSubscriptionFactory) -> Self {
        self.subscriptions.push(Arc::new(factory));
        self
    }

    /// Upstream factories, in the order they were added.
    #[must_use]
    pub fn upstreams(&self) -> &[Arc<dyn DynUpstreamFactory>] {
        &self.upstreams
    }

    /// Inbound surfaces, in the order they were added.
    #[must_use]
    pub fn surfaces(&self) -> &[Arc<dyn InboundSurface>] {
        &self.surfaces
    }

    /// Subscription factories, in the order they were added.
    #[must_use]
    pub fn subscriptions(&self) -> &[Arc<dyn DynSubscriptionFactory>] {
        &self.subscriptions
    }
}
