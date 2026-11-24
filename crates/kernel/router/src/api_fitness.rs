//! Process-local memory of APIs a provider refuses for every model.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// Provider/protocol pairs that must not be asked again in this process.
///
/// The daemon shares this value across router snapshots. It deliberately has
/// no persistent representation: a restart lets the provider serve the API again.
#[derive(Clone, Default)]
pub struct ApiFitness {
    refused: Arc<Mutex<BTreeSet<(String, String)>>>,
}

impl ApiFitness {
    pub(crate) fn refuse(&self, provider: &str, protocol: &str) {
        self.refused
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert((provider.to_owned(), protocol.to_owned()));
    }

    pub(crate) fn is_refused(&self, provider: &str, protocol: &str) -> bool {
        self.refused
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&(provider.to_owned(), protocol.to_owned()))
    }
}
