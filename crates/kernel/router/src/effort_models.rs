//! Model ids that represent one model at different reasoning efforts.

use std::collections::BTreeSet;

use hofund_ir::Effort;

/// One family id and the concrete ids a relay lists for its effort levels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffortModels {
    family: String,
    default: String,
    by_level: Vec<(String, String)>,
    members: BTreeSet<String>,
}

impl EffortModels {
    /// Builds a resolver. `by_level` is ordered weakest to strongest.
    #[must_use]
    pub fn new(
        family: impl Into<String>,
        default: impl Into<String>,
        by_level: Vec<(String, String)>,
    ) -> Self {
        let family = family.into();
        let default = default.into();
        let members = std::iter::once(family.clone())
            .chain(std::iter::once(default.clone()))
            .chain(by_level.iter().map(|(_, id)| id.clone()))
            .collect();
        Self {
            family,
            default,
            by_level,
            members,
        }
    }

    /// The id clients use for the whole family.
    #[must_use]
    pub fn family(&self) -> &str {
        &self.family
    }

    /// The id sent when no effort was requested.
    #[must_use]
    pub fn default_id(&self) -> &str {
        &self.default
    }

    /// Concrete levels, weakest first.
    #[must_use]
    pub fn levels(&self) -> Vec<&str> {
        self.by_level
            .iter()
            .map(|(level, _)| level.as_str())
            .collect()
    }

    /// Whether `model` is the family id or one of its concrete member ids.
    #[must_use]
    pub fn contains(&self, model: &str) -> bool {
        self.members.contains(model)
    }

    /// Family and concrete member ids accepted for compatibility.
    pub fn member_ids(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(String::as_str)
    }

    /// Selects the concrete id for `model` at `effort`.
    ///
    /// A concrete member id is returned unchanged. For the family id, no
    /// requested effort uses the default; otherwise the nearest level wins,
    /// with an equal-distance tie going to the stronger level.
    #[must_use]
    pub fn at(&self, model: &str, effort: Option<Effort>) -> String {
        if model != self.family {
            return model.to_owned();
        }
        let Some(effort) = effort else {
            return self.default.clone();
        };
        let wanted = rank(effort);
        self.by_level
            .iter()
            .filter_map(|(level, id)| Effort::from_label(level).map(|level| (rank(level), id)))
            .min_by_key(|(level, _)| (level.abs_diff(wanted), std::cmp::Reverse(*level)))
            .map_or_else(|| self.default.clone(), |(_, id)| id.clone())
    }
}

fn rank(effort: Effort) -> u8 {
    match effort {
        Effort::None => 0,
        Effort::Minimal => 1,
        Effort::Low => 2,
        Effort::Medium => 3,
        Effort::High => 4,
        Effort::Xhigh => 5,
        Effort::Max => 6,
    }
}
