//! Bag of JSON fields this version of the IR does not model.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Unknown JSON fields preserved so a same-protocol passthrough does not drop them.
///
/// Codecs lift this map back onto the protocol object when they encode. Keys that
/// the codec already wrote are left alone; known fields win over the bag.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Extensions(Map<String, Value>);

impl Extensions {
    /// An empty bag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the bag has no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrows the field named `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Inserts `value` under `key`, replacing a previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) {
        self.0.insert(key.into(), value);
    }

    /// Removes every field.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Iterates fields in the order they were decoded.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    /// Wraps a JSON object that is already only unknown fields.
    #[must_use]
    pub fn from_map(fields: Map<String, Value>) -> Self {
        Self(fields)
    }

    /// Borrows the underlying object.
    #[must_use]
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    /// Copies fields onto `object` without replacing keys that are already set.
    pub fn merge_into(&self, object: &mut Map<String, Value>) {
        for (key, value) in &self.0 {
            object.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }

    /// Fills gaps from `patch`. Keys already set on `self` stay in place.
    pub fn absorb(&mut self, patch: &Self) {
        let mut combined = self.0.clone();
        patch.merge_into(&mut combined);
        self.0 = combined;
    }
}
