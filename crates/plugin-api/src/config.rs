//! Config values a factory validates before `build`.

use std::collections::BTreeMap;

use crate::error::PluginError;

/// Marker for a config value a factory has already validated.
///
/// It is not [`Any`](std::any::Any). The erased factory calls `validate` and
/// then `build` on the typed value, so nothing downcasts.
pub trait ValidConfig: Send + Sync + 'static {}

/// A config value the factory rejected.
///
/// [`ConfigError::path`] is a dotted field path such as `options.project`.
/// It is empty when the rejection is not about one field.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfigError {
    path: String,
    message: String,
}

impl ConfigError {
    /// A rejection with `message` and an empty field path.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            path: String::new(),
            message: message.into(),
        }
    }

    /// Sets the field path. `path` is dotted, such as `options.region`.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// The field path, or an empty string when the rejection is not about one field.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The rejection text, without the field path.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            formatter.write_str(&self.message)
        } else {
            write!(formatter, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

/// `build` failed, or `validate` failed on the erased path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
#[error("{message}")]
pub struct BuildError {
    message: String,
}

impl BuildError {
    /// A build failure with `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The failure text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<ConfigError> for BuildError {
    fn from(error: ConfigError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<BuildError> for PluginError {
    fn from(error: BuildError) -> Self {
        PluginError::config(error.message)
    }
}

/// One JSON-like config value.
///
/// Numbers are integers. A later variant can add a float without a breaking
/// match, because the enum is `#[non_exhaustive]`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RawValue {
    /// A string.
    String(String),
    /// An integer.
    Integer(i64),
    /// A boolean.
    Boolean(bool),
    /// An ordered list.
    Array(Vec<RawValue>),
    /// A nested table.
    Table(RawTable),
}

/// The opaque `[providers.options]` or `[subscriptions.options]` table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawTable {
    entries: BTreeMap<String, RawValue>,
}

impl RawTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Inserts `value` at `key`, replacing a previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: RawValue) {
        self.entries.insert(key.into(), value);
    }

    /// The value at `key`, if any.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&RawValue> {
        self.entries.get(key)
    }

    /// Whether the table has no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every key and value, in key order.
    ///
    /// A factory walks this to reject unknown keys.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &RawValue)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value))
    }

    /// The keys, in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// A form the desktop can render without matching on a vendor id.
///
/// This is a small ordered schema, not the `schemars` crate. PR B records
/// that choice: the form needs objects, strings, booleans, integers, and
/// arrays, and `schemars` would pull a second copy of crates the menu-bar
/// stack already duplicates.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Schema {
    root: SchemaNode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SchemaNode {
    Object { fields: Vec<SchemaField> },
    String,
    Boolean,
    Integer,
    Array(Box<SchemaNode>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SchemaField {
    name: String,
    required: bool,
    value: SchemaNode,
}

impl Schema {
    /// An object with no fields.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            root: SchemaNode::Object { fields: Vec::new() },
        }
    }

    /// A string field.
    #[must_use]
    pub fn string() -> Self {
        Self {
            root: SchemaNode::String,
        }
    }

    /// A boolean field.
    #[must_use]
    pub fn boolean() -> Self {
        Self {
            root: SchemaNode::Boolean,
        }
    }

    /// An integer field.
    #[must_use]
    pub fn integer() -> Self {
        Self {
            root: SchemaNode::Integer,
        }
    }

    /// An array of `item`.
    #[must_use]
    pub fn array(item: Self) -> Self {
        Self {
            root: SchemaNode::Array(Box::new(item.root)),
        }
    }

    /// An object. `required` names fields that must be present.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a field name is empty, repeated, or listed
    /// as required without a field of that name.
    pub fn object(fields: Vec<(String, Schema)>, required: &[&str]) -> Result<Self, ConfigError> {
        let mut built = Vec::with_capacity(fields.len());
        for (name, schema) in fields {
            if name.is_empty() {
                return Err(ConfigError::new("schema field name is empty"));
            }
            if built.iter().any(|field: &SchemaField| field.name == name) {
                return Err(ConfigError::new("schema field name is repeated"));
            }
            let is_required = required.contains(&name.as_str());
            built.push(SchemaField {
                name,
                required: is_required,
                value: schema.root,
            });
        }
        for name in required {
            if !built.iter().any(|field| field.name == *name) {
                return Err(ConfigError::new("required schema field is missing"));
            }
        }
        Ok(Self {
            root: SchemaNode::Object { fields: built },
        })
    }

    /// Field names at the root, when the root is an object.
    #[must_use]
    pub fn field_names(&self) -> Vec<&str> {
        match &self.root {
            SchemaNode::Object { fields } => {
                fields.iter().map(|field| field.name.as_str()).collect()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildError, ConfigError, RawTable, RawValue, Schema};

    #[test]
    fn config_error_becomes_a_build_error() {
        let error = BuildError::from(ConfigError::new("missing project"));
        assert_eq!(error.message(), "missing project");
    }

    #[test]
    fn schema_object_rejects_a_repeated_or_unknown_required_name() {
        let string = Schema::string();
        assert!(Schema::object(vec![("project".to_owned(), string.clone())], &["zone"]).is_err());
        assert!(
            Schema::object(
                vec![
                    ("project".to_owned(), string.clone()),
                    ("project".to_owned(), string),
                ],
                &[],
            )
            .is_err()
        );
        let schema = Schema::object(vec![("project".to_owned(), Schema::string())], &["project"])
            .expect("schema");
        assert_eq!(schema.field_names(), ["project"]);
    }

    #[test]
    fn raw_table_keeps_insertion_order_by_key() {
        let mut table = RawTable::new();
        table.insert("b", RawValue::Integer(1));
        table.insert("a", RawValue::Boolean(true));
        assert!(matches!(table.get("a"), Some(RawValue::Boolean(true))));
        assert!(table.get("missing").is_none());
        let keys: Vec<_> = table.keys().collect();
        assert_eq!(keys, ["a", "b"]);
        assert!(table.iter().any(|(key, _)| key == "a"));
    }

    #[test]
    fn config_error_keeps_the_field_path() {
        let error = ConfigError::new("required").with_path("options.project");
        assert_eq!(error.path(), "options.project");
        assert_eq!(error.message(), "required");
        assert_eq!(error.to_string(), "options.project: required");
        let build = BuildError::from(error);
        assert_eq!(build.message(), "options.project: required");
    }
}
