//! List visibility, disabled matching, and runtime retirement.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_config::CatalogSettings;

use crate::ModelCatalog;
use crate::parse::ResolvedEntry;

/// How long a vendor-reported retirement stays out of lists.
pub const RETIRED_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Names a provider model answers to for pattern matching.
#[must_use]
pub fn model_names(provider: &str, model: &str) -> Vec<String> {
    let model = model.trim();
    let mut out = vec![model.to_owned()];
    if !provider.is_empty() {
        out.push(format!("{provider}/{model}"));
    }
    if let Some(tail) = model.rsplit('/').next()
        && tail != model
    {
        out.push(tail.to_owned());
    }
    out
}

/// Names a route model and hop answer to for disabled matching.
#[must_use]
pub fn hop_names(route: &str, asked: &str, provider: &str, wire_model: &str) -> Vec<String> {
    let mut out = model_names(provider, wire_model);
    out.extend(model_names(provider, route));
    if asked != route {
        out.extend(model_names(provider, asked));
        out.push(asked.to_owned());
    }
    out.push(route.to_owned());
    out.sort();
    out.dedup();
    out
}

/// Case-insensitive glob: `*` is any run including `/`, `?` is one byte.
#[must_use]
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains(['*', '?']) {
        return pattern.eq_ignore_ascii_case(value);
    }
    let pattern = pattern.to_ascii_lowercase();
    let value = value.to_ascii_lowercase();
    let (pattern, value) = (pattern.as_bytes(), value.as_bytes());
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for &token in pattern {
        let mut current = vec![false; value.len() + 1];
        if token == b'*' {
            current[0] = previous[0];
            for index in 1..=value.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=value.len() {
                current[index] =
                    previous[index - 1] && (token == b'?' || token == value[index - 1]);
            }
        }
        previous = current;
    }
    previous[value.len()]
}

/// Returns the pattern that matched, if any.
#[must_use]
pub fn match_patterns<'a>(patterns: &'a [String], names: &[String]) -> Option<&'a str> {
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        for name in names {
            if name.is_empty() {
                continue;
            }
            if glob_match(trimmed, name) {
                return Some(trimmed);
            }
        }
    }
    None
}

/// Shared catalog visibility state for lists and routing.
#[derive(Clone)]
pub struct CatalogVisibility {
    settings: Arc<RwLock<CatalogSettings>>,
    runtime: Arc<RwLock<RuntimeRetired>>,
}

impl CatalogVisibility {
    /// Builds visibility from settings and a data root for runtime retirement.
    #[must_use]
    pub fn open(settings: CatalogSettings, data_root: &Path) -> Self {
        Self {
            settings: Arc::new(RwLock::new(settings)),
            runtime: Arc::new(RwLock::new(RuntimeRetired::load(data_root))),
        }
    }

    /// Replaces the in-memory settings snapshot.
    pub fn replace_settings(&self, settings: CatalogSettings) {
        if let Ok(mut guard) = self.settings.write() {
            *guard = settings;
        }
    }

    /// Settings snapshot used by this visibility handle.
    #[must_use]
    pub fn settings(&self) -> CatalogSettings {
        self.settings
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Reloads runtime retirement from disk.
    pub fn reload_runtime(&self) {
        if let Ok(mut guard) = self.runtime.write() {
            *guard = RuntimeRetired::load(&guard.root);
        }
    }

    /// Records that a provider's wire model was retired by the vendor.
    pub fn retire(&self, provider: &str, model: &str) {
        if provider.is_empty() || model.is_empty() {
            return;
        }
        let key = format!("{provider}/{model}");
        if let Ok(mut guard) = self.runtime.write() {
            guard.retire(key);
        }
    }

    /// Clears runtime retirement after a successful answer.
    pub fn unretire(&self, provider: &str, model: &str) {
        let key = format!("{provider}/{model}");
        if let Ok(mut guard) = self.runtime.write() {
            guard.unretire(&key);
        }
    }

    /// Whether the user disabled this hop.
    #[must_use]
    pub fn hop_disabled(&self, route: &str, asked: &str, provider: &str, wire_model: &str) -> bool {
        let names: Vec<_> = hop_names(route, asked, provider, wire_model);
        self.id_disabled(&names)
    }

    /// Whether a client-facing model id is disabled.
    #[must_use]
    pub fn id_disabled(&self, names: &[String]) -> bool {
        let settings = self.settings();
        match_patterns(&settings.disabled, names).is_some()
    }

    /// Matching disabled pattern for an id, when disabled.
    #[must_use]
    pub fn disabled_pattern(&self, id: &str) -> Option<String> {
        let settings = self.settings();
        let names = model_names("", id);
        let names: Vec<_> = names.into_iter().chain([id.to_owned()]).collect();
        match_patterns(&settings.disabled, &names).map(str::to_owned)
    }

    /// Whether a route model should be omitted from model lists.
    #[must_use]
    pub fn hide_route_from_lists(&self, route: &str, catalog: &ModelCatalog) -> bool {
        if self.id_disabled(&[route.to_owned()]) {
            return true;
        }
        let Some(entry) = catalog.resolve(route) else {
            return self.forced_retired(route);
        };
        self.hide_entry_from_lists(route, &entry)
    }

    fn hide_entry_from_lists(&self, route: &str, entry: &ResolvedEntry) -> bool {
        if entry.retired || entry.deprecated {
            return true;
        }
        if self.forced_retired(route) {
            return true;
        }
        if !entry.provider_id.is_empty() {
            let key = format!("{}/{}", entry.provider_id, entry.model_id);
            if self.runtime_retired(&key) {
                return true;
            }
        }
        self.runtime_retired(route)
    }

    fn forced_retired(&self, id: &str) -> bool {
        let settings = self.settings();
        let names: Vec<_> = model_names("", id);
        match_patterns(&settings.retired, &names).is_some()
    }

    fn runtime_retired(&self, key: &str) -> bool {
        self.runtime
            .read()
            .ok()
            .is_some_and(|guard| guard.active(key))
    }

    /// User-facing disabled error text.
    #[must_use]
    pub fn disabled_message(&self, id: &str, pattern: &str) -> String {
        format!(
            "model {id} is disabled in hofund (it matches {pattern:?} in [catalog].disabled); enable it with `hofund models enable {pattern}` or remove it from [catalog].disabled. Only requests through hofund are refused"
        )
    }
}

/// On-disk runtime retirements keyed by `provider/model`.
#[derive(Clone, Debug)]
struct RuntimeRetired {
    root: PathBuf,
    at: BTreeMap<String, u64>,
}

impl RuntimeRetired {
    fn load(data_root: &Path) -> Self {
        let path = data_root.join("catalog/retired.json");
        let at = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            root: data_root.to_path_buf(),
            at,
        }
    }

    fn path(&self) -> PathBuf {
        self.root.join("catalog/retired.json")
    }

    fn retire(&mut self, key: String) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        self.prune(now);
        self.at.insert(key, now);
        let _ = self.save();
    }

    fn unretire(&mut self, key: &str) {
        if self.at.remove(key).is_some() {
            let _ = self.save();
        }
    }

    fn active(&self, key: &str) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        self.at
            .get(key)
            .is_some_and(|at| now.saturating_sub(*at) < RETIRED_FOR.as_secs())
    }

    fn prune(&mut self, now: u64) {
        self.at
            .retain(|_, at| now.saturating_sub(*at) < RETIRED_FOR.as_secs());
    }

    fn save(&self) -> std::io::Result<()> {
        let path = self.path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut at = self.at.clone();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        at.retain(|_, stamp| now.saturating_sub(*stamp) < RETIRED_FOR.as_secs());
        let bytes = serde_json::to_vec_pretty(&at)?;
        std::fs::write(path, bytes)
    }
}

/// Returns route model ids that should appear on model list surfaces.
#[must_use]
pub fn listable_route_models(
    routes: impl IntoIterator<Item = String>,
    catalog: Option<&ModelCatalog>,
    visibility: Option<&CatalogVisibility>,
) -> Vec<String> {
    routes
        .into_iter()
        .filter(|route| {
            visibility.is_none_or(|policy| {
                catalog.is_none_or(|catalog| !policy.hide_route_from_lists(route, catalog))
            })
        })
        .collect()
}

/// Normalizes a disabled or retired pattern list for config storage.
#[must_use]
pub fn normalize_patterns(list: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for pattern in list {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            continue;
        }
        if out
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(trimmed))
        {
            continue;
        }
        out.push(trimmed.to_owned());
    }
    out
}

/// Applies catalog lifecycle flags from config overrides.
pub fn apply_override_status(entry: &mut ResolvedEntry, patch: &hofund_config::CatalogOverride) {
    if let Some(retired) = patch.retired {
        entry.retired = retired;
    }
    if let Some(deprecated) = patch.deprecated {
        entry.deprecated = deprecated;
    }
}
