//! Process-global memory of model ids served only in Max Mode.

use std::collections::HashSet;
use std::sync::Mutex;
#[cfg(test)]
use std::sync::MutexGuard;

static MAX_ONLY: Mutex<Option<HashSet<String>>> = Mutex::new(None);
#[cfg(test)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn slot() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    MAX_ONLY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether a later execute for this model id should start in Max Mode.
pub(crate) fn contains(model_id: &str) -> bool {
    slot().as_ref().is_some_and(|ids| ids.contains(model_id))
}

/// Remembers that this model id is served only in Max Mode.
pub(crate) fn remember(model_id: &str) {
    slot()
        .get_or_insert_with(HashSet::new)
        .insert(model_id.to_owned());
}

/// Forgets remembered ids so tests start from a clean process set.
#[cfg(test)]
pub(crate) fn clear() {
    *slot() = None;
}

/// Serializes tests that mutate the process-global set.
#[cfg(test)]
pub(crate) fn test_lock() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
